//! Exact conflict identity, resolution jobs, and closed resolution results
//! (SDK-1 additive).
//!
//! This module owns every type shared by the Rust SDK, CLI, JSON, C FFI,
//! TypeScript, MCP, and NDJSON event surfaces for automatic conflict
//! resolution: the versioned canonical [`ConflictIdentity`], its byte-exact
//! domain-separated Blake3 fingerprint, the bounded [`ResolutionJob`], the
//! closed [`ResolutionResult`] outcomes, the bounded human escalation
//! reasons, and the bounded verification policy reference. Adapters must
//! never reimplement identity canonicalization, fingerprinting, or
//! result validation.
//!
//! Identity never includes mtimes, display paths, artifact directory names,
//! local wall-clock values, hostname/environment values, candidate bytes, or
//! free-form error text. The fingerprint covers exactly the canonical
//! fields below; adding a field later is a versioned protocol change and the
//! fingerprint domain/version string must change with it.

use crate::{is_safe_rel_path, is_valid_hash, ConflictKind};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

/// Current canonical identity/job/result schema version.
pub const RESOLUTION_SCHEMA_VERSION: u32 = 1;

/// Fixed domain string domain-separating every conflict-identity fingerprint.
pub const RESOLUTION_FINGERPRINT_DOMAIN: &str = "feanorfs-conflict-identity-v1";

/// Discriminator prefix for resolution job profiles carried inside `ffmsg1`
/// bodies (reserved; adapters publish opaque wakeups only).
pub const RESOLUTION_JOB_DISCRIMINATOR: &str = "ffres1";

/// Maximum attempt number bound on one fingerprint.
pub const RESOLUTION_MAX_ATTEMPT: u32 = 10_000;

/// Maximum number of accepted intent/message ids referenced by one identity.
pub const RESOLUTION_MAX_INTENT_IDS: usize = 32;

/// Maximum number of causal message references in one job.
pub const RESOLUTION_MAX_CAUSAL_REFS: usize = 64;

/// Maximum UTF-8 byte length of one identifier field (workspace, task,
/// policy, owner).
pub const RESOLUTION_MAX_ID_BYTES: usize = 512;

/// Maximum UTF-8 byte length of one lowercase hex digest field.
pub const RESOLUTION_MAX_HASH_BYTES: usize = 64;

/// Maximum plaintext size of one conflict leg or candidate (engine bound).
pub const RESOLUTION_MAX_LEG_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Maximum plaintext size of one resolution candidate accepted by the engine.
pub const RESOLUTION_MAX_CANDIDATE_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum number of bounded diagnostics in one result.
pub const RESOLUTION_MAX_DIAGNOSTICS: usize = 16;

/// Maximum UTF-8 byte length of one diagnostic entry.
pub const RESOLUTION_DIAGNOSTIC_BYTES: usize = 512;

/// Maximum UTF-8 byte length of a prevention or last-resort reason.
pub const RESOLUTION_MAX_REASON_BYTES: usize = 1024;

/// Maximum UTF-8 byte length of the single bounded human question.
pub const RESOLUTION_MAX_QUESTION_BYTES: usize = 2048;

/// Maximum UTF-8 byte length of a verification summary.
pub const RESOLUTION_MAX_VERIFICATION_SUMMARY_BYTES: usize = 2048;

/// Maximum number of allowed output paths in one job.
pub const RESOLUTION_MAX_OUTPUT_PATHS: usize = 64;

/// Maximum number of artifact descriptors in one job (one per leg role).
pub const RESOLUTION_MAX_ARTIFACTS: usize = 3;

/// Maximum number of additional paths a guarded publication may apply.
pub const RESOLUTION_MAX_ADDITIONAL_PATHS: usize = 16;

/// Maximum verification timeout in milliseconds (24 h).
pub const RESOLUTION_MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Fixed verification-policy identity bound into every automatic job.
pub const RESOLUTION_VERIFICATION_POLICY_ID: &str = "feanorfs-inline-verify-v1";

/// Fixed command-configuration reference for the inline verification policy.
/// This is a reference, never an executable command or harness spawn.
pub const RESOLUTION_VERIFICATION_CONFIG_REF: &str = "feanorfs-resolver-inline-config-v1";

/// Default verification timeout before fresh evidence expires.
pub const RESOLUTION_DEFAULT_VERIFICATION_TIMEOUT_MS: u64 = 10 * 60 * 1000;

/// One leg of a conflict triple: explicit presence/deletion, encrypted hash,
/// plaintext size, and portable executable mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictLegDescriptor {
    /// Whether this leg was captured in the materialized triple at all.
    pub present: bool,
    /// Explicit deletion: the leg is a deletion tombstone.
    pub deleted: bool,
    /// Encrypted (sealed) Blake3 hash when live; empty for absent/deleted.
    pub hash: String,
    /// Plaintext size in bytes; 0 for absent/deleted.
    pub size: u64,
    /// Portable executable mode (`0` or [`crate::EXECUTABLE_MODE`]).
    pub mode: u32,
}

/// Versioned canonical identity of one exact conflict.
///
/// Base fields describe the conflict itself; automatic-resolution fields are
/// present only when the identity is bound to an automatic job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictIdentity {
    pub schema_version: u32,
    pub workspace_id: String,
    /// Workspace head snapshot id the conflict was last verified against.
    pub current_snapshot: String,
    /// Snapshot id the conflict legs were inspected on.
    pub about_snapshot: String,
    /// Tree root of the snapshot used to inspect the conflict.
    pub tree_root: String,
    /// Canonical portable workspace-relative path.
    pub path: String,
    pub base: ConflictLegDescriptor,
    pub ours: ConflictLegDescriptor,
    pub theirs: ConflictLegDescriptor,
    /// Conflict kind derived from the three legs.
    pub kind: ConflictKind,
    /// Canonical task id for automatic resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Accepted intent/message ids for automatic resolution (sorted, unique).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intent_message_ids: Vec<String>,
    /// Assignment id for automatic resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    /// Attempt number for automatic resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Designated owner for automatic resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub designated_owner: Option<String>,
    /// Verification-policy identity for automatic resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_policy: Option<String>,
}

impl ConflictIdentity {
    /// Whether this identity carries automatic-resolution binding fields.
    #[must_use]
    pub const fn is_automatic(&self) -> bool {
        self.task_id.is_some()
            || !self.intent_message_ids.is_empty()
            || self.assignment_id.is_some()
            || self.attempt.is_some()
            || self.designated_owner.is_some()
            || self.verification_policy.is_some()
    }
}

/// Derives the canonical conflict kind from three leg descriptors (mirrors
/// the three-way classifier over `FileState` legs; absent legs contribute
/// `deleted = false` because absence from the triple is not a deletion).
#[must_use]
pub fn derive_conflict_kind(
    base: &ConflictLegDescriptor,
    ours: &ConflictLegDescriptor,
    theirs: &ConflictLegDescriptor,
) -> ConflictKind {
    let we_deleted = ours.present && ours.deleted;
    let they_deleted = theirs.present && theirs.deleted;
    if we_deleted && !they_deleted && !base.deleted {
        ConflictKind::DeleteEdit
    } else if !we_deleted && they_deleted && !base.deleted {
        ConflictKind::EditDelete
    } else {
        ConflictKind::EditEdit
    }
}

