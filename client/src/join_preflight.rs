use anyhow::{bail, Context as _, Result};
use feanorfs_common::FileState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{ApiClient, ClientDb, Config, SyncCtx, WorkspaceInvite};

pub const MAX_PREFLIGHT_EXAMPLES: usize = 5;
pub const MAX_IGNORE_POLICY_BYTES: usize = 4 * 1024;
pub const LARGE_FILE_NOTICE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct JoinPathGroup {
    pub count: usize,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinPreflight {
    pub local_only: JoinPathGroup,
    pub remote_only: JoinPathGroup,
    pub same: JoinPathGroup,
    pub conflicts: JoinPathGroup,
    pub large_files: JoinPathGroup,
    pub ignore_policy_known: bool,
    pub ignore_policy_differs: bool,
}

impl JoinPreflight {
    #[must_use]
    pub fn destination_has_files(&self) -> bool {
        self.local_only.count + self.same.count + self.conflicts.count > 0
    }

    #[must_use]
    pub fn needs_confirmation(&self) -> bool {
        self.local_only.count > 0 || self.conflicts.count > 0 || self.ignore_policy_differs
    }
}

struct ScratchState(PathBuf);

impl ScratchState {
    fn create() -> Result<Self> {
        for _ in 0..8 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).context("generate join preview scratch name")?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let path = std::env::temp_dir().join(format!("feanorfs-join-preview-{suffix}"));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("create private join preview scratch space")
                }
            }
        }
        bail!("could not allocate unique join preview scratch space")
    }
}

impl Drop for ScratchState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn normalize_ignore_policy(policy: &str) -> Result<String> {
    if policy.len() > MAX_IGNORE_POLICY_BYTES {
        bail!(
            ".feanorfsignore is too large for secure pairing ({} bytes; maximum {MAX_IGNORE_POLICY_BYTES})",
            policy.len()
        );
    }
    if policy.contains('\0') {
        bail!(".feanorfsignore contains an unsupported NUL character");
    }
    Ok(policy.replace("\r\n", "\n").replace('\r', "\n"))
}

pub fn read_ignore_policy(workspace: &Path) -> Result<String> {
    match std::fs::read_to_string(workspace.join(".feanorfsignore")) {
        Ok(policy) => normalize_ignore_policy(&policy),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).context("read destination .feanorfsignore"),
    }
}

pub async fn apply_invited_ignore_policy(workspace: &Path, policy: Option<&str>) -> Result<()> {
    let Some(policy) = policy else {
        return Ok(());
    };
    let policy = normalize_ignore_policy(policy)?;
    let path = workspace.join(".feanorfsignore");
    if policy.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("apply mirror ignore policy"),
        }
    } else {
        crate::fs_util::atomic_write(workspace, ".feanorfsignore", policy.as_bytes())
            .await
            .context("apply mirror ignore policy")?;
    }
    Ok(())
}

pub async fn preview_join(workspace: &Path, invite: &WorkspaceInvite) -> Result<JoinPreflight> {
    let local_policy = read_ignore_policy(workspace)?;
    let invited_policy = invite
        .ignore_policy
        .as_deref()
        .map(normalize_ignore_policy)
        .transpose()?;
    let effective_policy = invited_policy.as_deref().unwrap_or(&local_policy);
    let ignore_policy_differs = invited_policy
        .as_deref()
        .is_some_and(|policy| policy != local_policy);

    let large_files = large_file_paths(workspace, effective_policy);

    let scratch = ScratchState::create()?;
    let db = ClientDb::new(scratch.0.join("state")).await?;
    let mut local = feanorfs_agent_core::local::scan_local_directory_with_policy(
        workspace,
        &db,
        Some(&invite.encryption_key),
        false,
        Some(effective_policy),
    )
    .await?;
    if ignore_policy_differs {
        // The accepted policy is applied before the real scan. Do not report
        // the old policy file as a content conflict that cannot actually occur.
        local.remove(".feanorfsignore");
    }

    let config = Config {
        server_url: invite.server_url.clone(),
        workspace_id: invite.workspace_id.clone(),
        encryption_password: Some(invite.encryption_key.clone()),
        server_password: invite.server_token.clone(),
        tls_ca_pem: invite.tls_ca_pem.clone(),
        format_version: 3,
        hub_local: invite.hub_local,
        relay: invite.relay.clone(),
    };
    let api = ApiClient::from_config(&scratch.0, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, workspace, &config)?;
    let remote = crate::conflicts::load_server_view(&ctx).await?;

    let mut preview = classify(
        &local,
        &remote,
        invited_policy.is_some(),
        ignore_policy_differs,
    );
    preview.large_files = group(large_files);
    Ok(preview)
}

