pub mod agent_contract;
pub mod invite;
pub mod sync_delta;
pub mod three_way;
pub mod tray_contract;
pub mod tree;
mod tree_codec;
mod tree_convert;
mod tree_diff;

pub use agent_contract::{
    AgentCleanResult, AgentListEntry, AgentListOfflineResult, AgentListResult, LogEntry, LogResult,
    SpawnResult, UndoResult,
};
pub use invite::{decode_invite, encode_invite, looks_like_invite, WorkspaceInvite, INVITE_PREFIX};
pub use tray_contract::{
    ConflictKeepResult, ConflictShowResult, RecentWorkspaceEntry, RecentWorkspacesResult,
    TrayAgentEntry, TrayAgentsSummary, TrayConflictEntry, TrayPauseResult, TrayStatusResult,
};

pub use sync_delta::compute_sync_delta;
pub use three_way::{classify_conflict_kind, conflict_candidate_paths, detect_concurrent_edits};
pub use tree::{
    Snapshot, Tree, TreeBundle, TreeChange, TreeChangeKind, TreeEntry, TreeEntryKind,
    EXECUTABLE_MODE,
};
pub use tree_convert::{flat_to_tree, flat_to_tree_with_conflicts, tree_to_flat};
pub use tree_diff::diff_trees;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Insecure legacy default password used when no E2EE password is configured.
/// Kept as a single constant so all call sites share the same fallback.
pub const LEGACY_DEFAULT_PASSWORD: &str = "default-secret-key";

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
    /// Portable executable intent. `0` means non-executable; `1` means executable.
    #[serde(default, skip_serializing_if = "is_zero_mode")]
    pub mode: u32,
}

const fn is_zero_mode(mode: &u32) -> bool {
    *mode == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub workspace_id: String,
    pub files: Vec<FileState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    EditEdit,
    EditDelete,
    DeleteEdit,
}

impl ConflictKind {
    pub fn from_db_str(s: &str) -> Result<Self> {
        match s {
            "edit_edit" => Ok(Self::EditEdit),
            "edit_delete" => Ok(Self::EditDelete),
            "delete_edit" => Ok(Self::DeleteEdit),
            other => anyhow::bail!("unknown conflict kind in db: {other}"),
        }
    }

    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::EditEdit => "edit_edit",
            Self::EditDelete => "edit_delete",
            Self::DeleteEdit => "delete_edit",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictRecord {
    pub path: String,
    pub kind: ConflictKind,
    pub conflict_dir: String,
    pub opened_at: i64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictResolution {
    pub path: String,
    pub method: String,
    pub source_file_hash: Option<String>,
    pub resolved_at: i64,
    pub resolver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub upload_required: Vec<String>, // Paths of files the client needs to upload
    pub download_required: Vec<FileState>, // Metadata of files the client needs to download
    pub delete_local: Vec<String>,    // Paths the client must delete locally
}

/// Opaque per-workspace snapshot head returned by the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadResponse {
    pub snapshot_id: Option<String>,
}

/// Compare-and-swap request for one opaque workspace head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapHeadRequest {
    pub workspace_id: String,
    pub expected: Option<String>,
    pub new: String,
}

/// Snapshot row recorded when an agent workspace is spawned.
/// Represents the server's view of a file at spawn time, which becomes the
/// "base" version used by agent land/check to detect concurrent edits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshotEntry {
    pub agent_name: String,
    pub path: String,
    pub base_hash: String,
    pub base_size: u64,
    pub base_mtime: i64,
}

/// Triple emitted when both the agent and the server modified the same path
/// since the snapshot was taken. FeanorFS does not merge — the consumer
/// (human or AI agent) reconciles the three versions and syncs back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrentEdit {
    pub path: String,
    pub base: Option<FileState>,
    pub ours: Option<FileState>,
    pub theirs: Option<FileState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ConflictKind>,
    #[serde(default)]
    pub local_available: bool,
    #[serde(default)]
    pub cloud_available: bool,
    #[serde(default)]
    pub is_binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_clean: Option<bool>,
}

/// One path applied (or failed) during `agent land`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandedPath {
    pub path: String,
    pub action: String,
}

/// Structured result of `agent check` (read-only preview).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCheckResult {
    pub agent_name: String,
    pub our_changes: Vec<FileState>,
    pub their_changes: Vec<FileState>,
    pub conflicts: Vec<ConcurrentEdit>,
    pub conflict_risk: Vec<String>,
}

/// Structured result of `agent land` (check + apply).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentLandResult {
    pub agent_name: String,
    pub our_changes: Vec<FileState>,
    pub their_changes: Vec<FileState>,
    pub conflicts: Vec<ConcurrentEdit>,
    pub landed: Vec<LandedPath>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}

/// Result of `agent refresh`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRefreshResult {
    pub agent_name: String,
    pub refreshed: Vec<String>,
    pub deferred: Vec<String>,
}