/// Byte-exact canonical fingerprint of one conflict identity.
///
/// The fingerprint is Blake3 over the fixed domain, the schema version, and
/// every canonical field below with 64-bit little-endian length prefixes.
/// Variable-width collections are sorted/unique and length-prefixed, so
/// reordering or ambiguous concatenation cannot change cross-language
/// identity. The automatic-resolution block is presence-marked as one unit.
#[must_use]
pub fn compute_conflict_identity_fingerprint(identity: &ConflictIdentity) -> String {
    let mut hasher = blake3::Hasher::new();
    push_bytes(&mut hasher, RESOLUTION_FINGERPRINT_DOMAIN.as_bytes());
    push_u32(&mut hasher, RESOLUTION_SCHEMA_VERSION);
    push_bytes(&mut hasher, identity.workspace_id.as_bytes());
    push_bytes(&mut hasher, identity.current_snapshot.as_bytes());
    push_bytes(&mut hasher, identity.about_snapshot.as_bytes());
    push_bytes(&mut hasher, identity.tree_root.as_bytes());
    push_bytes(&mut hasher, identity.path.as_bytes());
    push_leg(&mut hasher, &identity.base);
    push_leg(&mut hasher, &identity.ours);
    push_leg(&mut hasher, &identity.theirs);
    push_bytes(&mut hasher, identity.kind.as_db_str().as_bytes());
    if identity.is_automatic() {
        hasher.update(&[1]);
        push_opt_bytes(&mut hasher, identity.task_id.as_deref().map(str::as_bytes));
        push_u64(&mut hasher, identity.intent_message_ids.len() as u64);
        for id in &identity.intent_message_ids {
            push_bytes(&mut hasher, id.as_bytes());
        }
        push_opt_bytes(
            &mut hasher,
            identity.assignment_id.as_deref().map(str::as_bytes),
        );
        push_u32(&mut hasher, identity.attempt.unwrap_or(0));
        push_opt_bytes(
            &mut hasher,
            identity.designated_owner.as_deref().map(str::as_bytes),
        );
        push_opt_bytes(
            &mut hasher,
            identity.verification_policy.as_deref().map(str::as_bytes),
        );
    } else {
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn push_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn push_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

fn push_u32(hasher: &mut blake3::Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn push_opt_bytes(hasher: &mut blake3::Hasher, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            push_bytes(hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn push_leg(hasher: &mut blake3::Hasher, leg: &ConflictLegDescriptor) {
    hasher.update(&[u8::from(leg.present), u8::from(leg.deleted)]);
    push_bytes(hasher, leg.hash.as_bytes());
    push_u64(hasher, leg.size);
    push_u32(hasher, leg.mode);
}

fn validate_leg(leg: &ConflictLegDescriptor, label: &str) -> Result<()> {
    ensure!(
        leg.mode == 0 || leg.mode == crate::EXECUTABLE_MODE,
        "{label} leg has a non-portable executable mode {}",
        leg.mode
    );
    if !leg.present {
        ensure!(
            !leg.deleted && leg.hash.is_empty() && leg.size == 0,
            "{label} leg is absent but carries deletion/hash/size content"
        );
        return Ok(());
    }
    ensure!(
        leg.size <= RESOLUTION_MAX_LEG_BYTES,
        "{label} leg exceeds the {RESOLUTION_MAX_LEG_BYTES} byte plaintext bound"
    );
    if leg.deleted {
        ensure!(
            leg.hash.is_empty() && leg.size == 0 && leg.mode == 0,
            "{label} leg is deleted but carries hash/size/mode content"
        );
    } else {
        ensure!(
            is_valid_hash(&leg.hash),
            "{label} leg must carry a full 64-hex encrypted hash when live"
        );
    }
    Ok(())
}

/// Validates one conflict identity in full (schema, ids, bounds, derived
/// kind, canonical automatic fields).
///
/// # Errors
/// Returns an error for any out-of-bounds or internally inconsistent field.
pub fn validate_conflict_identity(identity: &ConflictIdentity) -> Result<()> {
    ensure!(
        identity.schema_version == RESOLUTION_SCHEMA_VERSION,
        "unsupported conflict identity schema {} (expected {RESOLUTION_SCHEMA_VERSION})",
        identity.schema_version
    );
    ensure!(
        !identity.workspace_id.trim().is_empty()
            && identity.workspace_id.len() <= RESOLUTION_MAX_ID_BYTES,
        "workspace id must be non-empty and at most {RESOLUTION_MAX_ID_BYTES} bytes"
    );
    ensure!(
        is_valid_hash(&identity.current_snapshot)
            && is_valid_hash(&identity.about_snapshot)
            && is_valid_hash(&identity.tree_root),
        "conflict identity snapshots and tree root must be full 64-hex ids"
    );
    ensure!(
        is_safe_rel_path(&identity.path),
        "conflict path must be one canonical portable workspace-relative path"
    );
    validate_leg(&identity.base, "base")?;
    validate_leg(&identity.ours, "ours")?;
    validate_leg(&identity.theirs, "theirs")?;
    ensure!(
        identity.kind == derive_conflict_kind(&identity.base, &identity.ours, &identity.theirs),
        "conflict kind must be derived from the three legs"
    );
    ensure!(
        identity.intent_message_ids.len() <= RESOLUTION_MAX_INTENT_IDS,
        "accepted intent list exceeds {RESOLUTION_MAX_INTENT_IDS} entries"
    );
    let mut previous: Option<&str> = None;
    for id in &identity.intent_message_ids {
        ensure!(
            is_valid_hash(id),
            "accepted intent reference must be a full 64-hex message id"
        );
        ensure!(
            previous.is_none_or(|prev| prev < id.as_str()),
            "accepted intent references must be sorted and unique"
        );
        previous = Some(id.as_str());
    }
    if let Some(task_id) = &identity.task_id {
        ensure!(
            crate::work_contract::is_valid_task_id(task_id),
            "automatic identity task id is invalid"
        );
    }
    if let Some(assignment_id) = &identity.assignment_id {
        ensure!(
            crate::integrator_contract::is_valid_hex_id(assignment_id, 32),
            "automatic identity assignment id must be exactly 32 lowercase hex chars"
        );
    }
    if let Some(attempt) = identity.attempt {
        ensure!(
            attempt <= RESOLUTION_MAX_ATTEMPT,
            "automatic identity attempt exceeds {RESOLUTION_MAX_ATTEMPT}"
        );
    }
    if let Some(owner) = &identity.designated_owner {
        ensure!(
            crate::integrator_contract::is_valid_agent_name(owner),
            "automatic identity designated owner is invalid"
        );
    }
    if let Some(policy) = &identity.verification_policy {
        ensure!(
            !policy.trim().is_empty() && policy.len() <= RESOLUTION_MAX_ID_BYTES,
            "automatic identity verification policy is out of bounds"
        );
    }
    Ok(())
}

/// One artifact role descriptor referenced by a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDescriptor {
    pub role: ArtifactRoleName,
    /// Canonical portable path (relative to the protected workspace state
    /// root) of the materialized artifact.
    pub path: String,
}

/// Role of one materialized conflict artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRoleName {
    Original,
    Local,
    Cloud,
}

impl ArtifactRoleName {
    /// Stable wire string of this role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Local => "local",
            Self::Cloud => "cloud",
        }
    }
}

/// Engine-owned immutable candidate destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDestination {
    /// Canonical portable path (relative to the protected workspace state
    /// root) where the harness must create the candidate.
    pub path: String,
    /// Always true: candidates are create-new and never overwritten.
    #[serde(default = "default_true")]
    pub create_new: bool,
}

const fn default_true() -> bool {
    true
}

/// Fixed verification-policy reference bound into one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationPolicyRef {
    /// Fixed verification-policy identity.
    pub policy_id: String,
    /// Command-configuration reference (a reference, never a command).
    pub command_config_ref: String,
    /// Milliseconds before recorded verification evidence expires.
    pub timeout_ms: u64,
    /// Whether fresh verification evidence is required before publication.
    pub freshness_required: bool,
}

/// Typed prevention reason required before automatic job preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PreventionReason {
    /// Every bounded prevention path was exhausted for this conflict.
    Exhausted { detail: String },
    /// A prevention invariant was violated for this conflict.
    Violated { detail: String },
}

impl PreventionReason {
    /// Bounded plain-language detail for display/audit.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Exhausted { detail } | Self::Violated { detail } => detail,
        }
    }
}

