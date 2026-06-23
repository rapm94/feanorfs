use serde::{Deserialize, Serialize};
use std::path::Path;
use std::fs::File;
use std::io::Read;
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileState {
    pub path: String,       // Relative path from workspace root, using forward slashes '/'
    pub hash: String,       // Hex-encoded Blake3 hash
    pub size: u64,          // File size in bytes
    pub mtime: i64,         // Modification time in milliseconds since Unix Epoch
    pub deleted: bool,      // Whether the file has been deleted
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub workspace_id: String,
    pub files: Vec<FileState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub upload_required: Vec<String>,       // Paths of files the client needs to upload
    pub download_required: Vec<FileState>,  // Metadata of files the client needs to download
    pub delete_local: Vec<String>,          // Paths the client must delete locally
}

/// Computes the Blake3 hash of a byte slice and returns it as a hex string.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Computes the Blake3 hash of a file on disk.
pub fn hash_file<P: AsRef<Path>>(path: P) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 65536]; // 64KB buffer
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Normalizes a path to use forward slashes for cross-platform consistency.
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Encrypts or decrypts bytes using a symmetric keystream derived from a password and path via Blake3 XOF.
/// Because XOR is symmetric, calling this twice with the same password and path returns the original data.
pub fn crypt_bytes(data: &[u8], password: &str, path: &str) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(password.as_bytes());
    hasher.update(path.as_bytes());
    let mut reader = hasher.finalize_xof();
    
    let mut keystream = vec![0u8; data.len()];
    reader.fill(&mut keystream);
    
    let mut result = data.to_vec();
    for (r, k) in result.iter_mut().zip(keystream.iter()) {
        *r ^= k;
    }
    result
}
