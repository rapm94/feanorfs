use crate::workspace_read::{WalkCursor, WorkspaceReadRoot};
use ignore::WalkBuilder;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;
use unicode_normalization::UnicodeNormalization;

pub const DEFAULT_IGNORES: &[&str] = &[
    "target/",
    "node_modules/",
    ".DS_Store",
    "*.swp",
    "*~",
    ".venv/",
    "__pycache__/",
    "dist/",
    "build/",
    ".next/",
    ".cache/",
];

pub(super) const CACHEDIR_TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n";

/// Builds the ignore-policy matcher applied by the workspace walker: the
/// frozen `DEFAULT_IGNORES` plus the workspace's custom rules (explicit
/// policy argument or the private global `ignore` file).
///
/// The scanner reuses this exact matcher so paths skipped by policy are
/// recognized as skipped rather than mistaken for local deletions.
pub(super) fn build_ignore_matcher(
    base_path: &Path,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
) -> Option<ignore::gitignore::Gitignore> {
    if no_default_ignores {
        return None;
    }
    let mut patterns = ignore::gitignore::GitignoreBuilder::new(base_path);
    for pattern in DEFAULT_IGNORES {
        let _ = patterns.add_line(None, pattern);
    }
    let disk_policy;
    let content = match ignore_policy {
        Some(content) => Some(content),
        None => {
            disk_policy = crate::workspace_layout::workspace_state_path(base_path)
                .ok()
                .and_then(|state| fs::read_to_string(state.join("ignore")).ok());
            disk_policy.as_deref()
        }
    };
    if let Some(content) = content {
        for line in content.lines().map(str::trim) {
            if !line.is_empty() && !line.starts_with('#') {
                let _ = patterns.add_line(None, line);
            }
        }
    }
    patterns.build().ok()
}

/// The scanner's skip policy for filtering filesystem events, so builds,
/// installs, and agents working in linked Git worktrees do not start sync
/// passes. Never broader than the scan.
pub struct IgnoreRules {
    base: PathBuf,
    file: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    matcher: Option<ignore::gitignore::Gitignore>,
    read_root: Option<WorkspaceReadRoot>,
    /// Skipped subtree that matched last; its marker is checked again for
    /// every event, but bursts stay inside one subtree.
    skipped: Option<PathBuf>,
}

impl IgnoreRules {
    #[must_use]
    pub fn load(base: &Path) -> Self {
        let file = crate::workspace_layout::workspace_state_path(base)
            .ok()
            .map(|state| state.join("ignore"));
        Self {
            stamp: ignore_stamp(file.as_deref()),
            matcher: build_ignore_matcher(base, false, None),
            read_root: WorkspaceReadRoot::open(base).ok(),
            skipped: None,
            base: base.to_path_buf(),
            file,
        }
    }

    /// Reloads rules changed by `feanorfs ignore`; true when they changed.
    pub fn refresh(&mut self) -> bool {
        let stamp = ignore_stamp(self.file.as_deref());
        if stamp == self.stamp {
            return false;
        }
        self.stamp = stamp;
        self.matcher = build_ignore_matcher(&self.base, false, None);
        true
    }

    /// True when the scanner never reports `path` or anything below it. Events
    /// do not say whether a path is a directory, so both readings must agree.
    #[must_use]
    pub fn ignores(&mut self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.base) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return false;
        }
        let by_rule = self.matcher.as_ref().is_some_and(|matcher| {
            [false, true].into_iter().all(|is_dir| {
                matcher
                    .matched_path_or_any_parents(relative, is_dir)
                    .is_ignore()
            })
        });
        by_rule || self.inside_skipped_subtree(relative)
    }

    fn inside_skipped_subtree(&mut self, relative: &Path) -> bool {
        let Some(read_root) = &self.read_root else {
            return false;
        };
        let mut cursor = WalkCursor::new(read_root.clone());
        if self.skipped.as_deref().is_some_and(|directory| {
            relative != directory
                && relative.starts_with(directory)
                && is_skipped_subtree(&mut cursor, directory)
        }) {
            return true;
        }
        // Outermost, so one match covers everything below it.
        self.skipped = nested_ancestors(relative)
            .filter(|directory| is_skipped_subtree(&mut cursor, directory))
            .last()
            .map(Path::to_path_buf);
        self.skipped.is_some()
    }
}