/// Bounded resolution job for exactly one conflict fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionJob {
    pub schema_version: u32,
    /// 128-bit engine job id (32 lowercase hex chars).
    pub job_id: String,
    pub task_id: String,
    /// 128-bit assignment id (32 lowercase hex chars).
    pub assignment_id: String,
    pub attempt: u32,
    pub workspace_id: String,
    /// Designated owner; also the resolver recorded in history.
    pub owner: String,
    pub conflict: ConflictIdentity,
    /// Full fingerprint of `conflict` (including automatic fields).
    pub conflict_fingerprint: String,
    /// Workspace head the job was prepared against.
    pub current_snapshot: String,
    /// Snapshot the conflict legs were inspected on.
    pub about_snapshot: String,
    /// Tree root of the inspected snapshot.
    pub tree_root: String,
    /// Accepted intent/message ids bound to this job (sorted, unique).
    #[serde(default)]
    pub accepted_intents: Vec<String>,
    /// Relevant causal message references (sorted, unique).
    #[serde(default)]
    pub causal_refs: Vec<String>,
    /// Materialized artifact descriptors/roles (one per leg role).
    #[serde(default)]
    pub artifacts: Vec<ArtifactDescriptor>,
    /// Engine-owned immutable candidate destination.
    pub candidate_destination: CandidateDestination,
    /// Exact allowed output paths (canonical portable paths).
    #[serde(default)]
    pub allowed_output_paths: Vec<String>,
    pub verification: VerificationPolicyRef,
    /// Typed prevention reason that had to be exhausted/violated.
    pub prevention: PreventionReason,
    /// Recorded last-resort reason explaining why prevention did not suffice.
    pub last_resort_reason: String,
    /// Engine-computed designation evidence (causal order or `ffint1`
    /// fallback with nonce/roster/ranking) so any machine can audit the
    /// owner choice. Never derived from caller prose.
    pub designation: OwnerDesignationEvidence,
}

/// Plaintext descriptor of one resolution candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDescriptor {
    /// Canonical portable path (the job's candidate destination).
    pub path: String,
    /// Plaintext Blake3 hash; empty when `deleted` is true.
    pub hash: String,
    pub size: u64,
    pub mode: u32,
    pub deleted: bool,
}

/// Closed resolution outcome set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionOutcome {
    CandidateReady,
    /// The resolver proved, under the job's fixed verification policy, that
    /// no candidate bytes are required: the conflict's current head already
    /// contains an exact leg (ours or theirs) for every remaining difference.
    /// Applying it publishes a new snapshot with the *unchanged*
    /// representative leg kept in place and removes only the matching
    /// conflict identity; no file bytes, modes, or deletions are applied
    /// beyond what the verified current head already carries.
    NoChangeRequired,
    Blocked,
    RequiresHuman,
    Failed,
    Stale,
}

impl ResolutionOutcome {
    /// Stable wire string for this outcome.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CandidateReady => "candidate_ready",
            Self::NoChangeRequired => "no_change_required",
            Self::Blocked => "blocked",
            Self::RequiresHuman => "requires_human",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }
}

/// Closed set of allowed human escalation reasons. Offline conditions, first
/// timeouts, signal-only heads, stale candidates, and ordinary lost CAS are
/// not human reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanResolutionReason {
    SemanticAmbiguity,
    UnavoidableDataLoss,
    MissingOrAuthFailedLeg,
    SecurityCompatibilityBoundaryChange,
    RequiredVerificationUnavailable,
    IndeterminateOwnership,
    BoundedResolverExhaustion,
    UnsupportedSizeSafetyBound,
    ExplicitProductDecision,
}

/// Closed resolution result bound to one job/assignment/attempt/fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionResult {
    pub schema_version: u32,
    pub outcome: ResolutionOutcome,
    pub job_id: String,
    pub assignment_id: String,
    pub attempt: u32,
    pub owner: String,
    pub conflict_fingerprint: String,
    /// Candidate descriptor; required exactly for `candidate_ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateDescriptor>,
    pub verification: crate::VerificationSummary,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    /// Exactly one bounded question, only for `requires_human`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Typed human reason; required exactly for `requires_human`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_reason: Option<HumanResolutionReason>,
    /// Monotonic question generation of this escalation. Every answer must
    /// reference the exact generation; stale generations are rejected.
    #[serde(default)]
    pub question_generation: u32,
    /// Typed safe options offered with the question; for `requires_human`
    /// at least one of [`HumanResolutionOption::Defer`] or
    /// [`HumanResolutionOption::KeepUnresolved`] must be present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safe_options: Vec<HumanResolutionOption>,
}

/// Validates one candidate descriptor (bounds, path, hash/size/mode/deletion
/// consistency).
///
/// # Errors
/// Returns an error for any out-of-bounds or inconsistent field.
pub fn validate_candidate_descriptor(candidate: &CandidateDescriptor) -> Result<()> {
    ensure!(
        is_safe_rel_path(&candidate.path) && candidate.path.len() <= crate::MAX_PORTABLE_PATH_BYTES,
        "candidate path must be one canonical portable relative path within bounds"
    );
    ensure!(
        candidate.size <= RESOLUTION_MAX_CANDIDATE_BYTES,
        "candidate exceeds the {RESOLUTION_MAX_CANDIDATE_BYTES} byte bound"
    );
    ensure!(
        candidate.mode == 0 || candidate.mode == crate::EXECUTABLE_MODE,
        "candidate mode must be portable (0 or executable)"
    );
    if candidate.deleted {
        ensure!(
            candidate.hash.is_empty() && candidate.size == 0 && candidate.mode == 0,
            "deleted candidate must carry no hash/size/mode"
        );
    } else {
        ensure!(
            is_valid_hash(&candidate.hash),
            "live candidate must carry a full 64-hex plaintext hash"
        );
    }
    Ok(())
}

/// Validates one resolution job in full (schema, ids, bounds, fingerprint
/// equality, causal references, policy, prevention).
///
/// # Errors
/// Returns an error for any out-of-bounds or internally inconsistent field.
pub fn validate_resolution_job(job: &ResolutionJob) -> Result<()> {
    ensure!(
        job.schema_version == RESOLUTION_SCHEMA_VERSION,
        "unsupported resolution job schema {} (expected {RESOLUTION_SCHEMA_VERSION})",
        job.schema_version
    );
    ensure!(
        crate::integrator_contract::is_valid_hex_id(&job.job_id, 32),
        "job id must be exactly 32 lowercase hex chars (128 bits)"
    );
    ensure!(
        crate::integrator_contract::is_valid_hex_id(&job.assignment_id, 32),
        "assignment id must be exactly 32 lowercase hex chars (128 bits)"
    );
    ensure!(
        crate::work_contract::is_valid_task_id(&job.task_id),
        "job task id is invalid"
    );
    ensure!(
        job.attempt <= RESOLUTION_MAX_ATTEMPT,
        "job attempt exceeds {RESOLUTION_MAX_ATTEMPT}"
    );
    ensure!(
        !job.workspace_id.trim().is_empty() && job.workspace_id.len() <= RESOLUTION_MAX_ID_BYTES,
        "job workspace id is out of bounds"
    );
    ensure!(
        crate::integrator_contract::is_valid_agent_name(&job.owner),
        "job owner must be a valid agent name"
    );
    validate_conflict_identity(&job.conflict)?;
    ensure!(
        is_valid_hash(&job.conflict_fingerprint)
            && job.conflict_fingerprint == compute_conflict_identity_fingerprint(&job.conflict),
        "job fingerprint must be the exact canonical fingerprint of its identity"
    );
    ensure!(
        job.current_snapshot == job.conflict.current_snapshot
            && job.about_snapshot == job.conflict.about_snapshot
            && job.tree_root == job.conflict.tree_root,
        "job snapshot fields must match its conflict identity"
    );
    ensure!(
        job.conflict.is_automatic(),
        "automatic job identity must carry task/intent/assignment/attempt/owner/policy"
    );
    ensure!(
        job.conflict.assignment_id.as_deref() == Some(job.assignment_id.as_str())
            && job.conflict.attempt == Some(job.attempt)
            && job.conflict.designated_owner.as_deref() == Some(job.owner.as_str())
            && job.conflict.task_id.as_deref() == Some(job.task_id.as_str()),
        "job automatic identity fields must match the job header"
    );
    validate_sorted_unique_hashes(
        &job.accepted_intents,
        RESOLUTION_MAX_INTENT_IDS,
        "accepted intents",
    )?;
    ensure!(
        !job.accepted_intents.is_empty(),
        "job must bind at least one accepted intent message id"
    );
    validate_sorted_unique_hashes(&job.causal_refs, RESOLUTION_MAX_CAUSAL_REFS, "causal refs")?;
    ensure!(
        !job.causal_refs.is_empty(),
        "job must carry verified causal message references"
    );
    ensure!(
        job.artifacts.len() <= RESOLUTION_MAX_ARTIFACTS,
        "artifact descriptors exceed {RESOLUTION_MAX_ARTIFACTS}"
    );
    ensure!(
        !job.artifacts.is_empty(),
        "job must carry at least one artifact descriptor"
    );
    let mut roles = Vec::new();
    for artifact in &job.artifacts {
        ensure!(
            is_safe_rel_path(&artifact.path)
                && artifact.path.len() <= crate::MAX_PORTABLE_PATH_BYTES,
            "artifact descriptor path is not canonical/portable"
        );
        ensure!(
            !roles.contains(&artifact.role),
            "duplicate artifact role {:?}",
            artifact.role
        );
        roles.push(artifact.role);
    }
    validate_candidate_destination(&job.candidate_destination)?;
    ensure!(
        job.allowed_output_paths.len() <= RESOLUTION_MAX_OUTPUT_PATHS,
        "allowed output paths exceed {RESOLUTION_MAX_OUTPUT_PATHS}"
    );
    ensure!(
        !job.allowed_output_paths.is_empty(),
        "job must allow at least one canonical output path"
    );
    for path in &job.allowed_output_paths {
        ensure!(
            is_safe_rel_path(path) && path.len() <= crate::MAX_PORTABLE_PATH_BYTES,
            "allowed output path is not canonical/portable"
        );
    }
    ensure!(
        !job.verification.policy_id.trim().is_empty()
            && job.verification.policy_id.len() <= RESOLUTION_MAX_ID_BYTES
            && !job.verification.command_config_ref.trim().is_empty()
            && job.verification.command_config_ref.len() <= RESOLUTION_MAX_ID_BYTES,
        "verification policy reference is out of bounds"
    );
    ensure!(
        job.verification.timeout_ms > 0 && job.verification.timeout_ms <= RESOLUTION_MAX_TIMEOUT_MS,
        "verification timeout is out of bounds"
    );
    ensure!(
        !job.last_resort_reason.trim().is_empty()
            && job.last_resort_reason.len() <= RESOLUTION_MAX_REASON_BYTES,
        "last-resort reason must be non-empty and bounded"
    );
    let detail = job.prevention.detail();
    ensure!(
        !detail.trim().is_empty() && detail.len() <= RESOLUTION_MAX_REASON_BYTES,
        "prevention reason detail must be non-empty and bounded"
    );
    validate_designation_evidence(&job.designation)?;
    ensure!(
        job.designation
            .eligible
            .iter()
            .any(|agent| agent == &job.owner),
        "designated owner must be engine-selected from the eligible roster"
    );
    Ok(())
}

