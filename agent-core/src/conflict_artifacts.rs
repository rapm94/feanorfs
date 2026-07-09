use crate::api::ApiClient;
use anyhow::Result;
use feanorfs_common::{
    unpack_bytes_with_policy, ConcurrentEdit, ConflictKind, FileState, LegacyPolicy,
};
use std::path::{Path, PathBuf};
use tokio::fs;

/// Prefix for placeholder bytes written when a version cannot be materialized.
pub const SENTINEL_PREFIX: &str = "<feanorfs-sentinel:";

pub const SUFFIX_ORIGINAL: &str = ".original";
pub const SUFFIX_LOCAL: &str = ".local";
pub const SUFFIX_CLOUD: &str = ".cloud";

// Legacy suffixes (read compat)
const SUFFIX_BASE: &str = ".base";
const SUFFIX_OURS: &str = ".ours";
const SUFFIX_THEIRS: &str = ".theirs";

pub fn is_sentinel_content(content: &[u8]) -> bool {
    content.starts_with(SENTINEL_PREFIX.as_bytes())
}

/// Label inside `<feanorfs-sentinel:{label}>\n`, if present.
#[must_use]
pub fn sentinel_label(content: &[u8]) -> Option<&str> {
    if !is_sentinel_content(content) {
        return None;
    }
    let s = std::str::from_utf8(content).ok()?;
    let rest = s.strip_prefix(SENTINEL_PREFIX)?;
    rest.strip_suffix(">\n").or_else(|| rest.strip_suffix('>'))
}

/// Cloud-side deletion artifact (edit/delete or delete/edit conflicts).
#[must_use]
pub fn is_cloud_deleted_sentinel(content: &[u8]) -> bool {
    sentinel_label(content) == Some("deleted")
}

pub fn is_binary_content(content: &[u8]) -> bool {
    content.is_empty() || content.contains(&0)
}

fn sentinel(label: &str) -> Vec<u8> {
    format!("{SENTINEL_PREFIX}{label}>\n").into_bytes()
}

#[must_use]
pub fn artifact_path(conflict_dir: &Path, rel_path: &str, suffix: &str) -> PathBuf {
    conflict_dir.join(format!("{rel_path}{suffix}"))
}

/// Resolve artifact path preferring new suffixes, falling back to legacy.
#[must_use]
pub fn resolve_artifact(conflict_dir: &Path, rel_path: &str, role: ArtifactRole) -> PathBuf {
    let (new_suffix, old_suffix) = match role {
        ArtifactRole::Original => (SUFFIX_ORIGINAL, SUFFIX_BASE),
        ArtifactRole::Local => (SUFFIX_LOCAL, SUFFIX_OURS),
        ArtifactRole::Cloud => (SUFFIX_CLOUD, SUFFIX_THEIRS),
    };
    let new_path = artifact_path(conflict_dir, rel_path, new_suffix);
    if new_path.exists() {
        return new_path;
    }
    artifact_path(conflict_dir, rel_path, old_suffix)
}

#[derive(Debug, Clone, Copy)]
pub enum ArtifactRole {
    Original,
    Local,
    Cloud,
}

pub async fn write_version_file(
    dest: &Path,
    state: Option<&FileState>,
    api: &ApiClient,
    password: &str,
    path: &str,
    policy: LegacyPolicy,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    match state {
        Some(f) if f.deleted => {
            fs::write(dest, sentinel("deleted")).await?;
        }
        Some(f) => match api.download_file(&f.hash).await {
            Ok(bytes) => {
                let computed_hash = feanorfs_common::hash_bytes(&bytes);
                if computed_hash != f.hash {
                    fs::write(
                        dest,
                        sentinel(&format!(
                            "integrity-mismatch expected={} computed={}",
                            f.hash, computed_hash
                        )),
                    )
                    .await?;
                } else {
                    match unpack_bytes_with_policy(&bytes, password, path, policy) {
                        Ok(plain) => fs::write(dest, &plain).await?,
                        Err(e) => {
                            fs::write(dest, sentinel(&format!("decrypt-failed {e}"))).await?;
                        }
                    }
                }
            }
            Err(e) => {
                fs::write(dest, sentinel(&format!("download-failed {e}"))).await?;
            }
        },
        None => {
            fs::write(dest, sentinel("missing")).await?;
        }
    }
    Ok(())
}

