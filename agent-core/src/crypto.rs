//! Seal plaintext for upload (AEAD pack + Blake3 hash of ciphertext).
use anyhow::Result;
use feanorfs_common::pack_bytes;

pub fn seal(content: &[u8], password: &str, path: &str) -> Result<(String, Vec<u8>)> {
    let packed = pack_bytes(content, password, path)?;
    let hash = feanorfs_common::hash_bytes(&packed);
    Ok((hash, packed))
}