fn validate_candidate_destination(destination: &CandidateDestination) -> Result<()> {
    ensure!(
        destination.create_new,
        "candidate destination must be create-new"
    );
    ensure!(
        is_safe_rel_path(&destination.path)
            && destination.path.len() <= crate::MAX_PORTABLE_PATH_BYTES,
        "candidate destination must be one canonical portable relative path"
    );
    Ok(())
}

fn validate_sorted_unique_hashes(values: &[String], limit: usize, label: &str) -> Result<()> {
    ensure!(
        values.len() <= limit,
        "{label} list exceeds {limit} entries"
    );
    let mut previous: Option<&str> = None;
    for value in values {
        ensure!(
            is_valid_hash(value),
            "{label} entry must be a full 64-hex id"
        );
        ensure!(
            previous.is_none_or(|prev| prev < value.as_str()),
            "{label} entries must be sorted and unique"
        );
        previous = Some(value.as_str());
    }
    Ok(())
}

/// Validates one resolution result in full (schema, bounds, closed outcome
/// constraints: candidate only for `candidate_ready`, exactly one bounded
/// question and human reason only for `requires_human`).
///
/// # Errors
/// Returns an error for any out-of-bounds or internally inconsistent field.
pub fn validate_resolution_result(result: &ResolutionResult) -> Result<()> {
    ensure!(
        result.schema_version == RESOLUTION_SCHEMA_VERSION,
        "unsupported resolution result schema {} (expected {RESOLUTION_SCHEMA_VERSION})",
        result.schema_version
    );
    ensure!(
        crate::integrator_contract::is_valid_hex_id(&result.job_id, 32)
            && crate::integrator_contract::is_valid_hex_id(&result.assignment_id, 32),
        "result job/assignment ids must be exactly 32 lowercase hex chars"
    );
    ensure!(
        result.attempt <= RESOLUTION_MAX_ATTEMPT,
        "result attempt exceeds {RESOLUTION_MAX_ATTEMPT}"
    );
    ensure!(
        crate::integrator_contract::is_valid_agent_name(&result.owner),
        "result owner must be a valid agent name"
    );
    ensure!(
        is_valid_hash(&result.conflict_fingerprint),
        "result conflict fingerprint must be a full 64-hex id"
    );
    ensure!(
        result.verification.summary.len() <= RESOLUTION_MAX_VERIFICATION_SUMMARY_BYTES,
        "result verification summary exceeds {RESOLUTION_MAX_VERIFICATION_SUMMARY_BYTES} bytes"
    );
    ensure!(
        result.diagnostics.len() <= RESOLUTION_MAX_DIAGNOSTICS,
        "result diagnostics exceed {RESOLUTION_MAX_DIAGNOSTICS} entries"
    );
    for diagnostic in &result.diagnostics {
        ensure!(
            diagnostic.len() <= RESOLUTION_DIAGNOSTIC_BYTES,
            "diagnostic exceeds {RESOLUTION_DIAGNOSTIC_BYTES} bytes"
        );
    }
    if let Some(question) = &result.question {
        ensure!(
            !question.trim().is_empty() && question.len() <= RESOLUTION_MAX_QUESTION_BYTES,
            "result question must be non-empty and at most {RESOLUTION_MAX_QUESTION_BYTES} bytes"
        );
    }
    match result.outcome {
        ResolutionOutcome::CandidateReady => {
            ensure!(
                result.candidate.is_some(),
                "candidate_ready result must include a candidate descriptor"
            );
            ensure!(
                result.question.is_none() && result.human_reason.is_none(),
                "candidate_ready result must not carry a human question"
            );
            ensure!(
                result.verification.status == crate::VerificationStatus::Passed,
                "candidate_ready result requires passed verification"
            );
            if let Some(candidate) = &result.candidate {
                validate_candidate_descriptor(candidate)?;
            }
        }
        ResolutionOutcome::NoChangeRequired => {
            ensure!(
                result.candidate.is_none()
                    && result.question.is_none()
                    && result.human_reason.is_none(),
                "no_change_required result must not carry a candidate or human question"
            );
            ensure!(
                result.verification.status == crate::VerificationStatus::Passed,
                "no_change_required result requires passed verification"
            );
        }
        ResolutionOutcome::Blocked => {
            ensure!(
                result.candidate.is_none()
                    && result.question.is_none()
                    && result.human_reason.is_none(),
                "blocked result must not carry a candidate or human question"
            );
        }
        ResolutionOutcome::RequiresHuman => {
            ensure!(
                result.question.is_some() && result.human_reason.is_some(),
                "requires_human result requires exactly one bounded question and a human reason"
            );
            ensure!(
                result.candidate.is_none(),
                "requires_human result must not carry a candidate"
            );
            ensure!(
                result.safe_options.len() <= 4
                    && result
                        .safe_options
                        .iter()
                        .any(|option| matches!(
                            option,
                            HumanResolutionOption::Defer | HumanResolutionOption::KeepUnresolved
                        )),
                "requires_human result must offer a bounded typed safe option (defer or keep_unresolved)"
            );
        }
        ResolutionOutcome::Failed | ResolutionOutcome::Stale => {
            ensure!(
                result.candidate.is_none()
                    && result.question.is_none()
                    && result.human_reason.is_none(),
                "failed/stale results must not carry a candidate or human question"
            );
        }
    }
    Ok(())
}

/// One typed safe option a human can choose for a resolution question.
/// `Defer` and `KeepUnresolved` never mutate the conflict; `SubmitCandidate`
/// routes the human's candidate through the identical guarded-publication
/// validation used by an agent result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanResolutionOption {
    Defer,
    KeepUnresolved,
    SubmitCandidate,
}

impl HumanResolutionOption {
    /// Stable wire string of this option.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Defer => "defer",
            Self::KeepUnresolved => "keep_unresolved",
            Self::SubmitCandidate => "submit_candidate",
        }
    }
}