impl ConcurrentEdit {
    #[must_use]
    pub fn new(
        path: String,
        base: Option<FileState>,
        ours: Option<FileState>,
        theirs: Option<FileState>,
    ) -> Self {
        let local_available = ours.as_ref().is_some_and(|o| !o.deleted);
        let cloud_available = theirs.as_ref().is_some_and(|t| !t.deleted);
        Self {
            local_available,
            cloud_available,
            path,
            base,
            ours,
            theirs,
            original_file: None,
            local_file: None,
            cloud_file: None,
            kind: None,
            is_binary: false,
            hint: None,
            proposed_file: None,
            proposal_clean: None,
        }
    }
}
/// decide what to apply, what to pull, and which conflicts need resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCommitResult {
    pub agent_name: String,
    pub our_changes: Vec<FileState>,
    pub their_changes: Vec<FileState>,
    pub conflicts: Vec<ConcurrentEdit>,
}

/// Computes the Blake3 hash of a byte slice and returns it as a hex string.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Computes the Blake3 hash of a file on disk.
pub fn hash_file<P: AsRef<Path>>(path: P) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    // 65_536-byte (64 KiB) buffer — heap-allocated to avoid a large stack frame.
    let mut buffer = vec![0u8; 65_536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Convert file size from SQLite i64 to native u64, saturating at u64::MAX.
pub fn file_size_from_db(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(u64::MAX)
}

/// Convert file size from native u64 to SQLite i64, saturating at i64::MAX.
pub fn file_size_to_db(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

/// Normalizes a path to use forward slashes for cross-platform consistency.
#[must_use]
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Returns true when `path` is a safe workspace-relative path.
#[must_use]
pub fn is_safe_rel_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let normalized = normalize_path(path);
    if normalized.starts_with('/') {
        return false;
    }
    for component in normalized.split('/') {
        if component == ".." {
            return false;
        }
    }
    if normalized == ".feanorfs"
        || normalized == ".git"
        || normalized.starts_with(".feanorfs/")
        || normalized.starts_with(".git/")
        || normalized.contains("/.git/")
        || normalized.contains("/.feanorfs/")
    {
        return false;
    }
    true
}

pub const AEAD_PREFIX_BYTE: u8 = 1;

/// Policy for handling blobs without the AEAD prefix byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LegacyPolicy {
    /// Hard-fail on non-AEAD blobs (format v2 workspaces).
    Reject,
    /// Fall back to legacy Blake3-XOF XOR decrypt (format v1 / migration).
    #[default]
    AllowXorFallback,
}

impl LegacyPolicy {
    #[must_use]
    pub fn from_format_version(version: u32) -> Self {
        if version >= 2 {
            Self::Reject
        } else {
            Self::AllowXorFallback
        }
    }
}

fn derive_crypto_key(password: &str, path: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"feanorfs-aead-v1");
    hasher.update(&(password.len() as u64).to_le_bytes());
    hasher.update(password.as_bytes());
    hasher.update(&(path.len() as u64).to_le_bytes());
    hasher.update(path.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Encrypts or decrypts bytes using a symmetric keystream derived from a password and path via Blake3 XOF.
/// Because XOR is symmetric, calling this twice with the same password and path returns the original data.
///
/// Length prefixes before each field provide domain separation so that
/// `(password="ab", path="cdef")` and `(password="abc", path="def")` produce
/// different keystreams — without them, Blake3's absorbed bytes would be
/// identical.
#[must_use]
pub fn crypt_bytes(data: &[u8], password: &str, path: &str) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(password.len() as u64).to_le_bytes());
    hasher.update(password.as_bytes());
    hasher.update(&(path.len() as u64).to_le_bytes());
    hasher.update(path.as_bytes());
    let mut reader = hasher.finalize_xof();

    let mut result = data.to_vec();
    // 65_536-byte (64 KiB) keystream chunk — heap-allocated to avoid a large stack frame.
    let mut chunk = vec![0u8; 65_536];
    let mut offset = 0;
    while offset < result.len() {
        let n = (result.len() - offset).min(chunk.len());
        reader.fill(&mut chunk[..n]);
        for i in 0..n {
            result[offset + i] ^= chunk[i];
        }
        offset += n;
    }
    result
}

/// Encrypts plaintext for upload (ChaCha20-Poly1305).
pub fn pack_bytes(data: &[u8], password: &str, path: &str) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};

    let key = derive_crypto_key(password, path);
    let cipher = ChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
    let mut nonce_hasher = blake3::Hasher::new();
    nonce_hasher.update(b"feanorfs-aead-nonce-v1");
    nonce_hasher.update(&key);
    nonce_hasher.update(&(data.len() as u64).to_le_bytes());
    nonce_hasher.update(data);
    let digest = nonce_hasher.finalize();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&digest.as_bytes()[..12]);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), data)
        .map_err(|e| anyhow::anyhow!("AEAD encrypt failed: {e}"))?;
    let mut out = Vec::with_capacity(1 + 12 + ciphertext.len());
    out.push(AEAD_PREFIX_BYTE);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts packed blob (ChaCha20-Poly1305 or legacy XOR per policy).
