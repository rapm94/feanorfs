//! Authenticated chunked file transport over the existing opaque CAS API.

use crate::SyncCtx;
use anyhow::{bail, Context as _, Result};
use feanorfs_common::{hash_bytes, is_valid_hash, pack_bytes, unpack_bytes_with_policy};
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt as _;

pub const CHUNK_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;
pub const LEGACY_SINGLE_BLOB_LIMIT_BYTES: u64 = 100 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 8 * 1024 * 1024;
const CHUNKED_PREFIX_BYTE: u8 = 2;
const FORMAT: &str = "feanorfs-chunked-file-v1";
const MAX_CHUNKS: usize = 131_072;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkManifest {
    format: String,
    plaintext_size: u64,
    plaintext_hash: String,
    chunks: Vec<ChunkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkRef {
    hash: String,
    plaintext_size: u32,
}

pub struct LargeFileFingerprint {
    pub plaintext_hash: String,
    pub encrypted_hash: String,
}

pub struct MaterializedFile {
    pub plaintext_hash: String,
    pub size: u64,
}

struct PlannedFile {
    manifest: ChunkManifest,
    ciphertext: Vec<u8>,
    hash: String,
}

#[must_use]
pub fn uses_chunk_transport(size: u64) -> bool {
    size > CHUNK_THRESHOLD_BYTES
}

#[must_use]
pub fn exceeds_legacy_single_blob_limit(size: u64) -> bool {
    size > LEGACY_SINGLE_BLOB_LIMIT_BYTES
}

pub fn fingerprint(
    base_path: &Path,
    password: &str,
    relative_path: &str,
) -> Result<LargeFileFingerprint> {
    let root = crate::workspace_read::WorkspaceReadRoot::open(base_path)?;
    let mut file = root
        .open_regular(relative_path)
        .with_context(|| format!("open large file {relative_path} for fingerprinting"))?;
    fingerprint_opened(&mut file, password, relative_path)
}

pub(crate) fn fingerprint_opened(
    file: &mut std::fs::File,
    password: &str,
    relative_path: &str,
) -> Result<LargeFileFingerprint> {
    let plan = plan_file(file, password, relative_path)?;
    Ok(LargeFileFingerprint {
        plaintext_hash: plan.manifest.plaintext_hash,
        encrypted_hash: plan.hash,
    })
}

pub async fn upload(ctx: &SyncCtx<'_>, relative_path: &str, expected_hash: &str) -> Result<()> {
    upload_inner(ctx, relative_path, expected_hash, true).await
}

/// Upload every authenticated chunk without consulting endpoint-local skip state.
#[doc(hidden)]
pub async fn upload_all_chunks(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    expected_hash: &str,
) -> Result<()> {
    upload_inner(ctx, relative_path, expected_hash, false).await
}

async fn upload_inner(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    expected_hash: &str,
    use_registry: bool,
) -> Result<()> {
    let root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    let mut file = root
        .open_regular(relative_path)
        .with_context(|| format!("open large file {relative_path} for upload"))?;
    let password = ctx.password_str();
    let plan = plan_file(&mut file, password, relative_path)?;
    if plan.hash != expected_hash {
        bail!("{relative_path} changed while preparing its chunked upload; retry sync");
    }
    file.seek(SeekFrom::Start(0))?;
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    let state_dir = ctx.state_dir()?;
    let mut pending = Vec::new();
    for (index, expected) in plan.manifest.chunks.iter().enumerate() {
        // Always read the chunk so the file cursor advances and mid-sync
        // changes to skipped chunks are still detected by length.
        let read = read_chunk(&mut file, &mut buffer)?;
        if read != expected.plaintext_size as usize {
            bail!("{relative_path} changed while uploading chunk {index}; retry sync");
        }
        let ciphertext = seal_chunk(&buffer[..read], password, relative_path, index)?;
        let hash = hash_bytes(&ciphertext);
        if hash != expected.hash {
            bail!("{relative_path} changed while uploading chunk {index}; retry sync");
        }
        if !use_registry || !crate::upload_registry::known(&state_dir, &expected.hash).await {
            pending.push((expected.hash.clone(), ciphertext));
            if pending.len() == 4 {
                flush_chunk_batch(ctx, &state_dir, &mut pending, use_registry).await?;
            }
        }
    }
    let mut extra = [0_u8; 1];
    if file.read(&mut extra)? != 0 {
        bail!("{relative_path} grew while uploading; retry sync");
    }
    flush_chunk_batch(ctx, &state_dir, &mut pending, use_registry).await?;
    ctx.api
        .upload_object(ctx.workspace_id(), &plan.hash, plan.ciphertext)
        .await
}

async fn flush_chunk_batch(
    ctx: &SyncCtx<'_>,
    state_dir: &Path,
    pending: &mut Vec<(String, Vec<u8>)>,
    use_registry: bool,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let futures = pending.iter().map(|(hash, ciphertext)| {
        ctx.api
            .upload_object(ctx.workspace_id(), hash, ciphertext.clone())
    });
    for result in futures_util::future::join_all(futures).await {
        result?;
    }
    if use_registry {
        for (hash, _) in pending.iter() {
            crate::upload_registry::remember(state_dir, hash);
        }
    }
    pending.clear();
    Ok(())
}

/// Authenticates and transactionally materializes one object at a safe workspace path.
///
/// A matching tracked entry may be rehydrated. An absent destination remains a
/// supported compatibility case and becomes tracked after commit; live
/// untracked content and tombstoned tracked paths are never overwritten.
pub async fn materialize(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    encrypted_hash: &str,
    expected_size: u64,
) -> Result<MaterializedFile> {
    if !feanorfs_common::is_safe_rel_path(relative_path) {
        bail!("materialize target must be a safe relative workspace path");
    }
    let _sync_guard = crate::lock::SyncLock::acquire(ctx.base)?;
    let local_files = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let before = ctx.db.get_cache_entries().await?;
    let (materialized_size, server_mtime, mode) = if let Some(entry) = before.get(relative_path) {
        if entry.deleted_at.is_some()
            || entry.encrypted_hash != encrypted_hash
            || (expected_size != 0 && entry.size != expected_size)
            || !local_files
                .get(relative_path)
                .is_some_and(|state| !state.deleted && state.hash == entry.encrypted_hash)
        {
            bail!("materialize target {relative_path} no longer matches its cached identity");
        }
        (
            if expected_size == 0 {
                entry.size
            } else {
                expected_size
            },
            entry.server_mtime,
            entry.mode,
        )
    } else {
        if local_files
            .get(relative_path)
            .is_some_and(|state| !state.deleted)
        {
            bail!("materialize target {relative_path} would overwrite an untracked local file");
        }
        (expected_size, 0, 0)
    };
    let response = feanorfs_common::SyncResponse {
        upload_required: Vec::new(),
        download_required: vec![feanorfs_common::FileState {
            path: relative_path.to_string(),
            hash: encrypted_hash.to_string(),
            size: materialized_size,
            mtime: server_mtime,
            deleted: false,
            mode,
        }],
        delete_local: Vec::new(),
    };
    crate::sync_pass::process_downloads(ctx, &response, &local_files, false).await?;
    let after = ctx.db.get_cache_entries().await?;
    let hydrated = after
        .get(relative_path)
        .filter(|entry| entry.hydrated && entry.encrypted_hash == encrypted_hash)
        .with_context(|| format!("materialize target {relative_path} did not commit to cache"))?;
    Ok(MaterializedFile {
        plaintext_hash: hydrated.plaintext_hash.clone(),
        size: hydrated.size,
    })
}

pub(crate) async fn materialize_to(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    encrypted_hash: &str,
    expected_size: u64,
    destination: &Path,
) -> Result<MaterializedFile> {
    let root = download_verified(ctx, encrypted_hash, root_download_limit(expected_size)).await?;
    if root.first() != Some(&CHUNKED_PREFIX_BYTE) {
        let plaintext =
            unpack_bytes_with_policy(&root, ctx.password_str(), relative_path, ctx.policy)?;
        if expected_size != 0 && plaintext.len() as u64 != expected_size {
            bail!("downloaded file size mismatch for {relative_path}");
        }
        atomic_write_destination(destination, &plaintext).await?;
        return Ok(MaterializedFile {
            plaintext_hash: hash_bytes(&plaintext),
            size: plaintext.len() as u64,
        });
    }

    let manifest = open_manifest(&root, ctx.password_str(), relative_path)?;
    if expected_size != 0 && manifest.plaintext_size != expected_size {
        bail!("chunk manifest size mismatch for {relative_path}");
    }
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let (temporary_path, mut temporary) = create_temp(destination).await?;
    let mut guard = TempGuard(Some(temporary_path.clone()));
    let mut plaintext_hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        let ciphertext = download_verified(ctx, &chunk.hash, CHUNK_BYTES + 29).await?;
        let plaintext = open_chunk(&ciphertext, ctx.password_str(), relative_path, index)?;
        if plaintext.len() != chunk.plaintext_size as usize {
            bail!("chunk size mismatch for {relative_path} at index {index}");
        }
        total = total
            .checked_add(plaintext.len() as u64)
            .context("large file size overflow")?;
        plaintext_hasher.update(&plaintext);
        temporary.write_all(&plaintext).await?;
    }
    if total != manifest.plaintext_size
        || plaintext_hasher.finalize().to_hex().as_str() != manifest.plaintext_hash
    {
        bail!("chunked file integrity check failed for {relative_path}");
    }
    temporary.flush().await?;
    temporary.sync_all().await?;
    drop(temporary);
    tokio::fs::rename(&temporary_path, destination).await?;
    guard.0 = None;
    Ok(MaterializedFile {
        plaintext_hash: manifest.plaintext_hash,
        size: total,
    })
}

