use crate::workspace_read::WorkspaceReadRoot;
use anyhow::{Context as _, Result};
use feanorfs_common::FileState;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read as _, Seek as _};
use std::path::Path;
use std::sync::OnceLock;

use super::walker::{
    build_ignore_matcher, build_workspace_walker_with_read_root, path_under_tagged_directory,
    portable_mode, portable_rel_path,
};
use super::{CacheEntry, ClientDb};

static WARNED_LEGACY_PASSWORD_SCAN: OnceLock<()> = OnceLock::new();

/// True when `relative` is skipped by the same policy the walker applies:
/// hard excludes, the ignore matcher (including ancestor directory patterns),
/// or a valid CACHEDIR.TAG ancestor.
fn path_is_policy_excluded(
    read_root: &WorkspaceReadRoot,
    relative: &str,
    ignores: &Option<ignore::gitignore::Gitignore>,
) -> bool {
    let relative_path = Path::new(relative);
    if super::walker::is_always_excluded(relative_path) {
        return true;
    }
    if path_under_tagged_directory(read_root, relative_path) {
        return true;
    }
    let Some(matcher) = ignores.as_ref() else {
        return false;
    };
    if matcher.matched(relative, false).is_ignore() {
        return true;
    }
    // The walker prunes ignored directories before descending, so descendant
    // files never reach the matcher. Replicate that: a directory pattern like
    // `out/` excludes every ancestor chain member beneath the workspace root.
    let mut ancestor = relative_path.parent();
    while let Some(directory) = ancestor {
        if !directory.as_os_str().is_empty() && matcher.matched(directory, true).is_ignore() {
            return true;
        }
        ancestor = directory.parent();
    }
    false
}

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

/// Scan with optional in-memory workspace ignore rules.
///
/// This exists for safe join preview: the receiver can classify its files
/// under the encrypted mirror policy before saving it in global state.
pub async fn scan_local_directory_with_policy(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
) -> Result<HashMap<String, FileState>> {
    let read_root = WorkspaceReadRoot::open(base_path)
        .with_context(|| format!("open workspace scan root {}", base_path.display()))?;
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

    for result in build_workspace_walker_with_read_root(
        base_path,
        no_default_ignores,
        ignore_policy,
        read_root.clone(),
    )
    .build()
    {
        let entry = result.context("walk workspace during directory scan")?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "inspect admitted workspace entry {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            continue;
        }
        let absolute = entry.path();
        let native_relative = absolute.strip_prefix(base_path).with_context(|| {
            format!(
                "derive native relative path for admitted workspace entry {}",
                absolute.display()
            )
        })?;
        let Some(relative) = native_relative.to_str() else {
            continue;
        };
        let Some(path) = portable_rel_path(relative) else {
            continue;
        };
        let mut file = read_root
            .open_regular_path(native_relative)
            .with_context(|| format!("open admitted workspace file {path}"))?;
        let metadata = DiskMetadata::read(&file)
            .with_context(|| format!("inspect admitted workspace file {path}"))?;

        let observed = if let Some(cached) = cached_entries.get(&path) {
            // A tombstoned entry that reappears on disk with the identical
            // mtime/size (rsync -a, cp -p, backup restore) must still have
            // its `deleted_at` cleared: hashes stay valid, but a stale
            // tombstone later makes hydrate/cat/predictive hydration treat
            // the live file as deleted.
            let tombstoned = cached.deleted_at.is_some();
            if cached.hydrated && cached.size == metadata.size && cached.mtime == metadata.mtime {
                #[cfg(unix)]
                let observed_mode = metadata.mode;
                #[cfg(not(unix))]
                let observed_mode = cached.mode;
                if cached.mode == observed_mode && !tombstoned {
                    cache_hits.insert(path.clone());
                }
                ObservedFile::cached(cached, metadata.size, metadata.mtime, observed_mode, true)
            } else if !cached.hydrated && !tombstoned {
                cache_hits.insert(path.clone());
                ObservedFile::cached(cached, cached.size, cached.mtime, cached.mode, false)
            } else {
                match hash_stable_file(
                    &mut file,
                    EncryptionContext {
                        password,
                        relative_path: &path,
                    },
                    metadata,
                )? {
                    Some(observed) => {
                        #[cfg(unix)]
                        {
                            observed
                        }
                        #[cfg(not(unix))]
                        {
                            ObservedFile {
                                mode: cached.mode,
                                ..observed
                            }
                        }
                    }
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
                &mut file,
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
    let ignores = build_ignore_matcher(base_path, no_default_ignores, ignore_policy);
    for (path, cached) in cached_entries.drain() {
        if !disk_files.contains_key(&path) {
            let excluded = path_is_policy_excluded(&read_root, &path, &ignores);
            if excluded {
                // Previously-tracked path is now skipped by an ignore rule or
                // a CACHEDIR.TAG. Freeze it: report the cached state unchanged
                // so the remote copy survives and nothing is re-downloaded,
                // and never tombstone a path that still exists on disk.
                final_files.insert(
                    path.clone(),
                    FileState {
                        path,
                        hash: cached.encrypted_hash,
                        size: cached.size,
                        mtime: cached.server_mtime,
                        deleted: false,
                        mode: cached.mode,
                    },
                );
                continue;
            }
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
    fn read(file: &fs::File) -> Result<Self> {
        let metadata = file.metadata()?;
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
    file: &mut fs::File,
    encryption: EncryptionContext<'_>,
    expected: DiskMetadata,
) -> Result<Option<ObservedFile>> {
    if crate::large_file::uses_chunk_transport(expected.size) {
        let fingerprint = crate::large_file::fingerprint_opened(
            file,
            encryption.password,
            encryption.relative_path,
        )?;
        let observed = DiskMetadata::read(file)?;
        if observed.size != expected.size || observed.mtime != expected.mtime {
            return Ok(None);
        }
        return Ok(Some(ObservedFile {
            plaintext_hash: fingerprint.plaintext_hash,
            encrypted_hash: fingerprint.encrypted_hash,
            size: expected.size,
            mtime: expected.mtime,
            server_mtime: expected.mtime,
            mode: expected.mode,
            hydrated: true,
        }));
    }

    file.rewind()
        .with_context(|| format!("rewind workspace file {}", encryption.relative_path))?;
    let capacity =
        usize::try_from(expected.size).context("small file size exceeds address space")?;
    let mut bytes = Vec::with_capacity(capacity);
    {
        let mut capped = file
            .by_ref()
            .take(crate::large_file::CHUNK_THRESHOLD_BYTES.saturating_add(1));
        capped
            .read_to_end(&mut bytes)
            .with_context(|| format!("read workspace file {}", encryption.relative_path))?;
    }
    let observed = DiskMetadata::read(file)?;
    if observed.size != expected.size
        || observed.mtime != expected.mtime
        || bytes.len() as u64 != expected.size
    {
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
