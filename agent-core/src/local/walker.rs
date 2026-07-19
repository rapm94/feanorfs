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

fn has_valid_cachedir_tag(directory: &Path) -> bool {
    let tag = directory.join("CACHEDIR.TAG");
    let Ok(metadata) = fs::symlink_metadata(&tag) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    let Ok(mut file) = fs::File::open(tag) else {
        return false;
    };
    let mut prefix = [0_u8; CACHEDIR_TAG_SIGNATURE.len()];
    file.read_exact(&mut prefix).is_ok() && prefix == CACHEDIR_TAG_SIGNATURE
}

#[must_use]
pub fn normalize_path_nfc(path: &str) -> String {
    feanorfs_common::normalize_path(&path.nfc().collect::<String>())
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

/// Build the workspace walker with an optional in-memory `.feanorfsignore` policy.
///
/// Join preflight uses the encrypted sender policy before any destination file
/// is written. `None` retains the ordinary behavior of reading the policy from
/// disk; `Some("")` explicitly applies no custom rules.
pub fn build_workspace_walker_with_ignore_policy(
    base_path: &Path,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(base_path);
    builder
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .follow_links(false);

    let ignores = if no_default_ignores {
        None
    } else {
        let mut patterns = ignore::gitignore::GitignoreBuilder::new(base_path);
        for pattern in DEFAULT_IGNORES {
            let _ = patterns.add_line(None, pattern);
        }
        let disk_policy;
        let content = match ignore_policy {
            Some(content) => Some(content),
            None => {
                disk_policy = fs::read_to_string(base_path.join(".feanorfsignore")).ok();
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
    };

    let base = base_path.to_path_buf();
    builder.filter_entry(move |entry| {
        let Some(file_type) = entry.file_type() else {
            return true;
        };
        if file_type.is_dir() && entry.path() != base && has_valid_cachedir_tag(entry.path()) {
            return false;
        }
        let Some(ignores) = &ignores else {
            return true;
        };
        let Ok(relative) = entry.path().strip_prefix(&base) else {
            return true;
        };
        let Some(path) = relative.to_str() else {
            return true;
        };
        !ignores.matched(path, file_type.is_dir()).is_ignore()
    });
    builder
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
                .map(normalize_path_nfc)
        })
        .filter(|path| feanorfs_common::is_safe_rel_path(path))
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    paths
}
