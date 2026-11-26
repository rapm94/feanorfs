use crate::workspace_read::{WalkCursor, WorkspaceReadRoot};
use anyhow::{Context as _, Result};
use feanorfs_common::FileState;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read as _, Seek as _};
use std::path::Path;
use std::sync::OnceLock;

use super::walker::{
    build_ignore_matcher, build_workspace_walker_with_cursor, path_under_skipped_subtree,
    portable_mode, portable_rel_path, SharedCursor,
};
use super::{CacheEntry, ClientDb};

static WARNED_LEGACY_PASSWORD_SCAN: OnceLock<()> = OnceLock::new();

/// True when `relative` is skipped by the same policy the walker applies:
/// hard excludes, the ignore matcher (including ancestor directory patterns),
/// or a valid CACHEDIR.TAG / linked Git worktree ancestor.
fn path_is_policy_excluded(
    cursor: &SharedCursor,
    relative: &str,
    ignores: &Option<ignore::gitignore::Gitignore>,
) -> bool {
    let relative_path = Path::new(relative);
    if super::walker::is_always_excluded(relative_path) {
        return true;
    }
    if path_under_skipped_subtree(&mut cursor.lock(), relative_path) {
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
    let (files, _) = scan(base_path, db, password, no_default_ignores, ignore_policy).await?;
    Ok(files)
}

/// Ordinary scan plus the sorted symlinks it skipped, from one walk.
pub async fn scan_local_directory_with_symlinks(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
) -> Result<(HashMap<String, FileState>, Vec<String>)> {
    scan(base_path, db, password, false, None).await
}

async fn scan(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
    no_default_ignores: bool,
    ignore_policy: Option<&str>,
) -> Result<(HashMap<String, FileState>, Vec<String>)> {
    let read_root = WorkspaceReadRoot::open(base_path)
        .with_context(|| format!("open workspace scan root {}", base_path.display()))?;
    let cursor = SharedCursor::new(read_root);
    let mut cached_entries = db.get_cache_entries().await?;
    let mut cache_hits = HashSet::new();
    let mut disk_files = HashMap::new();
    let mut symlinks = Vec::new();
    let password = password.unwrap_or_else(|| {
        WARNED_LEGACY_PASSWORD_SCAN.get_or_init(|| {
            tracing::warn!(
                "No E2EE password set. Using insecure legacy default for directory scan."
            );
        });
        feanorfs_common::LEGACY_DEFAULT_PASSWORD
    });

    for result in build_workspace_walker_with_cursor(
        base_path,
        no_default_ignores,
        ignore_policy,
        cursor.clone(),
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
        if !file_type.is_file() && !file_type.is_symlink() {
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
        if file_type.is_symlink() {
            symlinks.push(path);
            continue;
        }
        let mut admitted = AdmittedFile::inspect(&cursor, native_relative)
            .with_context(|| format!("open admitted workspace file {path}"))?;
        let metadata = admitted.metadata;

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
                match admitted.hash_stable(EncryptionContext {
                    password,
                    relative_path: &path,
                })? {
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
            let Some(observed) = admitted.hash_stable(EncryptionContext {
                password,
                relative_path: &path,
            })?
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

    let projected_records = cached_entries.len().saturating_add(
        disk_files
            .keys()
            .filter(|path| !cached_entries.contains_key(*path))
            .count(),
    );
    let mut final_files = HashMap::new();
    let mut dirty_entries = Vec::new();
    let mut tombstone_paths = Vec::new();
    let ignores = build_ignore_matcher(base_path, no_default_ignores, ignore_policy);
    for (path, cached) in cached_entries.drain() {
        if !disk_files.contains_key(&path) {
            // An existing tombstone keeps projecting a deletion even once its
            // path becomes excluded: freezing it would publish a live entry
            // whose bytes are gone from disk.
            let excluded =
                cached.deleted_at.is_none() && path_is_policy_excluded(&cursor, &path, &ignores);
            if excluded {
                // Previously-tracked path is now skipped by an ignore rule, a
                // CACHEDIR.TAG, or a linked worktree. Freeze it: report the
                // cached state unchanged so the remote copy survives and
                // nothing is re-downloaded, and never tombstone a path that
                // still exists on disk.
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
            tombstone_paths.push(path.clone());
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
    // Tombstones are cleared only after a successful push, so a pass that
    // cannot persist its scan would never shrink state again. At the record
    // cap, drop persisted tombstones; this pass still reports the deletions.
    // ponytail: evicts every tombstone at the cap; a format-v<3 pass that
    // then fails forgets those deletes and the hub copy returns on next sync.
    if projected_records > crate::state::MAX_LOCAL_STATE_RECORDS && !tombstone_paths.is_empty() {
        tracing::warn!(
            "Local state reached {} records; dropping {} deletion records so sync can continue",
            projected_records,
            tombstone_paths.len()
        );
        dirty_entries.retain(|entry| entry.deleted_at.is_none());
        db.apply_cache_changes(&dirty_entries, &tombstone_paths)
            .await?;
    } else if !dirty_entries.is_empty() {
        db.bulk_upsert_cache_entries(&dirty_entries).await?;
    }
    symlinks.sort_unstable();
    symlinks.dedup();
    Ok((final_files, symlinks))
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

/// One walked file. Unchanged files are recognized from no-follow metadata
/// alone; a file is opened only when its bytes must be read, and everything
/// derived from those bytes uses metadata of that same descriptor.
struct AdmittedFile<'a> {
    cursor: &'a SharedCursor,
    relative: &'a Path,
    metadata: DiskMetadata,
    opened: Option<fs::File>,
}

impl<'a> AdmittedFile<'a> {
    fn inspect(cursor: &'a SharedCursor, relative: &'a Path) -> Result<Self> {
        let (metadata, opened) = DiskMetadata::inspect(&mut cursor.lock(), relative)?;
        Ok(Self {
            cursor,
            relative,
            metadata,
            opened,
        })
    }

    fn hash_stable(&mut self, encryption: EncryptionContext<'_>) -> Result<Option<ObservedFile>> {
        let mut file = match self.opened.take() {
            Some(file) => file,
            None => {
                let file = self
                    .cursor
                    .lock()
                    .open_regular(self.relative)
                    .with_context(|| {
                        format!("open admitted workspace file {}", encryption.relative_path)
                    })?;
                self.metadata = DiskMetadata::read(&file)?;
                file
            }
        };
        hash_stable_file(&mut file, encryption, self.metadata)
    }
}

#[derive(Clone, Copy)]
struct DiskMetadata {
    size: u64,
    mtime: i64,
    mode: u32,
}

impl DiskMetadata {
    /// Metadata of one regular file, plus the descriptor on platforms that
    /// must open it to inspect it without following links.
    fn inspect(cursor: &mut WalkCursor, relative: &Path) -> Result<(Self, Option<fs::File>)> {
        #[cfg(unix)]
        {
            let stat = cursor.stat_regular(relative)?;
            let metadata = Self {
                size: u64::try_from(stat.st_size).context("negative file size")?,
                mtime: stat_millis(&stat),
                mode: u32::from(stat.st_mode & 0o111 != 0),
            };
            Ok((metadata, None))
        }
        #[cfg(not(unix))]
        {
            let file = cursor.open_regular(relative)?;
            Ok((Self::read(&file)?, Some(file)))
        }
    }

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

/// Must agree with [`modified_millis`]: a difference would rehash every file.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // `stat` field widths differ by platform
fn stat_millis(stat: &libc::stat) -> i64 {
    let seconds = stat.st_mtime as i64;
    if seconds < 0 {
        return 0;
    }
    seconds
        .saturating_mul(1000)
        .saturating_add(stat.st_mtime_nsec as i64 / 1_000_000)
}

fn modified_millis(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