fn ignore_stamp(file: Option<&Path>) -> Option<(SystemTime, u64)> {
    let metadata = fs::metadata(file?).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// True when `relative` lies beneath a skipped subtree (valid `CACHEDIR.TAG`
/// or linked Git worktree), matching the walker's pruning rule for descendants.
pub(super) fn path_under_skipped_subtree(cursor: &mut WalkCursor, relative: &Path) -> bool {
    nested_ancestors(relative).any(|directory| is_skipped_subtree(cursor, directory))
}

/// Directories above `relative`, deepest first, without the workspace root.
fn nested_ancestors(relative: &Path) -> impl Iterator<Item = &Path> {
    relative
        .ancestors()
        .skip(1)
        .filter(|directory| !directory.as_os_str().is_empty())
}

/// Nested directories the walker prunes by structural marker rather than name.
/// The workspace root is never passed here: pruning it would hide every file.
fn is_skipped_subtree(cursor: &mut WalkCursor, directory: &Path) -> bool {
    has_valid_cachedir_tag(cursor, directory) || is_linked_git_worktree(cursor, directory)
}

/// A linked Git worktree (`git worktree add`, agent tools) has a regular `.git`
/// file naming `<repo>/.git/worktrees/<name>`. It is a disposable duplicate
/// checkout. Submodule `.git` files point at `.git/modules/` and stay synced.
fn is_linked_git_worktree(cursor: &mut WalkCursor, directory: &Path) -> bool {
    let Ok(file) = cursor.open_regular(&directory.join(".git")) else {
        return false;
    };
    let mut content = String::new();
    if file.take(4096).read_to_string(&mut content).is_err() {
        return false;
    }
    content
        .strip_prefix("gitdir:")
        .and_then(|gitdir| Path::new(gitdir.trim()).parent())
        .and_then(Path::file_name)
        .is_some_and(|name| name == "worktrees")
}

fn has_valid_cachedir_tag(cursor: &mut WalkCursor, directory: &Path) -> bool {
    let tag = directory.join("CACHEDIR.TAG");
    let Ok(mut file) = cursor.open_regular(&tag) else {
        return false;
    };
    let mut prefix = [0_u8; CACHEDIR_TAG_SIGNATURE.len()];
    file.read_exact(&mut prefix).is_ok() && prefix == CACHEDIR_TAG_SIGNATURE
}

#[must_use]
pub fn normalize_path_nfc(path: &str) -> String {
    feanorfs_common::normalize_path(&path.nfc().collect::<String>())
}

/// Converts a platform-native relative path into the portable wire spelling.
/// On Unix, a backslash is a filename byte rather than a separator and must not
/// be rewritten into an aliased path.
pub(crate) fn portable_rel_path(path: &str) -> Option<String> {
    #[cfg(not(windows))]
    if path.contains('\\') {
        return None;
    }
    let normalized = if path.is_ascii() {
        feanorfs_common::normalize_path(path)
    } else {
        normalize_path_nfc(path)
    };
    feanorfs_common::is_safe_rel_path(&normalized).then_some(normalized)
}

#[cfg(unix)]
pub(super) fn portable_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    u32::from(metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(super) fn portable_mode(_metadata: &fs::Metadata) -> u32 {
    0
}

pub fn build_workspace_walker(base_path: &Path, no_default_ignores: bool) -> WalkBuilder {
    build_workspace_walker_with_ignore_policy(base_path, no_default_ignores, None)
}

/// Build the workspace walker with optional in-memory workspace ignore rules.
///
/// Join preflight uses the encrypted sender policy before any destination file
/// is written. `None` retains the ordinary behavior of reading the policy from
/// disk; `Some("")` explicitly applies no custom rules.
pub fn build_workspace_walker_with_ignore_policy(
    base_path: &Path,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
) -> WalkBuilder {
    let cursor = WorkspaceReadRoot::open(base_path)
        .ok()
        .map(SharedCursor::new);
    build_workspace_walker_inner(base_path, no_default_ignores, ignore_policy, cursor)
}

/// One walk's [`WalkCursor`], shared by the walker's pruning filter and the
/// scanner so each directory is opened once.
#[derive(Clone)]
pub(super) struct SharedCursor(Arc<Mutex<WalkCursor>>);

impl SharedCursor {
    pub(super) fn new(root: WorkspaceReadRoot) -> Self {
        Self(Arc::new(Mutex::new(WalkCursor::new(root))))
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, WalkCursor> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) fn build_workspace_walker_with_cursor(
    base_path: &Path,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
    cursor: SharedCursor,
) -> WalkBuilder {
    build_workspace_walker_inner(base_path, no_default_ignores, ignore_policy, Some(cursor))
}

fn build_workspace_walker_inner(
    base_path: &Path,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
    cursor: Option<SharedCursor>,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(base_path);
    builder
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .follow_links(false);

    let ignores = build_ignore_matcher(base_path, no_default_ignores, ignore_policy);

    let base = base_path.to_path_buf();
    builder.filter_entry(move |entry| {
        let Some(file_type) = entry.file_type() else {
            return true;
        };
        let Ok(relative) = entry.path().strip_prefix(&base) else {
            return true;
        };
        if is_always_excluded(relative) {
            return false;
        }
        if let (Some(ignores), Some(path)) = (&ignores, relative.to_str()) {
            if ignores.matched(path, file_type.is_dir()).is_ignore() {
                return false;
            }
        }
        // Last, because it is the only check that touches the filesystem.
        !(file_type.is_dir()
            && !relative.as_os_str().is_empty()
            && cursor
                .as_ref()
                .is_some_and(|cursor| is_skipped_subtree(&mut cursor.lock(), relative)))
    });
    builder
}

/// Metadata and VCS paths FeanorFS never transports, regardless of policy.
#[must_use]
pub fn is_always_excluded(relative: &Path) -> bool {
    let first = relative
        .components()
        .next()
        .and_then(|part| part.as_os_str().to_str());
    matches!(first, Some(".git" | ".jj" | ".feanorfs"))
        || relative == Path::new(".feanorfsignore")
        || relative
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".feanorfs-tmp-"))
}
