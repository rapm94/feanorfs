use crate::api::ApiClient;
use anyhow::Result;
use feanorfs_common::{unpack_bytes, ConcurrentEdit, FileState};
use std::path::Path;
use tokio::fs;

/// Prefix for placeholder bytes written when a version cannot be materialized.
pub const SENTINEL_PREFIX: &str = "<feanorfs-sentinel:";

pub fn is_sentinel_content(content: &[u8]) -> bool {
    content.starts_with(SENTINEL_PREFIX.as_bytes())
}

fn sentinel(label: &str) -> Vec<u8> {
    format!("{SENTINEL_PREFIX}{label}>\n").into_bytes()
}

pub async fn write_version_file(
    dest: &Path,
    state: Option<&FileState>,
    api: &ApiClient,
    password: &str,
    path: &str,
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
                    match unpack_bytes(&bytes, password, path) {
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
) -> Result<()> {
    let base_dest = dir.join(format!("{}.base", edit.path));
    let ours_dest = dir.join(format!("{}.ours", edit.path));
    let theirs_dest = dir.join(format!("{}.theirs", edit.path));

    write_version_file(&base_dest, edit.base.as_ref(), api, password, &edit.path).await?;

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
    )
    .await?;
    Ok(())
}
