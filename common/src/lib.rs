//! Shared wire models, encrypted snapshot types, and AEAD primitives for FeanorFS.
//!
//! # Boundary
//!
//! This crate is the pure contract/models layer: it performs no filesystem or
//! network I/O. All hashing of on-disk content happens at the
//! descriptor-anchored engine boundary (`feanorfs-agent-core`), never here.
//!
//! # Persisted size conversions
//!
//! `file_size_from_db` / `file_size_to_db` convert between the signed 64-bit
//! SQLite `INTEGER` representation and native `u64`. These helpers are internal
//! persistence plumbing with no external (out-of-workspace) consumer, so the
//! legacy saturating behavior was removed rather than deprecated: a saturated
//! value could reach allocation, download, or manifest limits as an enormous
//! valid-looking size. Both helpers now perform checked conversion and return
//! [`SizeConversionError`] — `CorruptMetadata` for negative stored sizes and
//! `UnsupportedSize` for native sizes above `i64::MAX` — so persistence/server
//! paths fail with a typed error instead of persisting or surfacing a
//! saturated size.
//!
//! The former `hash_file` helper (filesystem I/O) was removed; no consumer
//! existed in the workspace.

pub mod agent_contract;
pub mod hub_contract;
pub mod integrator_contract;
pub mod invite;
pub mod mesh_contract;
pub mod resolution_contract;
pub mod sealed_envelope;
pub mod sync_delta;
pub mod three_way;
pub mod tray_contract;
pub mod tree;
mod tree_codec;
mod tree_convert;
mod tree_diff;
pub mod work_contract;

pub use agent_contract::{
    encode_agent_message, parse_agent_message, AgentCleanResult, AgentInboxQuery, AgentInboxResult,
    AgentListEntry, AgentListOfflineResult, AgentListResult, AgentMessage, AgentMessageInput,
    AgentMessageKind, AgentMessagePayload, AgentSendResult, ContinuousAgentStatus,
    ContinuousAttention, ContinuousPhase, LogEntry, LogResult, SpawnResult, UndoResult,
    AGENT_INBOX_DEFAULT_LIMIT, AGENT_INBOX_MAX_LIMIT, AGENT_MESSAGE_DISCRIMINATOR,
    AGENT_MESSAGE_MAX_BODY_BYTES, AGENT_MESSAGE_MAX_ENCODED_BYTES, AGENT_NAME_MAX_BYTES,
    CONTINUOUS_STATUS_SCHEMA_VERSION,
};
pub use invite::{
    decode_hub_invite, decode_invite, encode_hub_invite, encode_invite, hub_ca_fingerprint,
    hub_mdns_hostname, looks_like_hub_invite, looks_like_invite, HubInvite, RelayConfig,
    WorkspaceInvite, HUB_INVITE_PREFIX, HUB_MDNS_SERVICE, INVITE_PREFIX,
};
pub use mesh_contract::{
    MeshCandidate, MeshCandidateKind, MeshConfig, MeshTransport, NodeId, MAX_MESH_CANDIDATES,
    MESH_CAPABILITY_VERSION,
};
pub use tray_contract::{
    ConflictKeepResult, ConflictShowResult, ContinuousHealth, RecentWorkspaceEntry,
    RecentWorkspacesResult, ResolutionHealth, TrayAgentEntry, TrayAgentsSummary, TrayConflictEntry,
    TrayOverviewResult, TrayPauseResult, TrayStatusResult, WorkerStatusSnapshot,
};

pub use integrator_contract::{
    encode_integrator_profile, filter_eligible, generate_assignment_id, generate_selection_nonce,
    is_valid_agent_name, is_valid_hex_id, normalize_capabilities, normalize_capability,
    parse_integrator_profile, rank_candidates, roster_fingerprint, validate_integrator_digest,
    validate_path_list, ConflictMaterializeEntry, ConflictMaterializeInput,
    ConflictMaterializeResult, EligibilityResult, IntegratorAssignInput, IntegratorAssignResult,
    IntegratorAssignmentState, IntegratorAttempt, IntegratorAttemptState, IntegratorAttemptStatus,
    IntegratorCandidate, IntegratorDigest, IntegratorDraw, IntegratorObserveInput,
    IntegratorObserveResult, IntegratorOutcomeState, IntegratorProfile, IntegratorStatusResult,
    VerificationStatus, VerificationSummary, INTEGRATOR_ALGORITHM_VERSION,
    INTEGRATOR_CAPABILITY_MAX_BYTES, INTEGRATOR_DEFAULT_ACK_TIMEOUT_MS,
    INTEGRATOR_DIGEST_FIELD_BYTES, INTEGRATOR_MAX_AUTHORS, INTEGRATOR_MAX_CANDIDATES,
    INTEGRATOR_MAX_CAPABILITIES, INTEGRATOR_MAX_HISTORY, INTEGRATOR_MAX_PATHS,
    INTEGRATOR_MAX_PATH_BYTES, INTEGRATOR_MAX_RISKS, INTEGRATOR_MAX_TASK_SUMMARY_BYTES,
    INTEGRATOR_PROFILE_DISCRIMINATOR, INTEGRATOR_RISK_BYTES,
};