fn large_file_paths(workspace: &Path, ignore_policy: &str) -> Vec<String> {
    let mut paths = feanorfs_agent_core::local::build_workspace_walker_with_ignore_policy(
        workspace,
        false,
        Some(ignore_policy),
    )
    .build()
    .filter_map(std::result::Result::ok)
    .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
    .filter_map(|entry| {
        let metadata = entry.metadata().ok()?;
        if metadata.len() <= LARGE_FILE_NOTICE_BYTES {
            return None;
        }
        let relative = entry.path().strip_prefix(workspace).ok()?.to_str()?;
        let path = feanorfs_common::normalize_path(relative);
        feanorfs_common::is_safe_rel_path(&path).then_some(path)
    })
    .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    paths
}

fn classify(
    local: &std::collections::HashMap<String, FileState>,
    remote: &std::collections::HashMap<String, FileState>,
    ignore_policy_known: bool,
    ignore_policy_differs: bool,
) -> JoinPreflight {
    let local = live_files(local);
    let remote = live_files(remote);
    let mut local_only = Vec::new();
    let mut remote_only = Vec::new();
    let mut same = Vec::new();
    let mut conflicts = Vec::new();

    for (path, local_file) in &local {
        match remote.get(path) {
            None => local_only.push(path.clone()),
            Some(remote_file)
                if local_file.hash == remote_file.hash && local_file.mode == remote_file.mode =>
            {
                same.push(path.clone());
            }
            Some(_) => conflicts.push(path.clone()),
        }
    }
    remote_only.extend(
        remote
            .keys()
            .filter(|path| !local.contains_key(*path))
            .cloned(),
    );

    JoinPreflight {
        local_only: group(local_only),
        remote_only: group(remote_only),
        same: group(same),
        conflicts: group(conflicts),
        large_files: JoinPathGroup::default(),
        ignore_policy_known,
        ignore_policy_differs,
    }
}

fn live_files(files: &std::collections::HashMap<String, FileState>) -> BTreeMap<String, FileState> {
    files
        .iter()
        .filter(|(_, file)| !file.deleted)
        .map(|(path, file)| (path.clone(), file.clone()))
        .collect()
}

fn group(mut paths: Vec<String>) -> JoinPathGroup {
    paths.sort_unstable();
    paths.dedup();
    JoinPathGroup {
        count: paths.len(),
        examples: paths.into_iter().take(MAX_PREFLIGHT_EXAMPLES).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, hash: &str) -> FileState {
        FileState {
            path: path.into(),
            hash: hash.into(),
            size: 1,
            mtime: 0,
            deleted: false,
            mode: 0,
        }
    }

    #[test]
    fn classification_is_bounded_and_deterministic() {
        let local = (0..8)
            .map(|index| {
                let path = format!("local-{index}.txt");
                (path.clone(), file(&path, "local"))
            })
            .chain([(String::from("same"), file("same", "same"))])
            .chain([(String::from("conflict"), file("conflict", "ours"))])
            .collect();
        let remote = [
            (String::from("same"), file("same", "same")),
            (String::from("conflict"), file("conflict", "theirs")),
            (String::from("remote"), file("remote", "remote")),
        ]
        .into_iter()
        .collect();

        let preview = classify(&local, &remote, true, false);
        assert_eq!(preview.local_only.count, 8);
        assert_eq!(preview.local_only.examples.len(), MAX_PREFLIGHT_EXAMPLES);
        assert_eq!(preview.remote_only.count, 1);
        assert_eq!(preview.same.count, 1);
        assert_eq!(preview.conflicts.count, 1);
        assert!(preview.needs_confirmation());
    }

    #[test]
    fn ignore_policy_normalizes_line_endings_and_rejects_unsafe_input() {
        assert_eq!(normalize_ignore_policy("target/\r\n").unwrap(), "target/\n");
        assert!(normalize_ignore_policy("bad\0rule").is_err());
        assert!(normalize_ignore_policy(&"x".repeat(MAX_IGNORE_POLICY_BYTES + 1)).is_err());
    }
}
