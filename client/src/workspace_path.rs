//! Exact canonical workspace path identity at the client contract boundary.
//!
//! Every workspace path that becomes durable identity — a recent-workspace
//! entry, a supervisor registry member, a runner stop tombstone — must be an
//! exact, valid-UTF-8, canonical path. Lossy conversion (`to_string_lossy`)
//! is only ever allowed for human-facing display labels, never for identity:
//! a lossy-mangled path would silently change the filesystem object a later
//! operation reads, owns, or dispatches to.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

/// Upper bound on the length of a display label, so an adversarial or extreme
/// filename cannot blow up the tray/status projection.
const MAX_DISPLAY_LABEL_CHARS: usize = 64;

/// Why a workspace path cannot become canonical identity.
#[derive(Debug)]
pub enum WorkspacePathError {
    /// `fs::canonicalize` failed (typically: the folder does not exist).
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The path contains bytes that are not valid UTF-8.
    NotUtf8(PathBuf),
}

impl fmt::Display for WorkspacePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspacePathError::Canonicalize { path, source } => {
                write!(
                    f,
                    "canonicalize workspace path {}: {source}",
                    path.display()
                )
            }
            WorkspacePathError::NotUtf8(path) => {
                write!(f, "workspace path is not valid UTF-8: {}", path.display())
            }
        }
    }
}

impl std::error::Error for WorkspacePathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkspacePathError::Canonicalize { source, .. } => Some(source),
            WorkspacePathError::NotUtf8(_) => None,
        }
    }
}

/// An exact canonical workspace path.
///
/// Construction canonicalizes and requires valid UTF-8; a path that cannot be
/// represented exactly is rejected with [`WorkspacePathError`] BEFORE it is
/// persisted or dispatched. Values read back from a registry are wrapped with
/// [`Self::from_exact_string`] and used as exact identity — they are never
/// re-canonicalized (the folder may legitimately be gone) and never converted
/// lossily.
///
/// Serde is transparent: the durable JSON representation is the plain path
/// string, byte-for-byte identical to the pre-L5 format.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalWorkspacePath(String);

impl CanonicalWorkspacePath {
    /// Canonicalize `workspace` and require the result to be valid UTF-8.
    ///
    /// Fails with a typed error when the folder cannot be resolved or when
    /// the canonical path is not valid UTF-8; the caller must abort the
    /// persistence/dispatch operation instead of falling back to a mangled
    /// path.
    pub fn canonicalize(workspace: &Path) -> Result<Self, WorkspacePathError> {
        let canonical =
            workspace
                .canonicalize()
                .map_err(|source| WorkspacePathError::Canonicalize {
                    path: workspace.to_path_buf(),
                    source,
                })?;
        Self::from_os(canonical)
    }

    /// Canonicalize, keeping the raw path when the folder cannot be resolved.
    ///
    /// This preserves the legacy "use the path as given when it no longer
    /// canonicalizes" behavior of the recent-workspace registry (a deleted
    /// folder must still be removable by its stored path), while still
    /// rejecting non-UTF-8 paths with a typed error instead of lossy-mangling
    /// them.
    pub fn canonicalize_keep_raw(workspace: &Path) -> Result<Self, WorkspacePathError> {
        match workspace.canonicalize() {
            Ok(canonical) => Self::from_os(canonical),
            Err(_) => Self::from_os(workspace.to_path_buf()),
        }
    }

    /// Wrap an already-exact UTF-8 path string (for example one read back
    /// from a registry file). No re-canonicalization: the string is used as
    /// exact identity even when the folder no longer exists.
    pub fn from_exact_string(path: String) -> Self {
        Self(path)
    }

    fn from_os(path: PathBuf) -> Result<Self, WorkspacePathError> {
        match path.into_os_string().into_string() {
            Ok(path) => Ok(Self(path)),
            Err(os) => Err(WorkspacePathError::NotUtf8(PathBuf::from(os))),
        }
    }

    /// The exact path as `&str`. Never lossy.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The exact path as `&Path`.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// Consume and return the exact path string.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Bounded lossy display label for UI rendering ONLY.
    ///
    /// Derives the final path component and truncates it to
    /// `MAX_DISPLAY_LABEL_CHARS` characters; unrepresentable bytes would be
    /// replaced with U+FFFD. This label MUST NOT be used for filesystem
    /// access, registry identity, runner ownership, or adapter output.
    pub fn display_label(&self) -> String {
        let name = self
            .as_path()
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| Cow::Borrowed("workspace"));
        name.chars().take(MAX_DISPLAY_LABEL_CHARS).collect()
    }
}