pub use hub_contract::{
    is_supported_format_version, parse_migration_token, validate_manifest_hashes,
    ManifestWriteOutcome, MigrationTokenError, MigrationWriteOutcome, SUPPORTED_FORMAT_VERSION,
};
pub use resolution_contract::{
    compute_conflict_identity_fingerprint, derive_conflict_kind, encode_resolution_profile,
    parse_resolution_profile, validate_candidate_descriptor, validate_conflict_identity,
    validate_designation_evidence, validate_human_resolution_answer, validate_resolution_job,
    validate_resolution_profile, validate_resolution_result, ArtifactDescriptor, ArtifactRoleName,
    CandidateDescriptor, CandidateDestination, ConflictIdentity, ConflictLegDescriptor,
    HumanResolutionAnswer, HumanResolutionOption, HumanResolutionReason, OwnerDesignationEvidence,
    OwnerDesignationMethod, PreventionReason, ResolutionJob, ResolutionOutcome, ResolutionProfile,
    ResolutionResult, ResolutionRevokeReason, ResolutionStaleKind, VerificationPolicyRef,
    RESOLUTION_DEFAULT_VERIFICATION_TIMEOUT_MS, RESOLUTION_DIAGNOSTIC_BYTES,
    RESOLUTION_FINGERPRINT_DOMAIN, RESOLUTION_JOB_DISCRIMINATOR, RESOLUTION_MAX_ADDITIONAL_PATHS,
    RESOLUTION_MAX_ARTIFACTS, RESOLUTION_MAX_ATTEMPT, RESOLUTION_MAX_CANDIDATE_BYTES,
    RESOLUTION_MAX_CAUSAL_REFS, RESOLUTION_MAX_DIAGNOSTICS, RESOLUTION_MAX_HASH_BYTES,
    RESOLUTION_MAX_ID_BYTES, RESOLUTION_MAX_INTENT_IDS, RESOLUTION_MAX_LEG_BYTES,
    RESOLUTION_MAX_OUTPUT_PATHS, RESOLUTION_MAX_PROFILE_BYTES, RESOLUTION_MAX_QUESTION_BYTES,
    RESOLUTION_MAX_REASON_BYTES, RESOLUTION_MAX_TIMEOUT_MS,
    RESOLUTION_MAX_VERIFICATION_SUMMARY_BYTES, RESOLUTION_SCHEMA_VERSION,
    RESOLUTION_VERIFICATION_CONFIG_REF, RESOLUTION_VERIFICATION_POLICY_ID,
};
pub use resolution_contract::{ResolutionAssignmentProfile, ResolutionRevokeProfile};
pub use sync_delta::compute_sync_delta;
pub use three_way::{classify_conflict_kind, conflict_candidate_paths, detect_concurrent_edits};
pub use tree::{
    ConflictModes, Snapshot, Tree, TreeBundle, TreeChange, TreeChangeKind, TreeEntry,
    TreeEntryKind, EXECUTABLE_MODE, MAX_CANONICAL_OBJECT_BYTES, MAX_ENCRYPTED_OBJECT_BYTES,
    MAX_SNAPSHOT_AUTHOR_BYTES, MAX_SNAPSHOT_MESSAGE_BYTES, MAX_SNAPSHOT_PARENTS, MAX_TREE_DEPTH,
    MAX_TREE_ENTRIES, MAX_TREE_OBJECTS, MAX_TREE_OUTPUT_PATHS, MAX_TREE_PATH_BYTES_TOTAL,
    MAX_TREE_WORK_ITEMS,
};
pub use tree_convert::{flat_to_tree, flat_to_tree_with_conflicts, tree_to_flat};
pub use tree_diff::diff_trees;
pub use work_contract::{
    encode_work_profile, evaluate_scope_overlap, is_directory_glob, is_valid_capability,
    is_valid_scope_entry, is_valid_task_id, parse_work_profile, transition_rejection,
    validate_work_amendment, validate_work_blocked, validate_work_completed,
    validate_work_decision, validate_work_intent, validate_work_profile, validate_work_scope,
    validate_work_settled, validate_work_superseded, validate_work_yield, WorkAmendInput,
    WorkAmendmentProfile, WorkAmendmentStatus, WorkBlockInput, WorkBlockedProfile,
    WorkCompleteInput, WorkCompletedProfile, WorkDecideInput, WorkDecisionAccept,
    WorkDecisionAcceptOverlap, WorkDecisionKind, WorkDecisionNarrow, WorkDecisionOrder,
    WorkDecisionProfile, WorkDecisionReject, WorkDecisionStatus, WorkIntentProfile, WorkOverlap,
    WorkOverlapAcceptance, WorkOverlapKind, WorkProfile, WorkProposalStatus, WorkProposeInput,
    WorkRejectReason, WorkScope, WorkSendResult, WorkSettleInput, WorkSettledProfile,
    WorkStatusInput, WorkStatusResult, WorkSupersededProfile, WorkTaskState, WorkTaskStatus,
    WorkVerification, WorkVerificationStatus, WorkYieldInput, WorkYieldProfile,
    WORK_MAX_ACTIVE_TASKS, WORK_MAX_AMENDMENTS, WORK_MAX_CAPABILITIES, WORK_MAX_CAPABILITY_BYTES,
    WORK_MAX_CONCERNS, WORK_MAX_CONCERN_BYTES, WORK_MAX_DEPENDENCIES, WORK_MAX_EVIDENCE,
    WORK_MAX_OUTCOME_BYTES, WORK_MAX_OVERLAP_ENTRIES, WORK_MAX_PATHS, WORK_MAX_PATHS_TOTAL_BYTES,
    WORK_MAX_PATH_BYTES, WORK_MAX_PENDING, WORK_MAX_PROFILE_BYTES, WORK_MAX_PROJECTION_ENTRIES,
    WORK_MAX_PROPOSALS_PER_TASK, WORK_MAX_REASON_BYTES, WORK_MAX_SEEN, WORK_MAX_SOURCE_REFS,
    WORK_MAX_TASK_ID_BYTES, WORK_MAX_TERMINAL_TASKS, WORK_MAX_VERIFICATION_SUMMARY_BYTES,
    WORK_PROFILE_DISCRIMINATOR, WORK_SCHEMA_VERSION,
};

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