pub async fn write_conflict_triple(
    dir: &Path,
    edit: &ConcurrentEdit,
    api: &ApiClient,
    password: &str,
    ours_from: Option<&Path>,
    ours_missing_label: &str,
    policy: LegacyPolicy,
) -> Result<()> {
    let base_dest = artifact_path(dir, &edit.path, SUFFIX_ORIGINAL);
    let ours_dest = artifact_path(dir, &edit.path, SUFFIX_LOCAL);
    let theirs_dest = artifact_path(dir, &edit.path, SUFFIX_CLOUD);

    write_version_file(
        &base_dest,
        edit.base.as_ref(),
        api,
        password,
        &edit.path,
        policy,
    )
    .await?;

    if let Some(ref ours) = edit.ours {
        if let Some(src) = ours_from {
            if src.exists() && !ours.deleted {
                fs::copy(src, &ours_dest).await?;
            } else {
                fs::write(&ours_dest, sentinel("deleted-locally")).await?;
            }
        } else {
            fs::write(&ours_dest, sentinel(ours_missing_label)).await?;
        }
    } else {
        fs::write(&ours_dest, sentinel(ours_missing_label)).await?;
    }

    write_version_file(
        &theirs_dest,
        edit.theirs.as_ref(),
        api,
        password,
        &edit.path,
        policy,
    )
    .await?;
    Ok(())
}

#[must_use]
pub fn enrich_conflict_edit(
    mut edit: ConcurrentEdit,
    kind: ConflictKind,
    conflict_dir: &Path,
) -> ConcurrentEdit {
    let original = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Original);
    let local = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Local);
    let cloud = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Cloud);
    edit.original_file = Some(original.to_string_lossy().into_owned());
    edit.local_file = Some(local.to_string_lossy().into_owned());
    edit.cloud_file = Some(cloud.to_string_lossy().into_owned());
    if local.exists() {
        if let Ok(b) = std::fs::read(&local) {
            edit.local_available = !is_sentinel_content(&b);
            edit.is_binary = is_binary_content(&b);
        }
    }
    if cloud.exists() {
        if let Ok(b) = std::fs::read(&cloud) {
            edit.cloud_available = !is_sentinel_content(&b);
            if !edit.is_binary {
                edit.is_binary = is_binary_content(&b);
            }
        }
    }
    set_kind_hint(&mut edit, kind);
    edit
}

#[must_use]
pub fn enrich_conflict_edit_preview(
    mut edit: ConcurrentEdit,
    kind: ConflictKind,
) -> ConcurrentEdit {
    set_kind_hint(&mut edit, kind);
    edit
}

fn set_kind_hint(edit: &mut ConcurrentEdit, kind: ConflictKind) {
    edit.kind = Some(kind);
    edit.hint = Some(format!(
        "feanorfs conflicts keep {} --local | --cloud | --both | --file <reconciled>",
        edit.path
    ));
}

#[cfg(test)]
mod tests {
    use super::{is_cloud_deleted_sentinel, is_sentinel_content, sentinel_label, SENTINEL_PREFIX};

    #[test]
    fn cloud_deleted_sentinel_is_recognized() {
        let deleted = format!("{SENTINEL_PREFIX}deleted>\n");
        assert!(is_sentinel_content(deleted.as_bytes()));
        assert_eq!(sentinel_label(deleted.as_bytes()), Some("deleted"));
        assert!(is_cloud_deleted_sentinel(deleted.as_bytes()));
    }

    #[test]
    fn download_failed_sentinel_is_not_cloud_deleted() {
        let failed = format!("{SENTINEL_PREFIX}download-failed offline>\n");
        assert!(is_sentinel_content(failed.as_bytes()));
        assert!(!is_cloud_deleted_sentinel(failed.as_bytes()));
    }
}
