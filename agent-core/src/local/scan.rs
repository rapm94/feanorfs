use anyhow::Result;
use feanorfs_common::FileState;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use super::walker::portable_mode;
use super::{build_workspace_walker_with_ignore_policy, normalize_path_nfc, CacheEntry, ClientDb};

static WARNED_LEGACY_PASSWORD_SCAN: OnceLock<()> = OnceLock::new();

pub async fn scan_local_directory(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
) -> Result<HashMap<String, FileState>> {
    scan_local_directory_with_opts(base_path, db, password, false).await
}

pub async fn scan_local_directory_with_opts(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
    no_default_ignores: bool,
) -> Result<HashMap<String, FileState>> {
    scan_local_directory_with_policy(base_path, db, password, no_default_ignores, None).await
}

/// Scan with an optional in-memory `.feanorfsignore` override.
///
/// This exists for safe join preview: the receiver can classify its files
/// under the encrypted mirror policy before writing that policy locally.
pub async fn scan_local_directory_with_policy(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
) -> Result<HashMap<String, FileState>> {
    let mut cached_entries = db.get_cache_entries().await?;
    let mut cache_hits = HashSet::new();
    let mut disk_files = HashMap::new();
    let password = password.unwrap_or_else(|| {
        WARNED_LEGACY_PASSWORD_SCAN.get_or_init(|| {
            tracing::warn!(
                "No E2EE password set. Using insecure legacy default for directory scan."
            );
        });
        feanorfs_common::LEGACY_DEFAULT_PASSWORD
    });

    for result in
        build_workspace_walker_with_ignore_policy(base_path, no_default_ignores, ignore_policy)
            .build()
    {
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let absolute = entry.path();
        let Ok(relative) = absolute.strip_prefix(base_path) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        let path = normalize_path_nfc(relative);
        if !feanorfs_common::is_safe_rel_path(&path) {
            continue;
        }
        let Ok(metadata) = DiskMetadata::read(absolute) else {
            continue;
        };

        let observed = if let Some(cached) = cached_entries.get(&path) {
            if cached.hydrated && cached.size == metadata.size && cached.mtime == metadata.mtime {
                if cached.mode == metadata.mode {
                    cache_hits.insert(path.clone());
                }
                ObservedFile::cached(cached, metadata.size, metadata.mtime, metadata.mode, true)
            } else if !cached.hydrated {
                cache_hits.insert(path.clone());
                ObservedFile::cached(cached, cached.size, cached.mtime, cached.mode, false)
            } else {
                match hash_stable_file(
                    absolute,
                    EncryptionContext {
                        password,
                        relative_path: &path,
                    },
                    metadata,
                )? {
                    Some(observed) => observed,
                    None => {
                        cache_hits.insert(path.clone());
                        ObservedFile::cached(
                            cached,
                            cached.size,
                            cached.mtime,
                            cached.mode,
                            cached.hydrated,
                        )
                    }
                }
            }
        } else {
            let Some(observed) = hash_stable_file(
                absolute,
                EncryptionContext {
                    password,
                    relative_path: &path,
                },
                metadata,
            )?
            else {
                continue;
            };
            observed
        };

        let cache_entry = CacheEntry {
            path: path.clone(),
            plaintext_hash: observed.plaintext_hash,
            encrypted_hash: observed.encrypted_hash.clone(),
            size: observed.size,
            mtime: observed.mtime,
            server_mtime: observed.server_mtime,
            mode: observed.mode,
            hydrated: observed.hydrated,
            deleted_at: None,
        };
        let file_state = FileState {
            path: path.clone(),
            hash: observed.encrypted_hash,
            size: observed.size,
            mtime: observed.server_mtime,
            deleted: false,
            mode: observed.mode,
        };
        disk_files.insert(path, (cache_entry, file_state));
    }

    let mut final_files = HashMap::new();
    let mut dirty_entries = Vec::new();
    for (path, cached) in cached_entries.drain() {
        if !disk_files.contains_key(&path) {
            let tombstone_mtime = cached
                .deleted_at
                .unwrap_or_else(|| cached.server_mtime.max(cached.mtime).saturating_add(1));
            if cached.deleted_at.is_none() {
                let mut tombstone = cached.clone();
                tombstone.deleted_at = Some(tombstone_mtime);
                dirty_entries.push(tombstone);
            }
            final_files.insert(
                path.clone(),
                FileState {
                    path,
                    hash: cached.encrypted_hash,
                    size: cached.size,
                    mtime: tombstone_mtime,
                    deleted: true,
                    mode: cached.mode,
                },
            );
        }
    }
    for (path, (cache_entry, file_state)) in disk_files {
        if !cache_hits.contains(&path) {
            dirty_entries.push(cache_entry);
        }
        final_files.insert(path, file_state);
    }
    if !dirty_entries.is_empty() {
        db.bulk_upsert_cache_entries(&dirty_entries).await?;
    }
    Ok(final_files)
}

struct ObservedFile {
    plaintext_hash: String,
    encrypted_hash: String,
    size: u64,
    mtime: i64,
    server_mtime: i64,
    mode: u32,
    hydrated: bool,
}

#[derive(Clone, Copy)]
struct DiskMetadata {
    size: u64,
    mtime: i64,
    mode: u32,
}

impl DiskMetadata {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            size: metadata.len(),
            mtime: modified_millis(&metadata),
            mode: portable_mode(&metadata),
        })
    }
}

#[derive(Clone, Copy)]
struct EncryptionContext<'a> {
    password: &'a str,
    relative_path: &'a str,
}

impl ObservedFile {
    fn cached(entry: &CacheEntry, size: u64, mtime: i64, mode: u32, hydrated: bool) -> Self {
        Self {
            plaintext_hash: entry.plaintext_hash.clone(),
            encrypted_hash: entry.encrypted_hash.clone(),
            size,
            mtime,
            server_mtime: entry.server_mtime,
            mode,
            hydrated,
        }
    }
}

fn hash_stable_file(
    path: &Path,
    encryption: EncryptionContext<'_>,
    expected: DiskMetadata,
) -> Result<Option<ObservedFile>> {
    let bytes = fs::read(path)?;
    let observed = DiskMetadata::read(path)?;
    if observed.size != expected.size || observed.mtime != expected.mtime {
        return Ok(None);
    }
    let plaintext_hash = feanorfs_common::hash_bytes(&bytes);
    let encrypted =
        feanorfs_common::pack_bytes(&bytes, encryption.password, encryption.relative_path)?;
    Ok(Some(ObservedFile {
        plaintext_hash,
        encrypted_hash: feanorfs_common::hash_bytes(&encrypted),
        size: expected.size,
        mtime: expected.mtime,
        server_mtime: expected.mtime,
        mode: expected.mode,
        hydrated: true,
    }))
}

fn modified_millis(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
