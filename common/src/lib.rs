use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Generates a cryptographically random 64-char hex password.
/// Uses getrandom (CSPRNG) for entropy, then Blake3-hashes the bytes
/// to produce a stable-length hex string suitable as an E2EE key.
pub fn generate_password() -> Result<String> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed)
        .map_err(|e| anyhow::anyhow!("Failed to generate random bytes: {}", e))?;
    Ok(blake3::hash(&seed).to_hex().to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileState {
    pub path: String,  // Relative path from workspace root, using forward slashes '/'
    pub hash: String,  // Hex-encoded Blake3 hash
    pub size: u64,     // File size in bytes
    pub mtime: i64,    // Modification time in milliseconds since Unix Epoch
    pub deleted: bool, // Whether the file has been deleted
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub workspace_id: String,
    pub files: Vec<FileState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub upload_required: Vec<String>, // Paths of files the client needs to upload
    pub download_required: Vec<FileState>, // Metadata of files the client needs to download
    pub delete_local: Vec<String>,    // Paths the client must delete locally
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypt_bytes_roundtrip_returns_original() {
        let plaintext = b"the quick brown fox jumps over the lazy dog";
        let password = "correct-horse-battery-staple";
        let path = "src/main.rs";

        let ciphertext = crypt_bytes(plaintext, password, path);
        assert_ne!(
            ciphertext, plaintext,
            "ciphertext must differ from plaintext"
        );
        let recovered = crypt_bytes(&ciphertext, password, path);
        assert_eq!(recovered, plaintext, "decrypt(encrypt(x)) must equal x");
    }

    #[test]
    fn crypt_bytes_roundtrip_empty_input() {
        let ciphertext = crypt_bytes(b"", "pass", "path/to/file");
        assert!(ciphertext.is_empty(), "empty input produces empty output");
        let recovered = crypt_bytes(&ciphertext, "pass", "path/to/file");
        assert!(recovered.is_empty());
    }

    #[test]
    fn crypt_bytes_roundtrip_single_byte() {
        let plaintext = [0x41u8];
        let ciphertext = crypt_bytes(&plaintext, "pw", "f.txt");
        assert_ne!(ciphertext, plaintext);
        let recovered = crypt_bytes(&ciphertext, "pw", "f.txt");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn crypt_bytes_different_paths_produce_different_ciphertext() {
        let plaintext = b"identical content";
        let password = "shared-password";

        let ct_a = crypt_bytes(plaintext, password, "path/a.txt");
        let ct_b = crypt_bytes(plaintext, password, "path/b.txt");

        assert_ne!(
            ct_a, ct_b,
            "same plaintext + password but different paths must yield different ciphertext"
        );
    }

    #[test]
    fn crypt_bytes_different_passwords_produce_different_ciphertext() {
        let plaintext = b"identical content";
        let path = "shared/path.txt";

        let ct_a = crypt_bytes(plaintext, "password-one", path);
        let ct_b = crypt_bytes(plaintext, "password-two", path);

        assert_ne!(
            ct_a, ct_b,
            "same plaintext + path but different passwords must yield different ciphertext"
        );
    }

    #[test]
    fn crypt_bytes_is_deterministic() {
        let plaintext = b"deterministic test payload";
        let password = "pw";
        let path = "file.rs";

        let ct1 = crypt_bytes(plaintext, password, path);
        let ct2 = crypt_bytes(plaintext, password, path);
        assert_eq!(ct1, ct2, "same inputs must produce same ciphertext");
    }

    #[test]
    fn crypt_bytes_empty_password_still_encrypts() {
        let plaintext = b"secret";
        let ciphertext = crypt_bytes(plaintext, "", "path");
        assert_ne!(ciphertext, plaintext);
        let recovered = crypt_bytes(&ciphertext, "", "path");
        assert_eq!(recovered, plaintext);
    }

    // --- hash_bytes ---

    #[test]
    fn hash_bytes_is_deterministic() {
        let data = b"hello world";
        assert_eq!(hash_bytes(data), hash_bytes(data));
    }

    #[test]
    fn hash_bytes_different_inputs_yield_different_hashes() {
        let a = hash_bytes(b"hello");
        let b = hash_bytes(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_bytes_empty_input_is_well_defined() {
        let h = hash_bytes(b"");
        assert_eq!(h.len(), 64, "Blake3 hex digest must be 64 chars");
    }

    #[test]
    fn hash_bytes_returns_hex_string() {
        let h = hash_bytes(b"data");
        assert!(
            h.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be hex-encoded: {h}"
        );
    }

    #[test]
    fn normalize_path_converts_backslashes_to_forward() {
        assert_eq!(normalize_path(r"src\main.rs"), "src/main.rs");
    }

    #[test]
    fn normalize_path_handles_nested_backslashes() {
        assert_eq!(
            normalize_path(r"src\nested\deep\file.rs"),
            "src/nested/deep/file.rs"
        );
    }

    #[test]
    fn normalize_path_preserves_forward_slashes() {
        assert_eq!(normalize_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn normalize_path_handles_empty_string() {
        assert_eq!(normalize_path(""), "");
    }

    #[test]
    fn normalize_path_handles_mixed_separators() {
        assert_eq!(normalize_path(r"src/mixed\path.rs"), "src/mixed/path.rs");
    }

    #[test]
    fn file_state_serde_roundtrip() {
        let state = FileState {
            path: "src/main.rs".to_string(),
            hash: "abc123".to_string(),
            size: 4096,
            mtime: 1719500000000,
            deleted: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let decoded: FileState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn file_state_deleted_flag_serializes_correctly() {
        let state = FileState {
            path: "deleted.txt".to_string(),
            hash: "deadbeef".to_string(),
            size: 0,
            mtime: 0,
            deleted: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"deleted\":true"), "json: {json}");
        let decoded: FileState = serde_json::from_str(&json).unwrap();
        assert!(decoded.deleted);
    }

    #[test]
    fn generate_password_returns_64_char_hex() {
        let pw = generate_password().unwrap();
        assert_eq!(pw.len(), 64, "password must be 64 hex chars: {pw}");
        assert!(
            pw.chars().all(|c| c.is_ascii_hexdigit()),
            "password must be hex: {pw}"
        );
    }

    #[test]
    fn generate_password_is_unique() {
        let a = generate_password().unwrap();
        let b = generate_password().unwrap();
        assert_ne!(a, b, "two generated passwords must differ");
    }
}