pub async fn read_bytes(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    encrypted_hash: &str,
    expected_size: u64,
) -> Result<Vec<u8>> {
    let root = download_verified(ctx, encrypted_hash, root_download_limit(expected_size)).await?;
    if root.first() != Some(&CHUNKED_PREFIX_BYTE) {
        let plaintext =
            unpack_bytes_with_policy(&root, ctx.password_str(), relative_path, ctx.policy)?;
        if expected_size != 0 && plaintext.len() as u64 != expected_size {
            bail!("downloaded file size mismatch for {relative_path}");
        }
        return Ok(plaintext);
    }
    let manifest = open_manifest(&root, ctx.password_str(), relative_path)?;
    if expected_size != 0 && manifest.plaintext_size != expected_size {
        bail!("chunk manifest size mismatch for {relative_path}");
    }
    let capacity = usize::try_from(expected_size).context("file is too large for memory")?;
    let mut output = Vec::with_capacity(capacity);
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        let ciphertext = download_verified(ctx, &chunk.hash, CHUNK_BYTES + 29).await?;
        let plaintext = open_chunk(&ciphertext, ctx.password_str(), relative_path, index)?;
        if plaintext.len() != chunk.plaintext_size as usize {
            bail!("chunk size mismatch for {relative_path} at index {index}");
        }
        output.extend_from_slice(&plaintext);
    }
    if (expected_size != 0 && output.len() as u64 != expected_size)
        || hash_bytes(&output) != manifest.plaintext_hash
    {
        bail!("chunked file integrity check failed for {relative_path}");
    }
    Ok(output)
}