/// Insecure legacy default password used when no E2EE password is configured.
/// Kept as a single constant so all call sites share the same fallback.
pub const LEGACY_DEFAULT_PASSWORD: &str = "default-secret-key";

/// Generates a cryptographically random 64-char hex password.
/// Uses getrandom (CSPRNG) for entropy, then Blake3-hashes the bytes
/// to produce a stable-length hex string suitable as an E2EE key.
pub fn generate_password() -> Result<String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| anyhow::anyhow!("Failed to generate random bytes: {e}"))?;
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
///
/// `wait_supported` is `true` only when the response was produced by a hub
/// that honored bounded head-wait query parameters (`after`/`wait_ms`). Hubs
/// that ignore those parameters return the field as `false` (JSON default),
/// which clients use to select the bounded-polling compatibility fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadResponse {
    pub snapshot_id: Option<String>,
    /// Whether the hub supports bounded opaque head-change waiting.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wait_supported: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// Bounded live continuous-reconciliation projection; present only while
    /// an active controller owns this agent (SDK-1 additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<crate::agent_contract::ContinuousAgentStatus>,
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

/// Error returned when a persisted file size cannot be represented exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeConversionError {
    /// A negative size read from storage — indicates corrupt metadata.
    CorruptMetadata { size: i64 },
    /// A size too large for the storage representation — cannot be persisted.
    UnsupportedSize { size: u64 },
}