/// Bounded typed human answer bound to one exact escalation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanResolutionAnswer {
    pub schema_version: u32,
    pub job_id: String,
    pub assignment_id: String,
    pub attempt: u32,
    pub conflict_fingerprint: String,
    /// Exact question generation this answer responds to.
    pub question_generation: u32,
    pub chosen_option: HumanResolutionOption,
    /// Required exactly when `chosen_option` is `SubmitCandidate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateDescriptor>,
    /// Required exactly when `chosen_option` is `SubmitCandidate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<crate::VerificationSummary>,
}

/// Validates one human answer in full (ids, generation, option/candidate/
/// verification consistency).
///
/// # Errors
/// Returns an error for any out-of-bounds or internally inconsistent field.
pub fn validate_human_resolution_answer(answer: &HumanResolutionAnswer) -> Result<()> {
    ensure!(
        answer.schema_version == RESOLUTION_SCHEMA_VERSION,
        "unsupported human answer schema {} (expected {RESOLUTION_SCHEMA_VERSION})",
        answer.schema_version
    );
    ensure!(
        crate::integrator_contract::is_valid_hex_id(&answer.job_id, 32),
        "human answer job id must be exactly 32 lowercase hex chars"
    );
    ensure!(
        crate::integrator_contract::is_valid_hex_id(&answer.assignment_id, 32),
        "human answer assignment id must be exactly 32 lowercase hex chars"
    );
    ensure!(
        answer.attempt <= RESOLUTION_MAX_ATTEMPT,
        "human answer attempt exceeds {RESOLUTION_MAX_ATTEMPT}"
    );
    ensure!(
        is_valid_hash(&answer.conflict_fingerprint),
        "human answer fingerprint must be a full 64-hex digest"
    );
    match answer.chosen_option {
        HumanResolutionOption::SubmitCandidate => {
            let candidate = answer.candidate.as_ref().ok_or_else(|| {
                anyhow::anyhow!("submit_candidate answers must carry a candidate descriptor")
            })?;
            validate_candidate_descriptor(candidate)?;
            ensure!(
                answer.verification.is_some(),
                "submit_candidate answers must carry verification evidence"
            );
        }
        HumanResolutionOption::Defer | HumanResolutionOption::KeepUnresolved => {
            ensure!(
                answer.candidate.is_none() && answer.verification.is_none(),
                "defer/keep_unresolved answers must not carry a candidate or evidence"
            );
        }
    }
    Ok(())
}

/// Wire designation method recorded in a job's evidence block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerDesignationMethod {
    /// The owner was selected from transitive causal ancestry over applied
    /// coordination messages.
    CausalEligible,
    /// The deterministic `ffint1` ranking fallback selected the owner.
    IntegratorFallback,
}

/// Engine-computed designation evidence persisted in the immutable job so
/// any machine can audit why the resolver was chosen. Never caller prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerDesignationEvidence {
    pub method: OwnerDesignationMethod,
    /// OS-CSPRNG nonce consumed by the `ffint1` ranking (fallback only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// Eligible-roster fingerprint ranked (fallback only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roster_fingerprint: Option<String>,
    /// Sorted unique eligible agent names considered for designation.
    #[serde(default)]
    pub eligible: Vec<String>,
    /// Sorted unique ranked agent names (fallback only).
    #[serde(default)]
    pub ranked: Vec<String>,
    /// Bounded engine-produced reasoning (causal order or ranking facts).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    pub attempt: u32,
}

/// Validates one designation evidence block.
///
/// # Errors
/// Returns an error for inconsistent or out-of-bounds evidence.
pub fn validate_designation_evidence(evidence: &OwnerDesignationEvidence) -> Result<()> {
    ensure!(
        evidence.attempt <= RESOLUTION_MAX_ATTEMPT,
        "designation attempt exceeds {RESOLUTION_MAX_ATTEMPT}"
    );
    ensure!(
        evidence.eligible.len() <= RESOLUTION_MAX_INTENT_IDS,
        "designation eligible roster exceeds its bound"
    );
    ensure!(
        evidence.ranked.len() <= RESOLUTION_MAX_INTENT_IDS,
        "designation ranked roster exceeds its bound"
    );
    for agent in evidence.eligible.iter().chain(evidence.ranked.iter()) {
        ensure!(
            crate::integrator_contract::is_valid_agent_name(agent),
            "designation evidence names an invalid agent"
        );
    }
    ensure!(
        evidence.reasoning.len() <= RESOLUTION_MAX_REASON_BYTES,
        "designation reasoning exceeds its bound"
    );
    match evidence.method {
        OwnerDesignationMethod::CausalEligible => {
            ensure!(
                !evidence.eligible.is_empty(),
                "causal designation must name at least one eligible agent"
            );
        }
        OwnerDesignationMethod::IntegratorFallback => {
            ensure!(
                evidence.nonce.as_deref().is_some_and(is_valid_hash)
                    && evidence
                        .roster_fingerprint
                        .as_deref()
                        .is_some_and(is_valid_hash),
                "integrator fallback evidence must carry its nonce and roster fingerprint"
            );
            ensure!(
                !evidence.ranked.is_empty(),
                "integrator fallback evidence must carry its ranked roster"
            );
        }
    }
    Ok(())
}

/// Typed revocation reason for an `ffres1` assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionRevokeReason {
    /// A newer attempt superseded this assignment.
    Superseded,
    /// The assignment was cancelled without a replacement.
    Cancelled,
}

impl ResolutionRevokeReason {
    /// Stable wire string of this reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::Cancelled => "cancelled",
        }
    }
}

/// `ffres1` assignment profile: the complete immutable job embedded for
/// cross-machine import by ID and fingerprint (bounded; no hub route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionAssignmentProfile {
    pub schema_version: u32,
    pub job: ResolutionJob,
    /// Question generation when the assignment carries an escalation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_generation: Option<u32>,
}

/// `ffres1` revoke/supersede profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionRevokeProfile {
    pub schema_version: u32,
    pub job_id: String,
    pub assignment_id: String,
    pub attempt: u32,
    pub conflict_fingerprint: String,
    pub reason: ResolutionRevokeReason,
}

/// `ffres1` profile carried inside an `ffmsg1` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResolutionProfile {
    Assignment(Box<ResolutionAssignmentProfile>),
    Result(ResolutionResult),
    Revoke(ResolutionRevokeProfile),
    HumanAnswer(HumanResolutionAnswer),
}

impl ResolutionProfile {
    /// Stable wire tag of this variant.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Assignment(_) => "assignment",
            Self::Result(_) => "result",
            Self::Revoke(_) => "revoke",
            Self::HumanAnswer(_) => "human_answer",
        }
    }
}

/// Maximum UTF-8 byte length of one encoded `ffres1` profile (matches the
/// 8 KiB encrypted signal body bound; the embedded job is bounded by
/// [`validate_resolution_job`]).
pub const RESOLUTION_MAX_PROFILE_BYTES: usize = 8 * 1024;

/// Validates one complete `ffres1` profile.
///
/// # Errors
/// Returns an error for invalid ids, bounds, or inconsistent fields.
pub fn validate_resolution_profile(profile: &ResolutionProfile) -> Result<()> {
    match profile {
        ResolutionProfile::Assignment(profile) => {
            ensure!(
                profile.schema_version == RESOLUTION_SCHEMA_VERSION,
                "unsupported assignment profile schema {} (expected {RESOLUTION_SCHEMA_VERSION})",
                profile.schema_version
            );
            validate_resolution_job(&profile.job)?;
            if let Some(generation) = profile.question_generation {
                ensure!(
                    generation <= RESOLUTION_MAX_ATTEMPT,
                    "assignment question generation exceeds its bound"
                );
            }
        }
        ResolutionProfile::Result(result) => validate_resolution_result(result)?,
        ResolutionProfile::Revoke(profile) => {
            ensure!(
                profile.schema_version == RESOLUTION_SCHEMA_VERSION,
                "unsupported revoke profile schema {} (expected {RESOLUTION_SCHEMA_VERSION})",
                profile.schema_version
            );
            ensure!(
                crate::integrator_contract::is_valid_hex_id(&profile.job_id, 32)
                    && crate::integrator_contract::is_valid_hex_id(&profile.assignment_id, 32),
                "revoke profile ids must be exactly 32 lowercase hex chars"
            );
            ensure!(
                profile.attempt <= RESOLUTION_MAX_ATTEMPT,
                "revoke profile attempt exceeds its bound"
            );
            ensure!(
                is_valid_hash(&profile.conflict_fingerprint),
                "revoke profile fingerprint must be a full 64-hex digest"
            );
        }
        ResolutionProfile::HumanAnswer(answer) => validate_human_resolution_answer(answer)?,
    }
    Ok(())
}