/// Lists the chunk hashes of a chunked file, or nothing for plain files.
///
/// `size` is the caller's knowledge of the file size: `Some` keeps the
/// fast path that skips blobs known to be below the chunk threshold; `None`
/// means the size is unknown (conflict legs other than the visible one are
/// not stored in the tree) and the blob itself must be inspected so a large
/// hidden leg's chunks are still included in reachability manifests.
pub async fn reachable_chunks(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    encrypted_hash: &str,
    size: Option<u64>,
) -> Result<Vec<String>> {
    if size.is_some_and(|size| !uses_chunk_transport(size)) {
        return Ok(Vec::new());
    }
    let root = download_verified(
        ctx,
        encrypted_hash,
        size.map_or(CHUNK_THRESHOLD_BYTES as usize + 29, root_download_limit),
    )
    .await?;
    if root.first() != Some(&CHUNKED_PREFIX_BYTE) {
        return Ok(Vec::new());
    }
    Ok(open_manifest(&root, ctx.password_str(), relative_path)?
        .chunks
        .into_iter()
        .map(|chunk| chunk.hash)
        .collect())
}

fn plan_file(file: &mut std::fs::File, password: &str, relative_path: &str) -> Result<PlannedFile> {
    file.seek(SeekFrom::Start(0))?;
    let before = file.metadata()?;
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    let mut chunks = Vec::new();
    let mut plaintext_hasher = blake3::Hasher::new();
    let mut index = 0_usize;
    loop {
        let read = read_chunk(&mut *file, &mut buffer)?;
        if read == 0 {
            break;
        }
        plaintext_hasher.update(&buffer[..read]);
        let ciphertext = seal_chunk(&buffer[..read], password, relative_path, index)?;
        chunks.push(ChunkRef {
            hash: hash_bytes(&ciphertext),
            plaintext_size: u32::try_from(read).expect("chunk size fits u32"),
        });
        index += 1;
        if chunks.len() > MAX_CHUNKS {
            bail!("large file exceeds the supported chunk count");
        }
    }
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        bail!("large file changed while it was being scanned; retry sync");
    }
    let manifest = ChunkManifest {
        format: FORMAT.into(),
        plaintext_size: before.len(),
        plaintext_hash: plaintext_hasher.finalize().to_hex().to_string(),
        chunks,
    };
    validate_manifest(&manifest)?;
    let plaintext = serde_json::to_vec(&manifest).context("encode chunk manifest")?;
    let mut ciphertext = pack_bytes(&plaintext, password, &manifest_domain(relative_path))?;
    ciphertext[0] = CHUNKED_PREFIX_BYTE;
    let hash = hash_bytes(&ciphertext);
    Ok(PlannedFile {
        manifest,
        ciphertext,
        hash,
    })
}

