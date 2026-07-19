//! Authenticated chunked file transport over the existing opaque CAS API.

use crate::{ApiClient, SyncCtx};
use anyhow::{bail, Context as _, Result};
use feanorfs_common::{hash_bytes, is_valid_hash, pack_bytes, unpack_bytes_with_policy};
use serde::{Deserialize, Serialize};
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
    path: &Path,
    password: &str,
    relative_path: &str,
) -> Result<LargeFileFingerprint> {
    let plan = plan_file(path, password, relative_path)?;
    Ok(LargeFileFingerprint {
        plaintext_hash: plan.manifest.plaintext_hash,
        encrypted_hash: plan.hash,
    })
}

pub async fn upload(ctx: &SyncCtx<'_>, relative_path: &str, expected_hash: &str) -> Result<()> {
    let path = ctx.base.join(relative_path);
    let password = ctx.password_str();
    let plan = plan_file(&path, password, relative_path)?;
    if plan.hash != expected_hash {
        bail!("{relative_path} changed while preparing its chunked upload; retry sync");
    }

    let mut file = std::fs::File::open(&path)
        .with_context(|| format!("open large file {relative_path} for upload"))?;
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    for (index, expected) in plan.manifest.chunks.iter().enumerate() {
        let read = read_chunk(&mut file, &mut buffer)?;
        if read != expected.plaintext_size as usize {
            bail!("{relative_path} changed while uploading chunk {index}; retry sync");
        }
        let ciphertext = seal_chunk(&buffer[..read], password, relative_path, index)?;
        let hash = hash_bytes(&ciphertext);
        if hash != expected.hash {
            bail!("{relative_path} changed while uploading chunk {index}; retry sync");
        }
        ctx.api
            .upload_object(ctx.workspace_id(), &hash, ciphertext)
            .await?;
    }
    ctx.api
        .upload_object(ctx.workspace_id(), &plan.hash, plan.ciphertext)
        .await
}

pub async fn materialize(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    encrypted_hash: &str,
    expected_size: u64,
) -> Result<MaterializedFile> {
    let root = download_verified(ctx.api, encrypted_hash).await?;
    if root.first() != Some(&CHUNKED_PREFIX_BYTE) {
        let plaintext =
            unpack_bytes_with_policy(&root, ctx.password_str(), relative_path, ctx.policy)?;
        if plaintext.len() as u64 != expected_size {
            bail!("downloaded file size mismatch for {relative_path}");
        }
        crate::fs_util::atomic_write(ctx.base, relative_path, &plaintext).await?;
        return Ok(MaterializedFile {
            plaintext_hash: hash_bytes(&plaintext),
            size: plaintext.len() as u64,
        });
    }

    let manifest = open_manifest(&root, ctx.password_str(), relative_path)?;
    if manifest.plaintext_size != expected_size {
        bail!("chunk manifest size mismatch for {relative_path}");
    }
    let destination = ctx.base.join(relative_path);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let (temporary_path, mut temporary) = create_temp(&destination).await?;
    let mut guard = TempGuard(Some(temporary_path.clone()));
    let mut plaintext_hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        let ciphertext = download_verified(ctx.api, &chunk.hash).await?;
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
    tokio::fs::rename(&temporary_path, &destination).await?;
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
    let root = download_verified(ctx.api, encrypted_hash).await?;
    if root.first() != Some(&CHUNKED_PREFIX_BYTE) {
        let plaintext =
            unpack_bytes_with_policy(&root, ctx.password_str(), relative_path, ctx.policy)?;
        if plaintext.len() as u64 != expected_size {
            bail!("downloaded file size mismatch for {relative_path}");
        }
        return Ok(plaintext);
    }
    let manifest = open_manifest(&root, ctx.password_str(), relative_path)?;
    if manifest.plaintext_size != expected_size {
        bail!("chunk manifest size mismatch for {relative_path}");
    }
    let capacity = usize::try_from(expected_size).context("file is too large for memory")?;
    let mut output = Vec::with_capacity(capacity);
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        let ciphertext = download_verified(ctx.api, &chunk.hash).await?;
        let plaintext = open_chunk(&ciphertext, ctx.password_str(), relative_path, index)?;
        if plaintext.len() != chunk.plaintext_size as usize {
            bail!("chunk size mismatch for {relative_path} at index {index}");
        }
        output.extend_from_slice(&plaintext);
    }
    if output.len() as u64 != expected_size || hash_bytes(&output) != manifest.plaintext_hash {
        bail!("chunked file integrity check failed for {relative_path}");
    }
    Ok(output)
}

pub async fn reachable_chunks(
    ctx: &SyncCtx<'_>,
    relative_path: &str,
    encrypted_hash: &str,
    size: u64,
) -> Result<Vec<String>> {
    if !uses_chunk_transport(size) {
        return Ok(Vec::new());
    }
    let root = download_verified(ctx.api, encrypted_hash).await?;
    if root.first() != Some(&CHUNKED_PREFIX_BYTE) {
        return Ok(Vec::new());
    }
    Ok(open_manifest(&root, ctx.password_str(), relative_path)?
        .chunks
        .into_iter()
        .map(|chunk| chunk.hash)
        .collect())
}

fn plan_file(path: &Path, password: &str, relative_path: &str) -> Result<PlannedFile> {
    let before = std::fs::metadata(path)?;
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    let mut chunks = Vec::new();
    let mut plaintext_hasher = blake3::Hasher::new();
    let mut index = 0_usize;
    loop {
        let read = read_chunk(&mut file, &mut buffer)?;
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
    let after = std::fs::metadata(path)?;
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

async fn download_verified(api: &ApiClient, hash: &str) -> Result<Vec<u8>> {
    if !is_valid_hash(hash) {
        bail!("invalid encrypted object hash");
    }
    let bytes = api.download_file(hash).await?;
    if hash_bytes(&bytes) != hash {
        bail!("downloaded encrypted object failed its ciphertext hash check");
    }
    Ok(bytes)
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
}
