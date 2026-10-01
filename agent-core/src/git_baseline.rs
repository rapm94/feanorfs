//! Read-only Git baseline detection.
//!
//! FeanorFS never writes `.git` and never runs Git. It reads only `HEAD` and
//! the one ref `HEAD` names (loose file or `packed-refs`) to label published
//! sync snapshots and to warn when this clone's baseline differs from the
//! baseline the shared work was published on.

use crate::ctx::SyncCtx;
use crate::snapshot::SnapshotEngine;
use anyhow::Result;
use feanorfs_common::git_baseline::{GitBaseline, GitBaselineMismatch};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

const MAX_POINTER_BYTES: u64 = 4096;
const MAX_PACKED_REFS_BYTES: u64 = 64 * 1024 * 1024;
/// Snapshots walked back from the head looking for the latest label.
const BASELINE_SCAN_DEPTH: usize = 32;

fn read_small(path: &Path) -> Option<String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_POINTER_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    Some(text.trim().to_string())
}

/// Locates (`git dir` holding `HEAD`, `common dir` holding refs), following
/// a `.git` file (`gitdir: …`) and a worktree `commondir`.
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = read_small(&dot_git)?;
        let target = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            root.join(target)
        }
    };
    let common_dir = match read_small(&git_dir.join("commondir")) {
        Some(common) if Path::new(&common).is_absolute() => PathBuf::from(common),
        Some(common) => git_dir.join(common),
        None => git_dir.clone(),
    };
    Some((git_dir, common_dir))
}

fn is_safe_ref(name: &str) -> bool {
    name.starts_with("refs/")
        && Path::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn packed_ref(common_dir: &Path, name: &str) -> Option<String> {
    let file = std::fs::File::open(common_dir.join("packed-refs")).ok()?;
    BufReader::new(file.take(MAX_PACKED_REFS_BYTES))
        .lines()
        .map_while(std::result::Result::ok)
        .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
        .find_map(|line| {
            let (id, ref_name) = line.split_once(' ')?;
            (ref_name == name).then(|| id.to_string())
        })
}

/// Reads the baseline of the Git clone rooted at `root`, if any. Unborn
/// branches, missing refs, and non-Git folders yield `None`.
#[must_use]
pub fn read_git_baseline(root: &Path) -> Option<GitBaseline> {
    let (git_dir, common_dir) = git_dirs(root)?;
    let head = read_small(&git_dir.join("HEAD"))?;
    match head.strip_prefix("ref:").map(str::trim) {
        Some(name) => {
            if !is_safe_ref(name) {
                return None;
            }
            let commit = read_small(&git_dir.join(name))
                .or_else(|| read_small(&common_dir.join(name)))
                .or_else(|| packed_ref(&common_dir, name))?;
            GitBaseline::new(&commit, name.strip_prefix("refs/heads/"))
        }
        None => GitBaseline::new(&head, None),
    }
}

/// The newest baseline label within the bounded first-parent history of
/// the workspace head (signal snapshots carry no label and are skipped).
///
/// # Errors
/// Returns an error for unreadable head or snapshot objects.
pub async fn shared_baseline(ctx: &SyncCtx<'_>) -> Result<Option<GitBaseline>> {
    let Some(mut cursor) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Ok(None);
    };
    let engine = SnapshotEngine::new(ctx);
    for _ in 0..BASELINE_SCAN_DEPTH {
        let snapshot = engine.load_snapshot(&cursor).await?;
        if let Some(baseline) = snapshot.message.as_deref().and_then(GitBaseline::parse) {
            return Ok(Some(baseline));
        }
        match snapshot.parents.first() {
            Some(parent) => cursor = parent.clone(),
            None => break,
        }
    }
    Ok(None)
}

/// Compares this clone's baseline with the shared one. Commits decide;
/// branch names are informational.
///
/// # Errors
/// Returns an error for unreadable head or snapshot objects.
pub async fn baseline_mismatch(ctx: &SyncCtx<'_>) -> Result<Option<GitBaselineMismatch>> {
    let Some(local) = read_git_baseline(ctx.base) else {
        return Ok(None);
    };
    Ok(shared_baseline(ctx)
        .await?
        .filter(|shared| shared.commit != local.commit)
        .map(|shared| GitBaselineMismatch { local, shared }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn reads_loose_packed_detached_and_worktree_heads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        assert_eq!(read_git_baseline(root), None);

        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join(".git/refs/heads/main"), &format!("{a}\n"));
        assert_eq!(read_git_baseline(root), GitBaseline::new(&a, Some("main")));

        std::fs::remove_file(root.join(".git/refs/heads/main")).unwrap();
        write(
            &root.join(".git/packed-refs"),
            &format!("# pack-refs with: peeled\n{b} refs/heads/main\n^{a}\n"),
        );
        assert_eq!(read_git_baseline(root), GitBaseline::new(&b, Some("main")));

        write(&root.join(".git/HEAD"), &a);
        assert_eq!(read_git_baseline(root), GitBaseline::new(&a, None));

        write(&root.join(".git/HEAD"), "ref: refs/heads/unborn\n");
        assert_eq!(read_git_baseline(root), None);
        write(&root.join(".git/HEAD"), "ref: refs/../../etc/passwd\n");
        assert_eq!(read_git_baseline(root), None);

        let worktree = root.join("wt");
        write(&worktree.join(".git"), "gitdir: ../.git/worktrees/wt\n");
        write(
            &root.join(".git/worktrees/wt/HEAD"),
            "ref: refs/heads/feature\n",
        );
        write(&root.join(".git/worktrees/wt/commondir"), "../..\n");
        write(&root.join(".git/refs/heads/feature"), &b);
        assert_eq!(
            read_git_baseline(&worktree),
            GitBaseline::new(&b, Some("feature"))
        );
    }
}