fn open_manifest(ciphertext: &[u8], password: &str, relative_path: &str) -> Result<ChunkManifest> {
    if ciphertext.first() != Some(&CHUNKED_PREFIX_BYTE) {
        bail!("not a chunk manifest");
    }
    let mut authenticated = ciphertext.to_vec();
    authenticated[0] = feanorfs_common::AEAD_PREFIX_BYTE;
    let plaintext = unpack_bytes_with_policy(
        &authenticated,
        password,
        &manifest_domain(relative_path),
        feanorfs_common::LegacyPolicy::Reject,
    )?;
    let manifest: ChunkManifest =
        serde_json::from_slice(&plaintext).context("decode chunk manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &ChunkManifest) -> Result<()> {
    if manifest.format != FORMAT || !is_valid_hash(&manifest.plaintext_hash) {
        bail!("invalid chunk manifest identity");
    }
    if manifest.chunks.is_empty() || manifest.chunks.len() > MAX_CHUNKS {
        bail!("invalid chunk manifest length");
    }
    let mut total = 0_u64;
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        if !is_valid_hash(&chunk.hash)
            || chunk.plaintext_size == 0
            || chunk.plaintext_size as usize > CHUNK_BYTES
            || (index + 1 != manifest.chunks.len() && chunk.plaintext_size as usize != CHUNK_BYTES)
        {
            bail!("invalid chunk manifest entry at index {index}");
        }
        total = total
            .checked_add(u64::from(chunk.plaintext_size))
            .context("chunk manifest size overflow")?;
    }
    if total != manifest.plaintext_size || !uses_chunk_transport(total) {
        bail!("chunk manifest plaintext size is inconsistent");
    }
    Ok(())
}