pub fn unpack_bytes(data: &[u8], password: &str, path: &str) -> Result<Vec<u8>> {
    unpack_bytes_with_policy(data, password, path, LegacyPolicy::AllowXorFallback)
}

/// Decrypt with an explicit legacy-blob policy (format v2 uses `Reject`).
pub fn unpack_bytes_with_policy(
    data: &[u8],
    password: &str,
    path: &str,
    policy: LegacyPolicy,
) -> Result<Vec<u8>> {
    if data.first() == Some(&AEAD_PREFIX_BYTE) && data.len() > 13 {
        use chacha20poly1305::aead::{Aead, KeyInit};
        use chacha20poly1305::{ChaCha20Poly1305, Nonce};

        let key = derive_crypto_key(password, path);
        let cipher = ChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
        let nonce = Nonce::from_slice(&data[1..13]);
        let plain = cipher.decrypt(nonce, &data[13..]).map_err(|_| {
            anyhow::anyhow!("wrong encryption key for this workspace (decryption failed)")
        })?;
        return Ok(plain);
    }
    match policy {
        LegacyPolicy::Reject => anyhow::bail!(
            "blob uses legacy unauthenticated encryption; run `feanorfs migrate` to re-seal"
        ),
        LegacyPolicy::AllowXorFallback => Ok(crypt_bytes(data, password, path)),
    }
}

/// Returns true if `hash` is a valid Blake3 hex digest (64 lowercase hex chars).
/// Used to reject path-traversal attempts in blob download/upload endpoints.
#[must_use]
pub fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
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

    #[test]
    fn is_safe_rel_path_allows_file_with_dot_dot_prefix() {
        assert!(is_safe_rel_path("file..txt"));
        assert!(is_safe_rel_path("v1..v2.patch"));
    }

    #[test]
    fn is_safe_rel_path_rejects_directory_traversal_components() {
        assert!(!is_safe_rel_path("../etc/passwd"));
        assert!(!is_safe_rel_path("src/../../etc/passwd"));
        assert!(!is_safe_rel_path("foo/../bar"));
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
            size: 4_096,
            mtime: 1_719_500_000_000,
            deleted: false,
            mode: 0,
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
            mode: 0,
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

    #[test]
    fn is_valid_hash_accepts_64_hex_chars() {
        let h = hash_bytes(b"some payload");
        assert!(is_valid_hash(&h), "{}", h);
    }

    #[test]
    fn is_valid_hash_rejects_too_short() {
        assert!(!is_valid_hash("abc123"));
    }

    #[test]
    fn is_valid_hash_rejects_too_long() {
        assert!(!is_valid_hash(&"a".repeat(65)));
    }

    #[test]
    fn is_valid_hash_rejects_non_hex() {
        assert!(!is_valid_hash(&"z".repeat(64)));
    }

    #[test]
    fn is_valid_hash_rejects_uppercase_hex() {
        assert!(!is_valid_hash(&"A".repeat(64)));
        assert!(!is_valid_hash(&"F".repeat(64)));
        assert!(is_valid_hash(&"a".repeat(64)));
        assert!(is_valid_hash(&"f".repeat(64)));
    }

    #[test]
    fn is_valid_hash_rejects_path_traversal_patterns() {
        assert!(!is_valid_hash(".."));
        assert!(!is_valid_hash("../../db.sqlite"));
        assert!(!is_valid_hash(""));
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let plain = b"hello aead world";
        let packed = pack_bytes(plain, "pw", "path/file.txt").unwrap();
        assert_eq!(packed.first(), Some(&AEAD_PREFIX_BYTE));
        let recovered = unpack_bytes(&packed, "pw", "path/file.txt").unwrap();
        assert_eq!(recovered, plain);
    }

    #[test]
    fn unpack_legacy_xor_still_works_with_allow_policy() {
        let plain = b"legacy blob";
        let xored = crypt_bytes(plain, "pw", "legacy.txt");
        let recovered = unpack_bytes(&xored, "pw", "legacy.txt").unwrap();
        assert_eq!(recovered, plain);
    }

    #[test]
    fn unpack_rejects_legacy_when_policy_reject() {
        let plain = b"legacy blob";
        let xored = crypt_bytes(plain, "pw", "legacy.txt");
        let err =
            unpack_bytes_with_policy(&xored, "pw", "legacy.txt", LegacyPolicy::Reject).unwrap_err();
        assert!(err.to_string().contains("legacy"));
    }

    #[test]
    fn pack_bytes_different_paths_differ() {
        let a = pack_bytes(b"x", "pw", "a.txt").unwrap();
        let b = pack_bytes(b"x", "pw", "b.txt").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn crypt_bytes_domain_separation_prevents_collision() {
        let pw_ab = "ab";
        let path_cdef = "cdef";
        let pw_abc = "abc";
        let path_def = "def";

        let ks1 = crypt_bytes(b"payload", pw_ab, path_cdef);
        let ks2 = crypt_bytes(b"payload", pw_abc, path_def);

        assert_ne!(
            ks1, ks2,
            "different password/path splits with same concatenation must differ"
        );
    }
}