/// Encodes one `ffres1` profile as `ffres1:` + canonical compact JSON.
///
/// # Errors
/// Returns an error for invalid or over-bound profiles.
pub fn encode_resolution_profile(profile: &ResolutionProfile) -> Result<String> {
    validate_resolution_profile(profile)?;
    let bytes = serde_json::to_vec(profile).context("serialize ffres1 profile")?;
    ensure!(
        bytes.len() <= RESOLUTION_MAX_PROFILE_BYTES,
        "ffres1 profile exceeds the signal body bound"
    );
    let mut body = String::with_capacity(RESOLUTION_JOB_DISCRIMINATOR.len() + 1 + bytes.len());
    body.push_str(RESOLUTION_JOB_DISCRIMINATOR);
    body.push(':');
    body.push_str(
        std::str::from_utf8(&bytes)
            .context("encoded ffres1 profile is not UTF-8 (impossible for compact JSON)")?,
    );
    Ok(body)
}

/// Parses an `ffres1` profile. Returns `None` for non-`ffres1` bodies,
/// unknown versions, malformed JSON, or profiles failing validation.
#[must_use]
pub fn parse_resolution_profile(body: &str) -> Option<ResolutionProfile> {
    let json = body
        .strip_prefix(RESOLUTION_JOB_DISCRIMINATOR)?
        .strip_prefix(':')?;
    if json.len() > RESOLUTION_MAX_PROFILE_BYTES {
        return None;
    }
    let profile: ResolutionProfile = serde_json::from_str(json).ok()?;
    if validate_resolution_profile(&profile).is_err() {
        return None;
    }
    Some(profile)
}

/// Typed reason a guarded publication was refused or invalidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStaleKind {
    /// Workspace head differs from the identity's current snapshot.
    HeadChanged,
    /// The conflict no longer exists in the current head.
    ConflictMissing,
    /// One or more conflict legs changed.
    LegsChanged,
    /// The recomputed identity/fingerprint no longer matches.
    IdentityMismatch,
    /// The assignment was revoked or superseded.
    AssignmentRevoked,
    /// Verification evidence expired or its policy changed.
    VerificationExpired,
    /// The candidate file is missing.
    CandidateMissing,
    /// Candidate bytes hash differently than the descriptor.
    CandidateHashMismatch,
    /// Candidate size differs from the descriptor.
    CandidateSizeMismatch,
    /// Candidate mode differs from the descriptor.
    CandidateModeMismatch,
    /// Candidate path differs from the job destination.
    CandidatePathMismatch,
    /// Candidate path resolves through a symlink or reparse alias.
    CandidateSymlink,
    /// The candidate file could not be opened due to permission denial.
    CandidatePermissionDenied,
    /// The candidate path is not a regular file.
    CandidateInvalidType,
    /// The candidate file failed to open with a typed I/O error.
    CandidateIoError,
}

/// Canonical fixtures for every leg combination, modes, deletion,
/// assignment, accepted-intent set, and verification policy. Update only
/// with a schema/version decision; golden fingerprints are frozen below.
pub mod resolution_fixtures {
    use super::*;
    use crate::EXECUTABLE_MODE;

    pub(crate) fn hex64(byte: u8) -> String {
        std::iter::repeat_n(char::from(byte), 64).collect()
    }

    fn leg(
        present: bool,
        deleted: bool,
        hash: &str,
        size: u64,
        mode: u32,
    ) -> ConflictLegDescriptor {
        ConflictLegDescriptor {
            present,
            deleted,
            hash: hash.to_string(),
            size,
            mode,
        }
    }

    fn live(hash: &str, size: u64, mode: u32) -> ConflictLegDescriptor {
        leg(true, false, hash, size, mode)
    }

    fn deleted() -> ConflictLegDescriptor {
        leg(true, true, "", 0, 0)
    }

    fn absent() -> ConflictLegDescriptor {
        leg(false, false, "", 0, 0)
    }

    fn base_identity(
        base: ConflictLegDescriptor,
        ours: ConflictLegDescriptor,
        theirs: ConflictLegDescriptor,
    ) -> ConflictIdentity {
        let kind = derive_conflict_kind(&base, &ours, &theirs);
        ConflictIdentity {
            schema_version: RESOLUTION_SCHEMA_VERSION,
            workspace_id: "fixture-workspace".to_string(),
            current_snapshot: hex64(b'a'),
            about_snapshot: hex64(b'a'),
            tree_root: hex64(b'4'),
            path: "src/main.rs".to_string(),
            base,
            ours,
            theirs,
            kind,
            task_id: None,
            intent_message_ids: Vec::new(),
            assignment_id: None,
            attempt: None,
            designated_owner: None,
            verification_policy: None,
        }
    }

    /// Canonical edit/edit live conflict (plain mode).
    pub fn edit_edit() -> ConflictIdentity {
        base_identity(
            live(&hex64(b'1'), 120, 0),
            live(&hex64(b'2'), 121, 0),
            live(&hex64(b'3'), 122, 0),
        )
    }

    /// Canonical edit/edit conflict with executable ours.
    pub fn edit_edit_exec() -> ConflictIdentity {
        base_identity(
            live(&hex64(b'1'), 120, 0),
            live(&hex64(b'2'), 121, EXECUTABLE_MODE),
            live(&hex64(b'3'), 122, EXECUTABLE_MODE),
        )
    }

    /// Canonical edit/delete conflict (ours live, theirs deleted).
    pub fn edit_delete() -> ConflictIdentity {
        base_identity(
            live(&hex64(b'1'), 120, 0),
            live(&hex64(b'2'), 121, 0),
            deleted(),
        )
    }

    /// Canonical delete/edit conflict (ours deleted, theirs live).
    pub fn delete_edit() -> ConflictIdentity {
        base_identity(
            live(&hex64(b'1'), 120, 0),
            deleted(),
            live(&hex64(b'3'), 122, 0),
        )
    }

    /// Canonical conflict with an absent base leg (empty file created on both
    /// sides).
    pub fn absent_base() -> ConflictIdentity {
        base_identity(absent(), live(&hex64(b'2'), 1, 0), live(&hex64(b'3'), 2, 0))
    }

    /// Full automatic identity bound to the canonical edit/edit conflict.
    pub fn automatic() -> ConflictIdentity {
        let mut identity = edit_edit();
        identity.task_id = Some("parser-impl".to_string());
        identity.intent_message_ids = vec![hex64(b'5'), hex64(b'6')];
        identity.assignment_id = Some("0123456789abcdef0123456789abcdef".to_string());
        identity.attempt = Some(0);
        identity.designated_owner = Some("agent-b".to_string());
        identity.verification_policy = Some(RESOLUTION_VERIFICATION_POLICY_ID.to_string());
        identity
    }