fn seal_chunk(bytes: &[u8], password: &str, relative_path: &str, index: usize) -> Result<Vec<u8>> {
    pack_bytes(bytes, password, &chunk_domain(relative_path, index))
}

fn open_chunk(
    ciphertext: &[u8],
    password: &str,
    relative_path: &str,
    index: usize,
) -> Result<Vec<u8>> {
    unpack_bytes_with_policy(
        ciphertext,
        password,
        &chunk_domain(relative_path, index),
        feanorfs_common::LegacyPolicy::Reject,
    )
}

fn manifest_domain(path: &str) -> String {
    format!("feanorfs-chunk-manifest-v1\0{path}")
}

fn chunk_domain(path: &str, index: usize) -> String {
    format!("feanorfs-file-chunk-v1\0{path}\0{index}")
}

fn read_chunk(reader: &mut impl std::io::Read, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

fn root_download_limit(expected_size: u64) -> usize {
    if expected_size == 0 {
        // Zero is also the legacy/hidden-conflict "size unknown" sentinel.
        // Permit any valid non-chunked file root, then authenticate and derive
        // the actual size before returning it.
        CHUNK_THRESHOLD_BYTES as usize + 29
    } else if uses_chunk_transport(expected_size) {
        feanorfs_common::MAX_ENCRYPTED_OBJECT_BYTES
    } else {
        usize::try_from(expected_size)
            .unwrap_or(CHUNK_THRESHOLD_BYTES as usize)
            .min(CHUNK_THRESHOLD_BYTES as usize)
            .saturating_add(29)
    }
}

async fn download_verified(ctx: &SyncCtx<'_>, hash: &str, max_bytes: usize) -> Result<Vec<u8>> {
    if !is_valid_hash(hash) {
        bail!("invalid encrypted object hash");
    }
    // Prefer the verified local object cache over a network fetch so hydrating,
    // reading, and reachability passes do not re-download cached blobs.
    if let Some(cached) = crate::objects::cached_object(ctx, hash, max_bytes).await? {
        return Ok(cached);
    }
    let bytes = ctx.api.download_file_bounded(hash, max_bytes).await?;
    if hash_bytes(&bytes) != hash {
        bail!("downloaded encrypted object failed its ciphertext hash check");
    }
    Ok(bytes)
}

async fn atomic_write_destination(destination: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let (temporary_path, mut temporary) = create_temp(destination).await?;
    let mut guard = TempGuard(Some(temporary_path.clone()));
    temporary.write_all(bytes).await?;
    temporary.flush().await?;
    temporary.sync_all().await?;
    drop(temporary);
    tokio::fs::rename(&temporary_path, destination).await?;
    guard.0 = None;
    Ok(())
}

async fn create_temp(destination: &Path) -> Result<(PathBuf, tokio::fs::File)> {
    // A sibling temp guarantees the verified file can be published with one
    // atomic rename even when the workspace is on a separate volume.
    let directory = destination
        .parent()
        .context("large-file destination has no parent")?;
    tokio::fs::create_dir_all(&directory).await?;
    for attempt in 0..64_u64 {
        let path = directory.join(format!(".feanorfs-tmp-{}-{attempt}", std::process::id()));
        match tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not allocate a temporary large-file download")
}

struct TempGuard(Option<PathBuf>);

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_legacy_size_accepts_any_valid_non_chunked_root() {
        assert_eq!(root_download_limit(0), CHUNK_THRESHOLD_BYTES as usize + 29);
    }

    #[test]
    fn manifest_is_authenticated_and_path_bound() {
        let mut chunks = vec![
            ChunkRef {
                hash: "a".repeat(64),
                plaintext_size: CHUNK_BYTES as u32,
            };
            8
        ];
        chunks.push(ChunkRef {
            hash: "b".repeat(64),
            plaintext_size: 1,
        });
        let manifest = ChunkManifest {
            format: FORMAT.into(),
            plaintext_size: CHUNK_THRESHOLD_BYTES + 1,
            plaintext_hash: "c".repeat(64),
            chunks,
        };
        let plaintext = serde_json::to_vec(&manifest).unwrap();
        let mut ciphertext =
            pack_bytes(&plaintext, "password", &manifest_domain("large.bin")).unwrap();
        ciphertext[0] = CHUNKED_PREFIX_BYTE;
        assert!(open_manifest(&ciphertext, "password", "large.bin").is_ok());
        assert!(open_manifest(&ciphertext, "password", "other.bin").is_err());
        let mut tampered = ciphertext;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open_manifest(&tampered, "password", "large.bin").is_err());
    }
    #[tokio::test]
    async fn public_materialize_normalizes_unknown_size_before_cache_commit() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let hub_dir = tempfile::tempdir().unwrap();
        let hub = crate::LocalHub::open(hub_dir.path().to_path_buf(), None)
            .await
            .unwrap();
        let api = crate::ApiClient::local(hub, None);
        let db = crate::ClientDb::new(state.path()).await.unwrap();
        let config = crate::Config {
            server_url: crate::local::LOCAL_HUB_URL.to_string(),
            workspace_id: "unknown-size-materialize".to_string(),
            encryption_password: Some("11".repeat(32)),
            server_password: None,
            tls_ca_pem: None,
            format_version: 3,
            hub_local: true,
            relay: None,
        };
        let ctx = crate::SyncCtx::from_config(&api, &db, workspace.path(), &config).unwrap();
        let path = "unknown.txt";
        let plaintext = b"authenticated bytes with an unknown legacy size";
        let ciphertext = feanorfs_common::pack_bytes(plaintext, ctx.password_str(), path).unwrap();
        let encrypted_hash = feanorfs_common::hash_bytes(&ciphertext);
        api.upload_object(ctx.workspace_id(), &encrypted_hash, ciphertext)
            .await
            .unwrap();

        let placeholder = workspace.path().join(path);
        std::fs::write(&placeholder, b"").unwrap();
        let mut permissions = std::fs::metadata(&placeholder).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&placeholder, permissions).unwrap();
        let metadata = std::fs::metadata(&placeholder).unwrap();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(0);
        db.upsert_cache_entry(&crate::local::CacheEntry {
            path: path.to_string(),
            plaintext_hash: String::new(),
            encrypted_hash: encrypted_hash.clone(),
            size: 0,
            mtime,
            server_mtime: 0,
            mode: 0,
            hydrated: false,
            deleted_at: None,
        })
        .await
        .unwrap();

        let result = materialize(&ctx, path, &encrypted_hash, 0).await.unwrap();
        assert_eq!(result.size, plaintext.len() as u64);
        assert_eq!(std::fs::read(&placeholder).unwrap(), plaintext);
        let cached = db.get_cache_entries().await.unwrap();
        let entry = &cached[path];
        assert!(entry.hydrated);
        assert_eq!(entry.size, plaintext.len() as u64);

        let absent_path = "new.txt";
        let absent_plaintext = b"safe absent compatibility target";
        let absent_ciphertext =
            feanorfs_common::pack_bytes(absent_plaintext, ctx.password_str(), absent_path).unwrap();
        let absent_hash = feanorfs_common::hash_bytes(&absent_ciphertext);
        api.upload_object(ctx.workspace_id(), &absent_hash, absent_ciphertext)
            .await
            .unwrap();
        let absent = materialize(
            &ctx,
            absent_path,
            &absent_hash,
            absent_plaintext.len() as u64,
        )
        .await
        .unwrap();
        assert_eq!(absent.size, absent_plaintext.len() as u64);
        assert_eq!(
            std::fs::read(workspace.path().join(absent_path)).unwrap(),
            absent_plaintext
        );
    }
}