/// Convert an already-exact UTF-8 path string (for example one read back from
/// a registry file). Same contract as [`CanonicalWorkspacePath::from_exact_string`].
///
/// Deliberately infallible: a JSON registry can only contain valid UTF-8, and
/// the value is used as exact identity, never re-canonicalized.
impl From<String> for CanonicalWorkspacePath {
    fn from(path: String) -> Self {
        Self::from_exact_string(path)
    }
}

/// Convert an already-exact UTF-8 path string (for example one read back from
/// a registry file). Same contract as [`CanonicalWorkspacePath::from_exact_string`].
impl From<&str> for CanonicalWorkspacePath {
    fn from(path: &str) -> Self {
        Self::from_exact_string(path.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_rejects_non_utf8_paths_with_typed_error() {
        use std::os::unix::ffi::OsStringExt as _;
        let directory = tempfile::tempdir().unwrap();
        let raw = std::ffi::OsString::from_vec(vec![b'w', 0x81]);
        let non_utf8 = directory.path().join(&raw);
        // Some filesystems reject the byte sequence outright (macOS returns
        // EILSEQ from mkdir); filesystems that store it (Linux ext4/tmpfs)
        // produce a real canonical path that must then be rejected as
        // non-UTF-8. Either way the path never becomes identity.
        let created = std::fs::create_dir_all(&non_utf8).is_ok();

        let error = CanonicalWorkspacePath::canonicalize(&non_utf8).unwrap_err();

        match error {
            WorkspacePathError::NotUtf8(_) => {
                assert!(
                    created,
                    "an existing non-UTF-8 folder must be rejected as NotUtf8"
                );
                assert!(error.to_string().contains("not valid UTF-8"));
            }
            WorkspacePathError::Canonicalize { .. } => {
                assert!(!created, "a resolvable folder must reach the UTF-8 check");
            }
        }
    }

    #[test]
    fn canonicalize_rejects_missing_folder_before_identity_is_used() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");

        let error = CanonicalWorkspacePath::canonicalize(&missing).unwrap_err();

        assert!(matches!(error, WorkspacePathError::Canonicalize { .. }));
    }

    #[test]
    fn canonicalize_keep_raw_preserves_missing_folder_but_rejects_non_utf8() {
        use std::os::unix::ffi::OsStringExt as _;
        let directory = tempfile::tempdir().unwrap();

        // Missing but UTF-8: kept as the raw exact path (legacy behavior).
        let missing = directory.path().join("missing");
        let kept = CanonicalWorkspacePath::canonicalize_keep_raw(&missing).unwrap();
        assert_eq!(kept.as_str(), missing.to_str().unwrap());

        // Missing and non-UTF-8: still rejected with the typed error, never
        // lossy-mangled. This is the deterministic rejection path on every
        // platform, and it is the path used before an unregister write.
        let raw = std::ffi::OsString::from_vec(vec![b'm', 0x80]);
        let non_utf8 = directory.path().join(&raw);
        let error = CanonicalWorkspacePath::canonicalize_keep_raw(&non_utf8).unwrap_err();
        assert!(matches!(error, WorkspacePathError::NotUtf8(_)));
        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn canonicalize_round_trips_through_exact_string() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = CanonicalWorkspacePath::canonicalize(directory.path()).unwrap();
        // Canonicalization resolves symlinks (e.g. /var -> /private/var on
        // macOS); compare against the canonical form of the temp dir.
        let expected = directory.path().canonicalize().unwrap();

        let restored = CanonicalWorkspacePath::from_exact_string(canonical.as_str().to_owned());

        assert_eq!(restored, canonical);
        assert_eq!(canonical.as_path(), expected);
        assert_eq!(restored.as_path(), expected);
    }

    #[test]
    fn display_label_is_bounded_and_ui_only() {
        let long = format!("folder-{}", "x".repeat(200));
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join(&long);
        std::fs::create_dir_all(&folder).unwrap();
        let canonical = CanonicalWorkspacePath::canonicalize(&folder).unwrap();

        let label = canonical.display_label();

        assert_eq!(label.chars().count(), MAX_DISPLAY_LABEL_CHARS);
        // The exact path identity is untouched by the label.
        assert!(canonical.as_str().ends_with(&long));
    }

    #[test]
    fn display_label_falls_back_for_paths_without_a_file_name() {
        let root = CanonicalWorkspacePath::from_exact_string("/".to_string());
        assert_eq!(root.display_label(), "workspace");
    }

    #[test]
    fn transparent_serde_matches_plain_string_encoding() {
        let path = CanonicalWorkspacePath::from_exact_string("/some/workspace".to_string());
        assert_eq!(
            serde_json::to_string(&path).unwrap(),
            r#""/some/workspace""#
        );
        let decoded: CanonicalWorkspacePath = serde_json::from_str(r#""/some/workspace""#).unwrap();
        assert_eq!(decoded, path);
    }
}