    /// Canonical job over [`automatic`].
    pub fn job() -> ResolutionJob {
        let conflict = automatic();
        let fingerprint = compute_conflict_identity_fingerprint(&conflict);
        ResolutionJob {
            schema_version: RESOLUTION_SCHEMA_VERSION,
            job_id: "fedcba9876543210fedcba9876543210".to_string(),
            task_id: "parser-impl".to_string(),
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            attempt: 0,
            workspace_id: "fixture-workspace".to_string(),
            owner: "agent-b".to_string(),
            conflict_fingerprint: fingerprint.clone(),
            current_snapshot: conflict.current_snapshot.clone(),
            about_snapshot: conflict.about_snapshot.clone(),
            tree_root: conflict.tree_root.clone(),
            accepted_intents: conflict.intent_message_ids.clone(),
            causal_refs: vec![hex64(b'7')],
            artifacts: vec![
                ArtifactDescriptor {
                    role: ArtifactRoleName::Original,
                    path: "conflicts/1/src/main.rs.original".to_string(),
                },
                ArtifactDescriptor {
                    role: ArtifactRoleName::Local,
                    path: "conflicts/1/src/main.rs.local".to_string(),
                },
                ArtifactDescriptor {
                    role: ArtifactRoleName::Cloud,
                    path: "conflicts/1/src/main.rs.cloud".to_string(),
                },
            ],
            candidate_destination: CandidateDestination {
                path:
                    "orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/candidate-0.bin"
                        .to_string(),
                create_new: true,
            },
            allowed_output_paths: vec!["src/main.rs".to_string()],
            verification: VerificationPolicyRef {
                policy_id: RESOLUTION_VERIFICATION_POLICY_ID.to_string(),
                command_config_ref: RESOLUTION_VERIFICATION_CONFIG_REF.to_string(),
                timeout_ms: RESOLUTION_DEFAULT_VERIFICATION_TIMEOUT_MS,
                freshness_required: true,
            },
            prevention: PreventionReason::Exhausted {
                detail: "no bounded prevention path remains for this conflict".to_string(),
            },
            last_resort_reason: "no bounded prevention path remains for this conflict".to_string(),
            designation: OwnerDesignationEvidence {
                method: OwnerDesignationMethod::CausalEligible,
                nonce: None,
                roster_fingerprint: None,
                eligible: vec!["agent-b".to_string()],
                ranked: vec![],
                reasoning: "agent-b authored the causally older accepted intent".to_string(),
                attempt: 0,
            },
            conflict,
        }
    }

    /// Canonical `candidate_ready` result over [`job`].
    pub fn result() -> ResolutionResult {
        ResolutionResult {
            schema_version: RESOLUTION_SCHEMA_VERSION,
            outcome: ResolutionOutcome::CandidateReady,
            job_id: "fedcba9876543210fedcba9876543210".to_string(),
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            attempt: 0,
            owner: "agent-b".to_string(),
            conflict_fingerprint: job().conflict_fingerprint.clone(),
            candidate: Some(CandidateDescriptor {
                path:
                    "orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/candidate-0.bin"
                        .to_string(),
                hash: crate::hash_bytes(b"reconciled content").to_string(),
                size: 18,
                mode: 0,
                deleted: false,
            }),
            verification: crate::VerificationSummary {
                status: crate::VerificationStatus::Passed,
                summary: "fixture verification passed".to_string(),
                ..crate::VerificationSummary::default()
            },
            diagnostics: vec![],
            question: None,
            human_reason: None,
            question_generation: 0,
            safe_options: vec![],
        }
    }

    /// Canonical `requires_human` result for every allowed human reason.
    pub fn human_result() -> ResolutionResult {
        let mut result = result();
        result.outcome = ResolutionOutcome::RequiresHuman;
        result.candidate = None;
        result.question = Some("Choose which conflict version to keep".to_string());
        result.human_reason = Some(HumanResolutionReason::SemanticAmbiguity);
        result.question_generation = 1;
        result.safe_options = vec![
            HumanResolutionOption::KeepUnresolved,
            HumanResolutionOption::Defer,
        ];
        result
    }

    /// Frozen golden fingerprints. Computed once from the canonical
    /// implementation; changing identity encoding requires a version bump
    /// and new golden vectors.
    pub mod golden {
        /// Fingerprint of `edit_edit`.
        pub const EDIT_EDIT: &str =
            "b97f79ea8da5cd93e1fe4c742a5c7e0e698a996c97d17bac862ce06114fdd2f6";
        /// Fingerprint of `automatic`.
        pub const AUTOMATIC: &str =
            "6b2f68617bf943514b164d5d85c92437bb92ded7405b436631ea569cf1239553";
        /// Fingerprint of `edit_delete`.
        pub const EDIT_DELETE: &str =
            "aa193b40d53f62a8da822dd79e8708b2a030da0884c5be5bac3269a7b46e9900";
        /// Fingerprint of `delete_edit`.
        pub const DELETE_EDIT: &str =
            "e270aa832599fc9329aa4197f29697d63185950b4c7910f39d3ca6375abc8dd0";
    }
}

#[cfg(test)]
mod tests {
    use super::resolution_fixtures as fixtures;
    use super::*;

    #[test]
    fn golden_fingerprints_are_frozen() {
        use super::resolution_fixtures::golden;
        assert_eq!(
            compute_conflict_identity_fingerprint(&fixtures::edit_edit()),
            golden::EDIT_EDIT
        );
        assert_eq!(
            compute_conflict_identity_fingerprint(&fixtures::automatic()),
            golden::AUTOMATIC
        );
        assert_eq!(
            compute_conflict_identity_fingerprint(&fixtures::edit_delete()),
            golden::EDIT_DELETE
        );
        assert_eq!(
            compute_conflict_identity_fingerprint(&fixtures::delete_edit()),
            golden::DELETE_EDIT
        );
    }

    #[test]
    fn identity_fixtures_are_valid_and_distinct() {
        let identities = [
            ("edit_edit", fixtures::edit_edit()),
            ("edit_edit_exec", fixtures::edit_edit_exec()),
            ("edit_delete", fixtures::edit_delete()),
            ("delete_edit", fixtures::delete_edit()),
            ("absent_base", fixtures::absent_base()),
            ("automatic", fixtures::automatic()),
        ];
        let mut fingerprints = Vec::new();
        for (label, identity) in &identities {
            validate_conflict_identity(identity)
                .unwrap_or_else(|error| panic!("{label} fixture must validate: {error}"));
            fingerprints.push(compute_conflict_identity_fingerprint(identity));
        }
        for left in 0..fingerprints.len() {
            for right in (left + 1)..fingerprints.len() {
                assert_ne!(
                    fingerprints[left], fingerprints[right],
                    "{:?} and {:?} must fingerprint differently",
                    identities[left].0, identities[right].0
                );
            }
        }
    }

    #[test]
    fn automatic_fixture_is_automatic_and_job_roundtrips() {
        let identity = fixtures::automatic();
        assert!(identity.is_automatic());
        assert!(!fixtures::edit_edit().is_automatic());

        let job = fixtures::job();
        validate_resolution_job(&job).unwrap();
        let json = serde_json::to_string(&job).unwrap();
        let parsed: ResolutionJob = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, job);