impl std::fmt::Display for SizeConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SizeConversionError::CorruptMetadata { size } => {
                write!(f, "corrupt metadata: stored file size {size} is negative")
            }
            SizeConversionError::UnsupportedSize { size } => write!(
                f,
                "unsupported file size {size}: exceeds the maximum representable by the storage format"
            ),
        }
    }
}

impl std::error::Error for SizeConversionError {}

/// Convert file size from SQLite i64 to native u64.
///
/// Returns [`SizeConversionError::CorruptMetadata`] for negative sizes instead
/// of saturating to `u64::MAX`, so a corrupted row can never surface as an
/// enormous valid-looking size that would drive allocation, download, or
/// manifest limits.
pub fn file_size_from_db(size: i64) -> Result<u64, SizeConversionError> {
    u64::try_from(size).map_err(|_| SizeConversionError::CorruptMetadata { size })
}

/// Convert file size from native u64 to SQLite i64.
///
/// Returns [`SizeConversionError::UnsupportedSize`] for sizes above `i64::MAX`
/// instead of saturating to `i64::MAX`, so an oversized file is rejected at
/// the persistence boundary rather than silently stored as `i64::MAX`.
pub fn file_size_to_db(size: u64) -> Result<i64, SizeConversionError> {
    i64::try_from(size).map_err(|_| SizeConversionError::UnsupportedSize { size })
}

/// Normalizes a path to use forward slashes for cross-platform consistency.
#[must_use]
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn is_windows_reserved_component(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
            | "CONIN$"
            | "CONOUT$"
    )
}

/// Maximum UTF-8 byte length of one portable path component.
pub const MAX_PORTABLE_COMPONENT_BYTES: usize = 255;
/// Maximum UTF-8 byte length of one complete portable workspace-relative path.
pub const MAX_PORTABLE_PATH_BYTES: usize = 4_096;

