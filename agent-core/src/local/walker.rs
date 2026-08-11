use crate::workspace_read::WorkspaceReadRoot;
use ignore::WalkBuilder;
use std::fs;
use std::io::Read;
use std::path::Path;
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

/// True when `relative` lies beneath a directory carrying a valid
/// `CACHEDIR.TAG`, matching the walker's pruning rule for descendants.
pub(super) fn path_under_tagged_directory(read_root: &WorkspaceReadRoot, relative: &Path) -> bool {
    let mut current = relative.parent();
    while let Some(directory) = current {
        if !directory.as_os_str().is_empty() && has_valid_cachedir_tag(read_root, directory) {
            return true;
        }
        current = directory.parent();
    }
    false
}

fn has_valid_cachedir_tag(read_root: &WorkspaceReadRoot, directory: &Path) -> bool {
    let tag = directory.join("CACHEDIR.TAG");
    let Ok(mut file) = read_root.open_regular_path(&tag) else {
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
    let normalized = normalize_path_nfc(path);
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
    let read_root = WorkspaceReadRoot::open(base_path).ok();
    build_workspace_walker_inner(base_path, no_default_ignores, ignore_policy, read_root)
}

pub(super) fn build_workspace_walker_with_read_root(
    base_path: &Path,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
    read_root: WorkspaceReadRoot,
) -> WalkBuilder {
    build_workspace_walker_inner(
        base_path,
        no_default_ignores,
        ignore_policy,
        Some(read_root),
    )
}

fn build_workspace_walker_inner(
    base_path: &Path,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
    read_root: Option<WorkspaceReadRoot>,
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
        if file_type.is_dir()
            && !relative.as_os_str().is_empty()
            && read_root
                .as_ref()
                .is_some_and(|root| has_valid_cachedir_tag(root, relative))
        {
            return false;
        }
        let Some(path) = relative.to_str() else {
            return true;
        };
        if is_always_excluded(relative) {
            return false;
        }
        let Some(ignores) = &ignores else {
            return true;
        };
        !ignores.matched(path, file_type.is_dir()).is_ignore()
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

pub fn collect_symlink_warnings(base_path: &Path) -> Vec<String> {
    let mut paths = build_workspace_walker(base_path, false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_symlink()))
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(base_path)
                .ok()
                .and_then(Path::to_str)
                .and_then(portable_rel_path)
        })
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    paths
}