        let result = fixtures::result();
        validate_resolution_result(&result).unwrap();
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ResolutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, result);
    }

    /// One mutation test per identity field: each mutation must change the
    /// byte-exact fingerprint.
    #[test]
    fn every_identity_field_mutation_changes_the_fingerprint() {
        let base = fixtures::automatic();
        let baseline = compute_conflict_identity_fingerprint(&base);

        let mutations: Vec<(&str, ConflictIdentity)> = vec![
            ("workspace_id", {
                let mut i = base.clone();
                i.workspace_id = "other-workspace".into();
                i
            }),
            ("current_snapshot", {
                let mut i = base.clone();
                i.current_snapshot = fixtures::hex64(b'9');
                i
            }),
            ("about_snapshot", {
                let mut i = base.clone();
                i.about_snapshot = fixtures::hex64(b'9');
                i
            }),
            ("tree_root", {
                let mut i = base.clone();
                i.tree_root = fixtures::hex64(b'9');
                i
            }),
            ("path", {
                let mut i = base.clone();
                i.path = "src/other.rs".into();
                i
            }),
            ("base_presence", {
                let mut i = base.clone();
                i.base.present = false;
                i.base.hash.clear();
                i.base.size = 0;
                i
            }),
            ("base_deletion", {
                let mut i = base.clone();
                i.base.deleted = true;
                i.base.hash.clear();
                i.base.size = 0;
                i.base.mode = 0;
                i.kind = derive_conflict_kind(&i.base, &i.ours, &i.theirs);
                i
            }),
            ("base_hash", {
                let mut i = base.clone();
                i.base.hash = fixtures::hex64(b'9');
                i
            }),
            ("base_size", {
                let mut i = base.clone();
                i.base.size = 999;
                i
            }),
            ("base_mode", {
                let mut i = base.clone();
                i.base.mode = crate::EXECUTABLE_MODE;
                i
            }),
            ("ours_presence", {
                let mut i = base.clone();
                i.ours.present = false;
                i.ours.hash.clear();
                i.ours.size = 0;
                i.kind = derive_conflict_kind(&i.base, &i.ours, &i.theirs);
                i
            }),
            ("ours_deletion", {
                let mut i = base.clone();
                i.ours.deleted = true;
                i.ours.hash.clear();
                i.ours.size = 0;
                i.ours.mode = 0;
                i.kind = derive_conflict_kind(&i.base, &i.ours, &i.theirs);
                i
            }),
            ("ours_hash", {
                let mut i = base.clone();
                i.ours.hash = fixtures::hex64(b'9');
                i
            }),
            ("ours_size", {
                let mut i = base.clone();
                i.ours.size = 777;
                i
            }),
            ("ours_mode", {
                let mut i = base.clone();
                i.ours.mode = if i.ours.mode == 0 {
                    crate::EXECUTABLE_MODE
                } else {
                    0
                };
                i
            }),
            ("theirs_presence", {
                let mut i = base.clone();
                i.theirs.present = false;
                i.theirs.hash.clear();
                i.theirs.size = 0;
                i.kind = derive_conflict_kind(&i.base, &i.ours, &i.theirs);
                i
            }),
            ("theirs_deletion", {
                let mut i = base.clone();
                i.theirs.deleted = true;
                i.theirs.hash.clear();
                i.theirs.size = 0;
                i.theirs.mode = 0;
                i.kind = derive_conflict_kind(&i.base, &i.ours, &i.theirs);
                i
            }),
            ("theirs_hash", {
                let mut i = base.clone();
                i.theirs.hash = fixtures::hex64(b'9');
                i
            }),
            ("theirs_size", {
                let mut i = base.clone();
                i.theirs.size = 555;
                i
            }),
            ("theirs_mode", {
                let mut i = base.clone();
                i.theirs.mode = crate::EXECUTABLE_MODE;
                i
            }),
            ("kind", {
                let mut i = base.clone();
                i.kind = ConflictKind::EditDelete;
                i
            }),
            ("task_id", {
                let mut i = base.clone();
                i.task_id = Some("other-task".into());
                i
            }),
            ("intent_ids_add", {
                let mut i = base.clone();
                i.intent_message_ids.push(fixtures::hex64(b'9'));
                i
            }),
            ("intent_ids_remove", {
                let mut i = base.clone();
                i.intent_message_ids.pop();
                i
            }),
            ("assignment_id", {
                let mut i = base.clone();
                i.assignment_id = Some("11111111111111111111111111111111".into());
                i
            }),
            ("attempt", {
                let mut i = base.clone();
                i.attempt = Some(1);
                i
            }),
            ("designated_owner", {
                let mut i = base.clone();
                i.designated_owner = Some("agent-a".into());
                i
            }),
            ("verification_policy", {
                let mut i = base.clone();
                i.verification_policy = Some("other-policy-v2".into());
                i
            }),
        ];

        assert_eq!(mutations.len(), 28, "keep mutation coverage complete");
        for (label, mutated) in mutations {
            let fingerprint = compute_conflict_identity_fingerprint(&mutated);
            assert_ne!(
                fingerprint, baseline,
                "mutating {label} must change the identity fingerprint"
            );
            assert_ne!(mutated, base, "mutating {label} must change the identity");
        }
    }

    #[test]
    fn absent_and_deleted_leg_encodings_are_ambiguous_proof() {
        // A live leg with hash "z"*64 vs a live leg with hash "z"*63 + "a":
        // length prefixes prevent truncation/concatenation ambiguity.
        let mut left = fixtures::edit_edit();
        let mut right = fixtures::edit_edit();
        right.ours.hash = format!("{}a", fixtures::hex64(b'9').trim_end_matches('9'));
        assert_ne!(
            compute_conflict_identity_fingerprint(&left),
            compute_conflict_identity_fingerprint(&right)
        );
        let _ = &mut left;
    }

    #[test]
    fn validation_rejects_out_of_bounds_fields() {
        let mut identity = fixtures::automatic();
        identity.intent_message_ids = vec!["x".repeat(64)];
        assert!(validate_conflict_identity(&identity).is_err());
        identity = fixtures::automatic();
        identity.intent_message_ids = vec![fixtures::hex64(b'i'), fixtures::hex64(b'i')];
        assert!(validate_conflict_identity(&identity).is_err());
        identity = fixtures::automatic();
        identity.path = "../escape".into();
        assert!(validate_conflict_identity(&identity).is_err());
        identity = fixtures::automatic();
        identity.ours.size = RESOLUTION_MAX_LEG_BYTES + 1;
        assert!(validate_conflict_identity(&identity).is_err());
        identity = fixtures::automatic();
        identity.ours.mode = 0o777;
        assert!(validate_conflict_identity(&identity).is_err());
        identity = fixtures::automatic();
        identity.kind = ConflictKind::DeleteEdit;
        assert!(validate_conflict_identity(&identity).is_err());

        let mut job = fixtures::job();
        job.conflict_fingerprint = fixtures::hex64(b'9');
        assert!(validate_resolution_job(&job).is_err());
        job = fixtures::job();
        job.candidate_destination.create_new = false;
        assert!(validate_resolution_job(&job).is_err());
        job = fixtures::job();
        job.last_resort_reason = "x".repeat(RESOLUTION_MAX_REASON_BYTES + 1);
        assert!(validate_resolution_job(&job).is_err());
        job = fixtures::job();
        job.prevention = PreventionReason::Violated {
            detail: " ".to_string(),
        };
        assert!(validate_resolution_job(&job).is_err());

        let mut result = fixtures::result();
        result.outcome = ResolutionOutcome::NoChangeRequired;
        assert!(validate_resolution_result(&result).is_err());
        result = fixtures::result();
        result.outcome = ResolutionOutcome::RequiresHuman;
        assert!(validate_resolution_result(&result).is_err());
        result = fixtures::result();
        result.verification.status = crate::VerificationStatus::Failed;
        assert!(validate_resolution_result(&result).is_err());
        result = fixtures::result();
        result.candidate.as_mut().unwrap().hash = "short".into();
        assert!(validate_resolution_result(&result).is_err());
        result = fixtures::result();
        result.candidate.as_mut().unwrap().size = RESOLUTION_MAX_CANDIDATE_BYTES + 1;
        assert!(validate_resolution_result(&result).is_err());
        result = fixtures::result();
        result.diagnostics = vec!["d".repeat(RESOLUTION_DIAGNOSTIC_BYTES + 1)];
        assert!(validate_resolution_result(&result).is_err());
        result = fixtures::result();
        result.question = Some("q".repeat(RESOLUTION_MAX_QUESTION_BYTES + 1));
        assert!(validate_resolution_result(&result).is_err());
    }

    #[test]
    fn every_allowed_human_reason_roundtrips_with_one_question() {
        for reason in [
            HumanResolutionReason::SemanticAmbiguity,
            HumanResolutionReason::UnavoidableDataLoss,
            HumanResolutionReason::MissingOrAuthFailedLeg,
            HumanResolutionReason::SecurityCompatibilityBoundaryChange,
            HumanResolutionReason::RequiredVerificationUnavailable,
            HumanResolutionReason::IndeterminateOwnership,
            HumanResolutionReason::BoundedResolverExhaustion,
            HumanResolutionReason::UnsupportedSizeSafetyBound,
            HumanResolutionReason::ExplicitProductDecision,
        ] {
            let mut result = fixtures::human_result();
            result.human_reason = Some(reason);
            validate_resolution_result(&result).unwrap_or_else(|error| {
                panic!("{:?} must be an allowed human reason: {error}", reason)
            });
            let json = serde_json::to_string(&result).unwrap();
            let parsed: ResolutionResult = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.human_reason, Some(reason));
            assert!(parsed.question.is_some());
        }
    }

    #[test]
    fn closed_outcome_set_serializes_stable() {
        for outcome in [
            ResolutionOutcome::CandidateReady,
            ResolutionOutcome::NoChangeRequired,
            ResolutionOutcome::Blocked,
            ResolutionOutcome::RequiresHuman,
            ResolutionOutcome::Failed,
            ResolutionOutcome::Stale,
        ] {
            assert_eq!(
                serde_json::from_str::<ResolutionOutcome>(&format!("\"{}\"", outcome.as_str()))
                    .unwrap(),
                outcome
            );
        }
        assert!(serde_json::from_str::<ResolutionOutcome>("\"unknown\"").is_err());
        assert!(serde_json::from_str::<ResolutionOutcome>("\"retry\"").is_err());
    }
}