/// Returns true when `path` is one canonical, portable workspace-relative path.
#[must_use]
pub fn is_safe_rel_path(path: &str) -> bool {
    if path.is_empty()
        || path.len() > MAX_PORTABLE_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || !path.nfc().eq(path.chars())
    {
        return false;
    }
    path.split('/').all(|component| {
        !component.is_empty()
            && component.len() <= MAX_PORTABLE_COMPONENT_BYTES
            && component != "."
            && component != ".."
            && !component.eq_ignore_ascii_case(".feanorfs")
            && !component.eq_ignore_ascii_case(".git")
            && !component.eq_ignore_ascii_case(".jj")
            && !component.ends_with([' ', '.'])
            && !component
                .chars()
                .any(|character| character.is_control() || r#"<>:"|?*"#.contains(character))
            && !is_windows_reserved_component(component)
    })
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
    let nonce_ref: &Nonce = (&nonce).into();
    let ciphertext = cipher
        .encrypt(nonce_ref, data)
        .map_err(|e| anyhow::anyhow!("AEAD encrypt failed: {e}"))?;
    let mut out = Vec::with_capacity(1 + 12 + ciphertext.len());
    out.push(AEAD_PREFIX_BYTE);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts packed blob (ChaCha20-Poly1305 or legacy XOR per policy).
pub fn unpack_bytes(data: &[u8], password: &str, path: &str) -> Result<Vec<u8>> {
    unpack_bytes_checked(data, password, path, LegacyPolicy::AllowXorFallback, None)
}

/// Decrypt with an explicit legacy-blob policy (format v2 uses `Reject`).
pub fn unpack_bytes_with_policy(
    data: &[u8],
    password: &str,
    path: &str,
    policy: LegacyPolicy,
) -> Result<Vec<u8>> {
    unpack_bytes_checked(data, password, path, policy, None)
}

/// Decrypt with an explicit legacy-blob policy and an optional expected
/// plaintext size.
///
/// The expected size disambiguates the dangerous overlap between two blob
/// shapes: a legacy v1 blob whose first plaintext byte collides with the
/// AEAD prefix byte (ciphertext length == plaintext length, XOR rescue is
/// correct) versus an AEAD blob whose authentication FAILED because of a
/// wrong key or corruption (length carries the nonce+tag overhead, XOR
/// "rescue" yields silent garbage). When the caller knows the size and the
/// blob cannot be a length-matched legacy collision, authentication failure
/// is reported instead of returning unauthenticated output. Pass `None`
/// only where no size expectation exists.
pub fn unpack_bytes_checked(
    data: &[u8],
    password: &str,
    path: &str,
    policy: LegacyPolicy,
    expected_plaintext_size: Option<u64>,
) -> Result<Vec<u8>> {
    if data.first() == Some(&AEAD_PREFIX_BYTE) && data.len() > 13 {
        use chacha20poly1305::aead::{Aead, KeyInit};
        use chacha20poly1305::{ChaCha20Poly1305, Nonce};

        let key = derive_crypto_key(password, path);
        let cipher = ChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
        let nonce: &Nonce = data[1..13].try_into().expect("12-byte nonce");
        match cipher.decrypt(nonce, &data[13..]) {
            Ok(plain) => return Ok(plain),
            Err(_) if policy == LegacyPolicy::AllowXorFallback => {
                // A genuine prefix-collision legacy blob is exactly its own
                // plaintext length; anything else is AEAD-shaped and must
                // not degrade into unauthenticated output.
                if let Some(expected) = expected_plaintext_size {
                    if data.len() as u64 != expected {
                        anyhow::bail!(
                            "wrong encryption key for this workspace (decryption failed)"
                        );
                    }
                }
                return Ok(crypt_bytes(data, password, path));
            }
            Err(_) => {
                anyhow::bail!("wrong encryption key for this workspace (decryption failed)");
            }
        }
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

/// Maximum newline-delimited object identifiers accepted in one manifest.
/// This bounds validation memory and missing-blob filesystem probes even when
/// the byte-level request limit would permit a duplicate-dense body.
pub const MANIFEST_MAX_ENTRIES: usize = 250_000;

/// Validates a reachability manifest and returns its canonical sorted object set.
/// The snapshot object that names the manifest must be part of its own closure.
///
/// # Errors
/// Returns an error for an invalid snapshot id, invalid object id, or rootless
/// manifest.
/// Validates and canonicalizes an already-split reachability manifest without
/// first allocating a duplicate newline-delimited body.
///
/// # Errors
/// Returns an error for excessive entries, invalid ids, or a missing root.
pub fn canonical_manifest_hash_list(snapshot_id: &str, hashes: &[String]) -> Result<Vec<String>> {
    ensure!(
        is_valid_hash(snapshot_id),
        "invalid snapshot id for manifest"
    );
    ensure!(
        hashes.len() <= MANIFEST_MAX_ENTRIES,
        "manifest exceeds {MANIFEST_MAX_ENTRIES} object entries"
    );
    ensure!(
        hashes.iter().all(|hash| is_valid_hash(hash)),
        "manifest contains invalid object id"
    );
    ensure!(
        hashes.iter().any(|hash| hash == snapshot_id),
        "manifest does not contain its snapshot root"
    );
    let mut canonical = hashes.to_vec();
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

pub fn canonical_manifest_hashes(snapshot_id: &str, manifest: &str) -> Result<Vec<String>> {
    ensure!(
        is_valid_hash(snapshot_id),
        "invalid snapshot id for manifest"
    );
    // Sort borrowed slices first so duplicate-dense hostile bodies do not
    // allocate one owned String per repeated line. Bound raw entries as well
    // as unique hashes because both validation and blob-presence probes cost
    // work even when the manifest deduplicates to a tiny closure.
    let mut hashes =
        Vec::with_capacity((manifest.len() / 65).min(MANIFEST_MAX_ENTRIES.saturating_add(1)));
    for hash in manifest.lines() {
        ensure!(
            hashes.len() < MANIFEST_MAX_ENTRIES,
            "manifest exceeds {MANIFEST_MAX_ENTRIES} object entries"
        );
        ensure!(is_valid_hash(hash), "manifest contains invalid object id");
        hashes.push(hash);
    }
    hashes.sort_unstable();
    hashes.dedup();
    ensure!(
        hashes.binary_search(&snapshot_id).is_ok(),
        "manifest does not contain its snapshot root"
    );
    Ok(hashes.into_iter().map(str::to_string).collect())
}

#[cfg(test)]
pub use sealed_envelope::{open, seal, EnvelopeDomain, SealError, SealedEnvelope};

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

    #[test]
    fn is_safe_rel_path_requires_one_canonical_spelling() {
        for path in [
            ".",
            "./src/main.rs",
            "src/./main.rs",
            "src//main.rs",
            "src/main.rs/",
            r"src\main.rs",
            "docs/re\u{301}sume\u{301}.txt",
        ] {
            assert!(!is_safe_rel_path(path), "accepted non-canonical {path:?}");
        }
        assert!(is_safe_rel_path("src/main.rs"));
        assert!(is_safe_rel_path("docs/résumé.txt"));
    }

    #[test]
    fn is_safe_rel_path_rejects_control_metadata_on_every_platform() {
        for path in [
            ".git/config",
            "nested/.GIT/config",
            ".jj/repo/store",
            "nested/.Jj/repo/store",
            ".feanorfs/config.json",
            "nested/.FEANORFS/config.json",
        ] {
            assert!(!is_safe_rel_path(path), "accepted control path {path:?}");
        }
    }

    #[test]
    fn is_safe_rel_path_rejects_windows_absolute_alias_and_device_paths() {
        for path in [
            "C:/Windows/win.ini",
            r"C:\Windows\win.ini",
            "//server/share/file",
            r"\\server\share\file",
            "file.txt:stream",
            "NUL",
            "aux.txt",
            "tools/LPT9.log",
            "COM¹",
            "COM².txt",
            "COM³",
            "LPT¹",
            "LPT².log",
            "LPT³",
            "trailing-dot.",
            "trailing-space ",
            "bad?.txt",
            "bad\0name",
        ] {
            assert!(!is_safe_rel_path(path), "accepted Windows alias {path:?}");
        }
    }

    #[test]
    fn is_safe_rel_path_enforces_portable_byte_bounds() {
        assert!(is_safe_rel_path(&"a".repeat(MAX_PORTABLE_COMPONENT_BYTES)));
        assert!(!is_safe_rel_path(
            &"a".repeat(MAX_PORTABLE_COMPONENT_BYTES + 1)
        ));
        assert!(is_safe_rel_path(&vec!["a"; 2_048].join("/")));
        assert!(!is_safe_rel_path(&vec!["a"; 2_049].join("/")));
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
    fn manifest_entry_count_is_bounded_before_owned_hash_allocation() {
        let root = "a".repeat(64);
        let manifest = format!("{}\n", root).repeat(MANIFEST_MAX_ENTRIES + 1);
        let error = canonical_manifest_hashes(&root, &manifest).unwrap_err();
        assert!(error.to_string().contains("object entries"));
    }

    #[test]
    fn manifest_requires_its_snapshot_root() {
        let root = "a".repeat(64);
        assert!(canonical_manifest_hashes(&root, "").is_err());
        assert!(canonical_manifest_hashes(&root, &format!("{}\n", "b".repeat(64))).is_err());
    }

    #[test]
    fn manifest_hashes_are_validated_sorted_and_deduplicated() {
        let root = "a".repeat(64);
        let other = "b".repeat(64);
        let manifest = format!("{other}\n{root}\n{other}\n");
        assert_eq!(
            canonical_manifest_hashes(&root, &manifest).unwrap(),
            vec![root, other]
        );
        assert!(canonical_manifest_hashes(&"z".repeat(64), &manifest).is_err());
        assert!(canonical_manifest_hashes(&"a".repeat(64), "not-a-hash\n").is_err());
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

    // Checked size conversions

    #[test]
    fn file_size_from_db_accepts_valid_sizes() {
        assert_eq!(file_size_from_db(0).unwrap(), 0);
        assert_eq!(file_size_from_db(1).unwrap(), 1);
        assert_eq!(file_size_from_db(i64::MAX).unwrap(), i64::MAX as u64);
    }

    #[test]
    fn file_size_from_db_rejects_negative_without_saturating() {
        // A negative stored size is corrupt metadata: it must fail with a typed
        // error instead of saturating to u64::MAX and looking like an enormous
        // file that would drive allocation/download/manifest limits.
        let error = file_size_from_db(-1).unwrap_err();
        assert_eq!(error, SizeConversionError::CorruptMetadata { size: -1 });
        assert!(error.to_string().contains("corrupt metadata"));
        let min_error = file_size_from_db(i64::MIN).unwrap_err();
        assert_eq!(
            min_error,
            SizeConversionError::CorruptMetadata { size: i64::MIN }
        );
    }

    #[test]
    fn file_size_to_db_accepts_valid_sizes() {
        assert_eq!(file_size_to_db(0).unwrap(), 0);
        assert_eq!(file_size_to_db(i64::MAX as u64).unwrap(), i64::MAX);
    }

    #[test]
    fn file_size_to_db_rejects_oversized_without_saturating() {
        // A size above i64::MAX cannot be stored in SQLite's signed INTEGER: it
        // must fail with a typed error instead of saturating to i64::MAX.
        let error = file_size_to_db(i64::MAX as u64 + 1).unwrap_err();
        assert_eq!(
            error,
            SizeConversionError::UnsupportedSize {
                size: i64::MAX as u64 + 1
            }
        );
        assert!(error.to_string().contains("unsupported file size"));
        let max_error = file_size_to_db(u64::MAX).unwrap_err();
        assert_eq!(
            max_error,
            SizeConversionError::UnsupportedSize { size: u64::MAX }
        );
    }

    #[test]
    fn file_size_conversions_roundtrip() {
        for size in [0u64, 1, 4_096, u32::MAX as u64, i64::MAX as u64] {
            assert_eq!(
                file_size_from_db(file_size_to_db(size).unwrap()).unwrap(),
                size
            );
        }
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
    fn legacy_xor_with_aead_prefix_byte_still_migrates() {
        let password = "legacy-password";
        let path = "legacy.bin";
        let first_keystream_byte = crypt_bytes(&[0], password, path)[0];
        let mut plaintext = vec![0x5a; 32];
        plaintext[0] = first_keystream_byte ^ AEAD_PREFIX_BYTE;
        let ciphertext = crypt_bytes(&plaintext, password, path);
        assert_eq!(ciphertext[0], AEAD_PREFIX_BYTE);
        assert_eq!(
            unpack_bytes_with_policy(&ciphertext, password, path, LegacyPolicy::AllowXorFallback)
                .unwrap(),
            plaintext
        );
        assert!(
            unpack_bytes_with_policy(&ciphertext, password, path, LegacyPolicy::Reject).is_err()
        );
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
    fn wrong_key_on_aead_blob_fails_when_plaintext_size_known() {
        let plain = vec![0x5a_u8; 64];
        let packed = pack_bytes(&plain, "key-a", "blob.bin").unwrap();
        assert_eq!(packed.first(), Some(&AEAD_PREFIX_BYTE));
        let err = unpack_bytes_checked(
            &packed,
            "key-b",
            "blob.bin",
            LegacyPolicy::AllowXorFallback,
            Some(plain.len() as u64),
        )
        .unwrap_err();
        assert!(err.to_string().contains("wrong encryption key"));
    }

    #[test]
    fn legacy_prefix_collision_rescues_when_size_matches() {
        let password = "legacy-password";
        let path = "legacy.bin";
        let first_keystream_byte = crypt_bytes(&[0], password, path)[0];
        let mut plaintext = vec![0x5a_u8; 32];
        plaintext[0] = first_keystream_byte ^ AEAD_PREFIX_BYTE;
        let ciphertext = crypt_bytes(&plaintext, password, path);
        assert_eq!(
            unpack_bytes_checked(
                &ciphertext,
                password,
                path,
                LegacyPolicy::AllowXorFallback,
                Some(plaintext.len() as u64),
            )
            .unwrap(),
            plaintext
        );
    }

    #[test]
    fn unknown_size_preserves_documented_legacy_fallback() {
        let plain = vec![0x5a_u8; 64];
        let packed = pack_bytes(&plain, "key-a", "blob.bin").unwrap();
        let recovered = unpack_bytes_checked(
            &packed,
            "key-b",
            "blob.bin",
            LegacyPolicy::AllowXorFallback,
            None,
        )
        .unwrap();
        assert_eq!(recovered.len(), packed.len());
    }

    #[test]
    fn size_mismatch_fails_even_for_legacy_shaped_blob() {
        let password = "legacy-password";
        let path = "legacy.bin";
        let first_keystream_byte = crypt_bytes(&[0], password, path)[0];
        let mut plaintext = vec![0x5a_u8; 32];
        plaintext[0] = first_keystream_byte ^ AEAD_PREFIX_BYTE;
        let ciphertext = crypt_bytes(&plaintext, password, path);
        let err = unpack_bytes_checked(
            &ciphertext,
            password,
            path,
            LegacyPolicy::AllowXorFallback,
            Some(plaintext.len() as u64 + 1),
        )
        .unwrap_err();
        assert!(err.to_string().contains("wrong encryption key"));
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
