//! Canonical randomized integrator assignment contracts (SDK-1 additive).
//!
//! This module owns every type shared by the Rust SDK, CLI, JSON, C FFI,
//! TypeScript, MCP, and NDJSON event surfaces: candidate descriptors,
//! eligibility and neutrality filtering, the auditable Blake3 ranking, the
//! `ffint1` assignment/reply profiles carried inside `ffmsg1` bodies, and the
//! bounded human digest. Adapters must never reimplement selection,
//! canonicalization, ranking, or lifecycle transitions.

use crate::{hash_bytes, is_valid_hash};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

/// Version string domain-separating every integrator selection draw.
pub const INTEGRATOR_ALGORITHM_VERSION: &str = "feanorfs-integrator-selection-v1";

/// Discriminator prefix for integrator profiles carried inside `ffmsg1.body`.
pub const INTEGRATOR_PROFILE_DISCRIMINATOR: &str = "ffint1";

/// Maximum number of candidates in one roster.
pub const INTEGRATOR_MAX_CANDIDATES: usize = 64;
/// Maximum number of capabilities per candidate or requirement list.
pub const INTEGRATOR_MAX_CAPABILITIES: usize = 64;
/// Maximum UTF-8 byte length of one lowercase-ASCII capability identifier.
pub const INTEGRATOR_CAPABILITY_MAX_BYTES: usize = 32;
/// Maximum UTF-8 byte length of one task summary.
pub const INTEGRATOR_MAX_TASK_SUMMARY_BYTES: usize = 1024;
/// Maximum number of conflict authors or explicit exclusions.
pub const INTEGRATOR_MAX_AUTHORS: usize = 32;
/// Maximum number of terminal assignments retained in dispatcher history.
pub const INTEGRATOR_MAX_HISTORY: usize = 64;
/// Maximum number of risk entries in one digest.
pub const INTEGRATOR_MAX_RISKS: usize = 10;
/// Maximum UTF-8 bytes for bounded digest text fields (outcome, summary,
/// decision question).
pub const INTEGRATOR_DIGEST_FIELD_BYTES: usize = 512;
/// Maximum UTF-8 bytes for one risk entry.
pub const INTEGRATOR_RISK_BYTES: usize = 256;
/// Maximum number of paths in an on-demand bounded path list.
pub const INTEGRATOR_MAX_PATHS: usize = 256;
/// Default acknowledgement timeout before the dispatcher may fall back.
pub const INTEGRATOR_DEFAULT_ACK_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// Whether a string is a valid FeanorFS agent name (mirrors the agent-core
/// validator so the common contract stays standalone and cross-language).
#[must_use]
pub fn is_valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(char::is_control)
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && name != "*"
}

/// Whether `value` is exactly `bytes` lowercase hex characters.
#[must_use]
pub fn is_valid_hex_id(value: &str, bytes: usize) -> bool {
    value.len() == bytes && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// One candidate descriptor supplied by the authorized dispatcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorCandidate {
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub available: bool,
}

const fn default_true() -> bool {
    true
}

/// Full dispatcher input for one integrator assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorAssignInput {
    /// Full reachable format-v3 snapshot ID the batch concerns.
    pub about_snapshot: String,
    /// Explicit candidate descriptors from the authorized dispatcher.
    pub candidates: Vec<IntegratorCandidate>,
    /// Required capabilities; every eligible candidate must have all of them.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Names of agents that authored a conflicting side; used for neutrality.
    #[serde(default)]
    pub conflict_authors: Vec<String>,
    /// Explicit user exclusions (hard filters).
    #[serde(default)]
    pub excluded: Vec<String>,
    /// Bounded plain-language objective.
    pub task_summary: String,
    /// Milliseconds to wait for acknowledgement before a pre-acceptance
    /// timeout may fall back to the next recorded candidate.
    #[serde(default)]
    pub ack_timeout_ms: Option<u64>,
}

/// Result of eligibility filtering before the random draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EligibilityResult {
    /// Final eligible pool, sorted by name.
    pub eligible: Vec<String>,
    /// Neutral subset (eligible candidates that did not author a conflict
    /// side); empty when no neutral candidate exists.
    pub neutral: Vec<String>,
    /// True when the draw used the neutral subset.
    pub neutral_integrator: bool,
    /// Bounded reason when no candidate remains (escalation path).
    pub no_candidate_reason: Option<String>,
}

/// The complete auditable draw for one assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorDraw {
    /// 128-bit OS-CSPRNG assignment id, 32 lowercase hex chars.
    pub assignment_id: String,
    /// 256-bit OS-CSPRNG selection nonce, 64 lowercase hex chars.
    pub selection_nonce: String,
    pub about_snapshot: String,
    /// Blake3 of the canonical sorted roster JSON.
    pub roster_fingerprint: String,
    /// Selected candidate first, then immutable fallback order.
    pub ranked: Vec<String>,
    pub neutral_integrator: bool,
    /// Final eligible pool, sorted by name.
    pub eligible: Vec<String>,
}

/// State of one assignment (serde snake_case on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegratorAssignmentState {
    /// Draw persisted, nothing offered yet.
    Created,
    /// Assignment request sent to the current attempt's candidate.
    Offered,
    /// The selected candidate accepted the assignment.
    Accepted,
    /// Accepted and integration work is in progress.
    Active,
    /// Integration finished with a verified digest.
    Completed,
    /// The integrator reported a blocker.
    Blocked,
    /// The dispatcher stopped the active integrator or a decision is needed.
    Revoked,
    /// Dispatcher state is uncertain; a human must decide.
    RequiresHuman,
    /// Assignment cancelled with no integrator.
    Cancelled,
}

/// State of one per-candidate attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegratorAttemptState {
    Offered,
    Accepted,
    Active,
    /// Pre-acceptance timeout; this candidate is out of the rotation.
    TimedOut,
    /// Superseded by a later attempt (stale replies are rejected).
    Superseded,
    /// Dispatcher explicitly revoked this attempt.
    Revoked,
    /// Candidate reported a blocker or produced a blocked outcome.
    Blocked,
    Completed,
}

impl IntegratorAttemptState {
    /// Whether this attempt can still accept the assignment.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Offered | Self::Accepted | Self::Active)
    }
}

/// One per-candidate attempt inside an assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorAttempt {
    pub attempt: u32,
    pub selected: String,
    pub state: IntegratorAttemptState,
    pub offered_at_ms: i64,
    pub request_message_id: Option<String>,
    pub acceptance_message_id: Option<String>,
    pub terminal_message_id: Option<String>,
    pub reason: Option<String>,
}

/// Verification outcome inside a digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Failed,
    Unknown,
}

impl VerificationStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// Verification summary inside a digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationSummary {
    pub status: VerificationStatus,
    pub summary: String,
}

/// Terminal outcome state carried by a result digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegratorOutcomeState {
    Completed,
    Blocked,
    RequiresHuman,
    Cancelled,
}

/// Bounded human digest produced by an integrator (INT-10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorDigest {
    pub assignment_id: String,
    pub integrator: String,
    pub about_snapshot: String,
    /// The snapshot actually inspected and tested.
    pub inspected_snapshot: String,
    pub state: IntegratorOutcomeState,
    pub landed_paths: u64,
    pub resolved_conflicts: u64,
    pub remaining_conflicts: u64,
    pub verification: VerificationSummary,
    pub outcome: String,
    #[serde(default)]
    pub risks: Vec<String>,
    /// At most one focused question; `None` when no decision is required.
    pub decision_required: Option<String>,
}

/// `ffint1` profile carried inside an `ffmsg1` body (INT-7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IntegratorProfile {
    /// Dispatcher -> candidate: assign one batch (ffmsg1 kind `request`).
    Assignment {
        assignment_id: String,
        attempt: u32,
        selected: String,
        about_snapshot: String,
        roster_fingerprint: String,
        neutral_integrator: bool,
        task: String,
    },
    /// Candidate -> dispatcher: acceptance checkpoint (kind `status`).
    Accepted {
        assignment_id: String,
        attempt: u32,
        about_snapshot: String,
    },
    /// Candidate -> dispatcher: terminal verified outcome (kind `result`).
    Result {
        assignment_id: String,
        attempt: u32,
        about_snapshot: String,
        digest: IntegratorDigest,
    },
    /// Candidate -> dispatcher: terminal blocker (kind `blocked`).
    Blocked {
        assignment_id: String,
        attempt: u32,
        about_snapshot: String,
        reason: String,
    },
}

/// Result of `agent integrator assign`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorAssignResult {
    pub assignment_id: String,
    pub about_snapshot: String,
    pub selected: String,
    pub fallback_order: Vec<String>,
    pub neutral_integrator: bool,
    pub roster_fingerprint: String,
    pub attempt: u32,
    pub request_message_id: String,
    pub state: IntegratorAssignmentState,
    pub task_summary: String,
}

/// One attempt rendered for `integrator status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorAttemptStatus {
    pub attempt: u32,
    pub selected: String,
    pub state: IntegratorAttemptState,
    pub offered_at_ms: i64,
    pub request_message_id: Option<String>,
    pub terminal_message_id: Option<String>,
    pub reason: Option<String>,
}

/// Result of `agent integrator status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorStatusResult {
    pub assignment_id: String,
    pub about_snapshot: String,
    pub state: IntegratorAssignmentState,
    pub selected: Option<String>,
    pub attempt: u32,
    pub neutral_integrator: bool,
    pub roster_fingerprint: String,
    pub fallback_order: Vec<String>,
    pub task_summary: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub attempts: Vec<IntegratorAttemptStatus>,
    pub digest: Option<IntegratorDigest>,
    pub inbox_cursor: Option<String>,
}

/// Result of one dispatcher observation pass (resume/observe).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorObserveResult {
    pub assignment_id: Option<String>,
    pub state: Option<IntegratorAssignmentState>,
    /// How many ffint1 signals were processed.
    pub messages_processed: usize,
    pub cursor: Option<String>,
    pub cursor_reset: bool,
    /// Human-readable bounded action taken ("none", "accepted", ...).
    pub action: String,
}

/// Result of materializing encrypted conflict legs on a third machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictMaterializeEntry {
    pub path: String,
    pub kind: crate::ConflictKind,
    pub original_available: bool,
    pub local_available: bool,
    pub cloud_available: bool,
    pub is_binary: bool,
    /// True when a pending local row already existed and was reused.
    pub already_materialized: bool,
}

/// Result of `conflicts materialize` (read-only; never changes the head).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictMaterializeResult {
    pub about_snapshot: String,
    /// Directory holding `.original`/`.local`/`.cloud` artifacts.
    pub conflict_dir: String,
    pub entries: Vec<ConflictMaterializeEntry>,
}

/// Validates and normalizes one capability identifier (lowercase ASCII,
/// bounded length). Returns the normalized identifier.
///
/// # Errors
/// Returns an error for empty, non-lowercase-ASCII, or oversized identifiers.
pub fn normalize_capability(capability: &str) -> Result<String> {
    let trimmed = capability.trim();
    ensure!(!trimmed.is_empty(), "capability must not be empty");
    ensure!(
        trimmed.len() <= INTEGRATOR_CAPABILITY_MAX_BYTES,
        "capability exceeds {INTEGRATOR_CAPABILITY_MAX_BYTES} bytes"
    );
    ensure!(
        trimmed
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()),
        "capability must be lowercase ASCII letters, digits, or '-'"
    );
    Ok(trimmed.to_string())
}

/// Normalizes a capability list: trims, validates, deduplicates, sorts.
///
/// # Errors
/// Returns an error for invalid identifiers or an oversized list.
pub fn normalize_capabilities(capabilities: &[String]) -> Result<Vec<String>> {
    ensure!(
        capabilities.len() <= INTEGRATOR_MAX_CAPABILITIES,
        "capability list exceeds {INTEGRATOR_MAX_CAPABILITIES} entries"
    );
    let mut out: Vec<String> = Vec::new();
    for capability in capabilities {
        let normalized = normalize_capability(capability)?;
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out.sort();
    Ok(out)
}

fn validate_candidate(candidate: &IntegratorCandidate, seen: &mut Vec<String>) -> Result<()> {
    ensure!(
        is_valid_agent_name(&candidate.name),
        "invalid candidate name {:?}",
        candidate.name
    );
    ensure!(
        !seen.contains(&candidate.name),
        "duplicate candidate {:?}",
        candidate.name
    );
    seen.push(candidate.name.clone());
    normalize_capabilities(&candidate.capabilities)?;
    Ok(())
}

/// Applies hard eligibility filters (INT-3):
/// remove disabled/unavailable candidates, require every requested
/// capability, apply explicit exclusions, then build the neutral subset that
/// excludes conflict authors. Uses the neutral subset when non-empty.
///
/// # Errors
/// Returns an error for invalid input (empty roster, invalid or duplicate
/// names, invalid capabilities, unbounded lists). A filtered-out roster is
/// not an error: it returns a `no_candidate_reason` for escalation.
pub fn filter_eligible(input: &IntegratorAssignInput) -> Result<EligibilityResult> {
    ensure!(
        !input.candidates.is_empty(),
        "candidate roster must not be empty"
    );
    ensure!(
        input.candidates.len() <= INTEGRATOR_MAX_CANDIDATES,
        "candidate roster exceeds {INTEGRATOR_MAX_CANDIDATES} entries"
    );
    ensure!(
        !input.task_summary.trim().is_empty()
            && input.task_summary.len() <= INTEGRATOR_MAX_TASK_SUMMARY_BYTES,
        "task summary must be non-empty and at most {INTEGRATOR_MAX_TASK_SUMMARY_BYTES} bytes"
    );
    ensure!(
        is_valid_hash(&input.about_snapshot),
        "about_snapshot must be a full 64-hex snapshot id"
    );
    ensure!(
        input.conflict_authors.len() <= INTEGRATOR_MAX_AUTHORS
            && input.excluded.len() <= INTEGRATOR_MAX_AUTHORS,
        "conflict author or exclusion list exceeds {INTEGRATOR_MAX_AUTHORS} entries"
    );
    for name in input.conflict_authors.iter().chain(input.excluded.iter()) {
        ensure!(
            is_valid_agent_name(name),
            "invalid agent name {:?} in authors/exclusions",
            name
        );
    }

    let required = normalize_capabilities(&input.required_capabilities)?;
    let excluded: Vec<&str> = input.excluded.iter().map(String::as_str).collect();
    let conflict_authors: Vec<&str> = input.conflict_authors.iter().map(String::as_str).collect();

    let mut seen = Vec::new();
    let mut eligible = Vec::new();
    let mut neutral = Vec::new();
    for candidate in &input.candidates {
        validate_candidate(candidate, &mut seen)?;
        if !candidate.enabled || !candidate.available {
            continue;
        }
        if excluded.contains(&candidate.name.as_str()) {
            continue;
        }
        let capabilities = normalize_capabilities(&candidate.capabilities)?;
        if !required.iter().all(|cap| capabilities.contains(cap)) {
            continue;
        }
        eligible.push(candidate.name.clone());
        if !conflict_authors.contains(&candidate.name.as_str()) {
            neutral.push(candidate.name.clone());
        }
    }
    eligible.sort();
    neutral.sort();

    let (pool, neutral_integrator) = if neutral.is_empty() {
        (eligible.clone(), false)
    } else {
        (neutral.clone(), true)
    };

    let no_candidate_reason = if pool.is_empty() {
        let mut parts = Vec::new();
        if eligible.is_empty() {
            parts.push(
                "no eligible candidate satisfies the roster, capability, and exclusion filters"
                    .to_string(),
            );
        } else {
            parts.push(
                "no neutral candidate exists (all eligible candidates authored a conflicting side)"
                    .to_string(),
            );
        }
        Some(parts.join("; "))
    } else {
        None
    };

    Ok(EligibilityResult {
        eligible,
        neutral,
        neutral_integrator,
        no_candidate_reason,
    })
}

/// Canonical Blake3 roster fingerprint: hash of the canonical JSON array of
/// the sorted final candidate names. Deterministic across platforms.
///
/// # Errors
/// Returns an error when the roster is empty or a name is invalid.
pub fn roster_fingerprint(pool: &[String]) -> Result<String> {
    ensure!(!pool.is_empty(), "final candidate pool must not be empty");
    let mut sorted = pool.to_vec();
    sorted.sort();
    for name in &sorted {
        ensure!(
            is_valid_agent_name(name),
            "invalid candidate name {:?}",
            name
        );
    }
    let canonical = serde_json::to_string(&sorted)?;
    Ok(hash_bytes(canonical.as_bytes()))
}

fn update_len_prefixed(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

/// Auditable random ranking (INT-4).
///
/// Scores every candidate with
/// `BLAKE3(domain ‖ len(workspace_id) ‖ workspace_id ‖ about_snapshot ‖
/// assignment_id ‖ selection_nonce ‖ roster_fingerprint ‖ len(agent_name) ‖
/// agent_name)` — every variable-width value is length-prefixed. Sorts
/// ascending by the 32-byte score, then by agent-name bytes as a deterministic
/// collision tie-breaker. The first element is selected; the rest is the
/// immutable fallback order.
///
/// The `workspace_id` is available only inside the trusted client process and
/// must never be emitted in user-facing output, messages, or logs.
///
/// # Errors
/// Returns an error for malformed ids or an empty roster.
pub fn rank_candidates(
    workspace_id: &str,
    about_snapshot: &str,
    assignment_id: &str,
    selection_nonce: &str,
    roster_fingerprint_value: &str,
    candidates: &[String],
) -> Result<Vec<String>> {
    ensure!(!candidates.is_empty(), "candidate pool must not be empty");
    ensure!(
        is_valid_hash(about_snapshot) && is_valid_hash(roster_fingerprint_value),
        "about_snapshot and roster_fingerprint must be full 64-hex ids"
    );
    ensure!(
        is_valid_hex_id(assignment_id, 32),
        "assignment_id must be exactly 32 lowercase hex chars (128 bits)"
    );
    ensure!(
        is_valid_hex_id(selection_nonce, 64),
        "selection_nonce must be exactly 64 lowercase hex chars (256 bits)"
    );
    let mut scored: Vec<(&String, [u8; 32])> = Vec::with_capacity(candidates.len());
    for name in candidates {
        ensure!(
            is_valid_agent_name(name),
            "invalid candidate name {:?}",
            name
        );
        let mut hasher = blake3::Hasher::new();
        update_len_prefixed(&mut hasher, INTEGRATOR_ALGORITHM_VERSION.as_bytes());
        update_len_prefixed(&mut hasher, workspace_id.as_bytes());
        update_len_prefixed(&mut hasher, about_snapshot.as_bytes());
        update_len_prefixed(&mut hasher, assignment_id.as_bytes());
        update_len_prefixed(&mut hasher, selection_nonce.as_bytes());
        update_len_prefixed(&mut hasher, roster_fingerprint_value.as_bytes());
        update_len_prefixed(&mut hasher, name.as_bytes());
        scored.push((name, hasher.finalize().into()));
    }
    scored.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| left.0.as_bytes().cmp(right.0.as_bytes()))
    });
    Ok(scored.into_iter().map(|(name, _)| name.clone()).collect())
}

/// Generates a 128-bit assignment id from the OS CSPRNG (32 lowercase hex).
///
/// # Errors
/// Returns an error when the operating system CSPRNG is unavailable.
pub fn generate_assignment_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(hex_lower(&bytes))
}

/// Generates a 256-bit selection nonce from the OS CSPRNG (64 lowercase hex).
///
/// # Errors
/// Returns an error when the operating system CSPRNG is unavailable.
pub fn generate_selection_nonce() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(hex_lower(&bytes))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Encodes one `ffint1` profile as `ffint1:` + canonical compact JSON.
/// The resulting string must fit inside the 8 KiB `ffmsg1` body bound.
///
/// # Errors
/// Returns an error for invalid ids, names, attempts, or oversized fields.
pub fn encode_integrator_profile(profile: &IntegratorProfile) -> Result<String> {
    validate_profile(profile)?;
    let json = serde_json::to_string(profile)?;
    let encoded = format!("{INTEGRATOR_PROFILE_DISCRIMINATOR}:{json}");
    ensure!(
        encoded.len() <= crate::AGENT_MESSAGE_MAX_BODY_BYTES,
        "ffint1 profile exceeds the 8 KiB signal body bound"
    );
    Ok(encoded)
}

/// Parses an `ffint1` profile. Returns `None` for unknown versions,
/// malformed payloads, or non-canonical JSON. Unknown future versions remain
/// ordinary signal text and cannot break typed inbox reads.
#[must_use]
pub fn parse_integrator_profile(body: &str) -> Option<IntegratorProfile> {
    let json = body.strip_prefix(INTEGRATOR_PROFILE_DISCRIMINATOR)?;
    let json = json.strip_prefix(':')?;
    let profile: IntegratorProfile = serde_json::from_str(json).ok()?;
    if serde_json::to_string(&profile).ok()? != json {
        return None;
    }
    if validate_profile(&profile).is_err() {
        return None;
    }
    Some(profile)
}

fn validate_profile(profile: &IntegratorProfile) -> Result<()> {
    let (assignment_id, _attempt, about_snapshot) = match profile {
        IntegratorProfile::Assignment {
            assignment_id,
            attempt,
            about_snapshot,
            ..
        }
        | IntegratorProfile::Accepted {
            assignment_id,
            attempt,
            about_snapshot,
        }
        | IntegratorProfile::Result {
            assignment_id,
            attempt,
            about_snapshot,
            ..
        }
        | IntegratorProfile::Blocked {
            assignment_id,
            attempt,
            about_snapshot,
            ..
        } => (assignment_id, attempt, about_snapshot),
    };
    ensure!(
        is_valid_hex_id(assignment_id, 32),
        "ffint1 assignment_id must be exactly 32 lowercase hex chars"
    );
    ensure!(
        is_valid_hash(about_snapshot),
        "ffint1 about_snapshot must be a full 64-hex snapshot id"
    );
    ensure!(
        *_attempt <= 10_000,
        "ffint1 attempt count exceeds the bounded maximum"
    );
    match profile {
        IntegratorProfile::Assignment {
            selected,
            roster_fingerprint,
            task,
            neutral_integrator,
            ..
        } => {
            ensure!(
                is_valid_agent_name(selected),
                "ffint1 selected must be a valid agent name"
            );
            ensure!(
                is_valid_hash(roster_fingerprint),
                "ffint1 roster_fingerprint must be a full 64-hex id"
            );
            ensure!(
                !task.trim().is_empty() && task.len() <= INTEGRATOR_MAX_TASK_SUMMARY_BYTES,
                "ffint1 task must be non-empty and bounded"
            );
            let _ = neutral_integrator;
            Ok(())
        }
        IntegratorProfile::Accepted { .. } => Ok(()),
        IntegratorProfile::Result { digest, .. } => validate_integrator_digest(digest),
        IntegratorProfile::Blocked { reason, .. } => {
            ensure!(
                !reason.trim().is_empty() && reason.len() <= INTEGRATOR_DIGEST_FIELD_BYTES,
                "ffint1 blocker reason must be non-empty and bounded"
            );
            Ok(())
        }
    }
}

/// Validates digest field, count, and byte bounds (INT-10).
///
/// # Errors
/// Returns an error for any out-of-bounds field.
pub fn validate_integrator_digest(digest: &IntegratorDigest) -> Result<()> {
    ensure!(
        is_valid_hex_id(&digest.assignment_id, 32),
        "digest assignment_id must be exactly 32 lowercase hex chars"
    );
    ensure!(
        is_valid_agent_name(&digest.integrator),
        "digest integrator must be a valid agent name"
    );
    ensure!(
        is_valid_hash(&digest.about_snapshot) && is_valid_hash(&digest.inspected_snapshot),
        "digest snapshots must be full 64-hex ids"
    );
    ensure!(
        !digest.outcome.trim().is_empty() && digest.outcome.len() <= INTEGRATOR_DIGEST_FIELD_BYTES,
        "digest outcome must be non-empty and at most {INTEGRATOR_DIGEST_FIELD_BYTES} bytes"
    );
    ensure!(
        digest.verification.summary.len() <= INTEGRATOR_DIGEST_FIELD_BYTES,
        "digest verification summary must be at most {INTEGRATOR_DIGEST_FIELD_BYTES} bytes"
    );
    ensure!(
        digest.risks.len() <= INTEGRATOR_MAX_RISKS,
        "digest risks exceed {INTEGRATOR_MAX_RISKS} entries"
    );
    for risk in &digest.risks {
        ensure!(
            risk.len() <= INTEGRATOR_RISK_BYTES,
            "digest risk exceeds {INTEGRATOR_RISK_BYTES} bytes"
        );
    }
    if let Some(question) = &digest.decision_required {
        ensure!(
            !question.trim().is_empty()
                && question.len() <= INTEGRATOR_DIGEST_FIELD_BYTES,
            "digest decision question must be non-empty and at most {INTEGRATOR_DIGEST_FIELD_BYTES} bytes"
        );
    }
    Ok(())
}

/// Ensures a bounded on-demand path list stays within the contract bound.
///
/// # Errors
/// Returns an error for an oversized list.
pub fn validate_path_list(paths: &[String]) -> Result<()> {
    ensure!(
        paths.len() <= INTEGRATOR_MAX_PATHS,
        "path list exceeds {INTEGRATOR_MAX_PATHS} entries"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConflictKind;

    const SNAP_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SNAP_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const WS: &str = "ws-test-1";
    const ASSIGNMENT: &str = "0123456789abcdef0123456789abcdef";
    const NONCE: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn candidate(name: &str, capabilities: &[&str]) -> IntegratorCandidate {
        IntegratorCandidate {
            name: name.to_string(),
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            enabled: true,
            available: true,
        }
    }

    fn input(candidates: Vec<IntegratorCandidate>) -> IntegratorAssignInput {
        IntegratorAssignInput {
            about_snapshot: SNAP_A.to_string(),
            candidates,
            required_capabilities: vec![],
            conflict_authors: vec![],
            excluded: vec![],
            task_summary: "Integrate parser implementation and tests".to_string(),
            ack_timeout_ms: Some(300_000),
        }
    }

    #[test]
    fn assignment_and_nonce_hex_helpers() {
        assert!(is_valid_hex_id(ASSIGNMENT, 32));
        assert!(is_valid_hex_id(NONCE, 64));
        assert!(!is_valid_hex_id(ASSIGNMENT, 64));
        assert!(!is_valid_hex_id(&ASSIGNMENT.to_uppercase(), 32));
        let id = generate_assignment_id().unwrap();
        assert!(is_valid_hex_id(&id, 32));
        let nonce = generate_selection_nonce().unwrap();
        assert!(is_valid_hex_id(&nonce, 64));
        // Two draws must not collide in practice.
        assert_ne!(id, generate_assignment_id().unwrap());
    }

    #[test]
    fn agent_name_validation_matches_signal_rules() {
        for name in [
            "",
            ".",
            "..",
            "*",
            "nested/name",
            "nested\\name",
            "bad\nname",
        ] {
            assert!(!is_valid_agent_name(name), "{name:?} must be rejected");
        }
        for name in ["agent-a", "mac-test", "ci1", "a"] {
            assert!(is_valid_agent_name(name), "{name:?} must be accepted");
        }
    }

    #[test]
    fn capability_normalization_is_stable() {
        assert_eq!(
            normalize_capabilities(&["rust".into(), "rust".into(), "ios".into()]).unwrap(),
            vec!["ios".to_string(), "rust".to_string()]
        );
        assert!(normalize_capabilities(&[" Rust ".into()]).is_err());
        assert!(normalize_capabilities(&["Rust".into()]).is_err());
        assert!(normalize_capabilities(&["".into()]).is_err());
        assert!(
            normalize_capabilities(&["toolchain-x86_64".into()]).is_err(),
            "underscore rejected"
        );
        assert!(normalize_capabilities(&["a".repeat(33)].into_iter().collect::<Vec<_>>()).is_err());
    }

    #[test]
    fn eligibility_filters_disabled_unavailable_incapable_excluded() {
        let result = filter_eligible(&IntegratorAssignInput {
            required_capabilities: vec!["rust".into()],
            excluded: vec!["excluded".into()],
            ..input(vec![
                IntegratorCandidate {
                    enabled: false,
                    ..candidate("off", &["rust"])
                },
                IntegratorCandidate {
                    available: false,
                    ..candidate("away", &["rust"])
                },
                candidate("no-cap", &["python"]),
                candidate("ok", &["rust", "ios"]),
                candidate("excluded", &["rust"]),
            ])
        })
        .unwrap();
        assert_eq!(result.eligible, vec!["ok"]);
        assert_eq!(result.neutral, vec!["ok"]);
        assert!(result.neutral_integrator);
        assert!(result.no_candidate_reason.is_none());
    }

    #[test]
    fn eligibility_requires_every_requested_capability() {
        let result = filter_eligible(&IntegratorAssignInput {
            required_capabilities: vec!["rust".into(), "ios".into()],
            ..input(vec![
                candidate("a", &["rust"]),
                candidate("b", &["rust", "ios"]),
            ])
        })
        .unwrap();
        assert_eq!(result.eligible, vec!["b"]);
    }

    #[test]
    fn eligibility_prefers_neutral_and_reports_when_impossible() {
        let result = filter_eligible(&IntegratorAssignInput {
            conflict_authors: vec!["a".into()],
            ..input(vec![candidate("a", &["rust"]), candidate("b", &["rust"])])
        })
        .unwrap();
        assert_eq!(result.eligible, vec!["a", "b"]);
        assert_eq!(result.neutral, vec!["b"]);
        assert!(result.neutral_integrator);
        assert!(result.no_candidate_reason.is_none());

        // No neutral candidate: full pool is used and disclosed.
        let result = filter_eligible(&IntegratorAssignInput {
            conflict_authors: vec!["a".into(), "b".into()],
            ..input(vec![candidate("a", &["rust"]), candidate("b", &["rust"])])
        })
        .unwrap();
        assert_eq!(result.eligible, vec!["a", "b"]);
        assert!(result.neutral.is_empty());
        assert!(!result.neutral_integrator);
        assert!(result.no_candidate_reason.is_none());

        // Every candidate excluded: bounded escalation reason, not an error.
        let result = filter_eligible(&IntegratorAssignInput {
            excluded: vec!["a".into(), "b".into()],
            ..input(vec![candidate("a", &["rust"]), candidate("b", &["rust"])])
        })
        .unwrap();
        assert!(result.no_candidate_reason.is_some());
        assert!(result.eligible.is_empty());
    }

    #[test]
    fn eligibility_rejects_empty_duplicate_and_invalid_rosters() {
        assert!(filter_eligible(&input(vec![])).is_err());
        assert!(filter_eligible(&input(vec![candidate("a", &[]), candidate("a", &[])])).is_err());
        assert!(filter_eligible(&input(vec![candidate("bad/name", &[])])).is_err());
        let mut too_many = Vec::new();
        for i in 0..=INTEGRATOR_MAX_CANDIDATES {
            too_many.push(candidate(&format!("agent-{i}"), &[]));
        }
        assert!(filter_eligible(&input(too_many)).is_err());
        let mut input = input(vec![candidate("a", &[])]);
        input.task_summary = "x".repeat(INTEGRATOR_MAX_TASK_SUMMARY_BYTES + 1);
        assert!(filter_eligible(&input).is_err());
        input.task_summary = "   ".to_string();
        assert!(filter_eligible(&input).is_err());
    }

    #[test]
    fn ranking_is_deterministic_and_domain_separated() {
        // Six candidates keep permutation-collision probability negligible so
        // the domain-separation assertions cannot flake (1/720 per pair).
        let pool = vec![
            "agent-a".to_string(),
            "agent-b".to_string(),
            "agent-c".to_string(),
            "agent-d".to_string(),
            "agent-e".to_string(),
            "agent-f".to_string(),
        ];
        let fp = roster_fingerprint(&pool).unwrap();
        let first = rank_candidates(WS, SNAP_A, ASSIGNMENT, NONCE, &fp, &pool).unwrap();
        let second = rank_candidates(WS, SNAP_A, ASSIGNMENT, NONCE, &fp, &pool).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 6);
        assert!(first.iter().all(|name| pool.contains(name)));
        // Changing any input changes the order (domain separation).
        let with_snapshot = rank_candidates(WS, SNAP_B, ASSIGNMENT, NONCE, &fp, &pool).unwrap();
        let with_nonce = rank_candidates(
            WS,
            SNAP_A,
            ASSIGNMENT,
            &NONCE.replacen('f', "e", 1),
            &fp,
            &pool,
        )
        .unwrap();
        let with_fp = rank_candidates(
            WS,
            SNAP_A,
            ASSIGNMENT,
            NONCE,
            &fp.replacen('a', "b", 1),
            &pool,
        )
        .unwrap();
        let with_ws = rank_candidates("other-ws", SNAP_A, ASSIGNMENT, NONCE, &fp, &pool).unwrap();
        assert_ne!(first, with_snapshot);
        assert_ne!(first, with_nonce);
        assert_ne!(first, with_fp);
        assert_ne!(first, with_ws);
        // Roster fingerprint is stable across sort orders of the pool.
        let shuffled = vec![
            "agent-f".to_string(),
            "agent-a".to_string(),
            "agent-d".to_string(),
            "agent-c".to_string(),
            "agent-b".to_string(),
            "agent-e".to_string(),
        ];
        assert_eq!(fp, roster_fingerprint(&shuffled).unwrap());
    }

    #[test]
    fn ranking_rejects_malformed_ids() {
        let pool = vec!["a".to_string()];
        let fp = roster_fingerprint(&pool).unwrap();
        assert!(rank_candidates(WS, "short", ASSIGNMENT, NONCE, &fp, &pool).is_err());
        assert!(rank_candidates(WS, SNAP_A, "short", NONCE, &fp, &pool).is_err());
        assert!(rank_candidates(WS, SNAP_A, ASSIGNMENT, "short", &fp, &pool).is_err());
        assert!(rank_candidates(WS, SNAP_A, ASSIGNMENT, NONCE, "short", &pool).is_err());
        assert!(rank_candidates(WS, SNAP_A, ASSIGNMENT, NONCE, &fp, &[]).is_err());
    }

    /// Golden vector: fixed nonce must reproduce the same ranking on every
    /// platform. Computed once from the canonical implementation.
    #[test]
    fn ranking_golden_vector_is_stable() {
        let pool = vec![
            "linux-dev".to_string(),
            "mac-test".to_string(),
            "ci1".to_string(),
        ];
        let fp = roster_fingerprint(&pool).unwrap();
        let ranked =
            rank_candidates("golden-workspace", SNAP_A, ASSIGNMENT, NONCE, &fp, &pool).unwrap();
        // Golden vector: computed once from the canonical implementation and
        // fixed forever. Same inputs must reproduce the same ranking and
        // roster fingerprint on every supported platform.
        let expected = vec![
            "ci1".to_string(),
            "mac-test".to_string(),
            "linux-dev".to_string(),
        ];
        assert_eq!(ranked, expected);
        assert_eq!(
            fp,
            "26a359d7aceb46c7bfa48880140bf6624163e47098d2478cb8ee43f32408d9d1"
        );
    }

    /// Fairness regression: over many deterministic nonces every equal
    /// candidate is selected a comparable number of times (detects obvious
    /// bias without a flaky statistical test).
    #[test]
    fn distribution_over_many_nonces_is_fair() {
        let pool = vec![
            "agent-a".to_string(),
            "agent-b".to_string(),
            "agent-c".to_string(),
        ];
        let fp = roster_fingerprint(&pool).unwrap();
        let mut counts = std::collections::HashMap::new();
        for i in 0..1500u64 {
            let nonce = crate::hash_bytes(&format!("seed-{i}").into_bytes());
            let ranked = rank_candidates(WS, SNAP_A, ASSIGNMENT, &nonce, &fp, &pool).unwrap();
            *counts.entry(ranked[0].clone()).or_insert(0u32) += 1;
        }
        let total: u32 = counts.values().sum();
        assert_eq!(total, 1500);
        let expected = 500;
        for count in counts.values() {
            let deviation = i64::from(*count) - expected;
            // 20% tolerance around the equal-chance expectation; the draw is
            // deterministic so this can never flake.
            assert!(
                deviation.abs() <= expected / 5,
                "selection count {count} deviates too far from equal chance"
            );
        }
    }

    fn digest(assignment_id: &str) -> IntegratorDigest {
        IntegratorDigest {
            assignment_id: assignment_id.to_string(),
            integrator: "agent-b".to_string(),
            about_snapshot: SNAP_A.to_string(),
            inspected_snapshot: SNAP_A.to_string(),
            state: IntegratorOutcomeState::Completed,
            landed_paths: 12,
            resolved_conflicts: 3,
            remaining_conflicts: 0,
            verification: VerificationSummary {
                status: VerificationStatus::Passed,
                summary: "84 tests passed".to_string(),
            },
            outcome: "Integrated parser implementation and tests.".to_string(),
            risks: vec![],
            decision_required: None,
        }
    }

    #[test]
    fn digest_bounds_are_enforced() {
        assert!(validate_integrator_digest(&digest(ASSIGNMENT)).is_ok());
        let mut bad = digest(ASSIGNMENT);
        bad.outcome = "x".repeat(INTEGRATOR_DIGEST_FIELD_BYTES + 1);
        assert!(validate_integrator_digest(&bad).is_err());
        let mut bad = digest(ASSIGNMENT);
        bad.risks = vec!["r".repeat(INTEGRATOR_RISK_BYTES + 1)];
        assert!(validate_integrator_digest(&bad).is_err());
        let mut bad = digest(ASSIGNMENT);
        bad.risks = vec!["r".to_string(); INTEGRATOR_MAX_RISKS + 1];
        assert!(validate_integrator_digest(&bad).is_err());
        let mut bad = digest(ASSIGNMENT);
        bad.decision_required = Some("q".repeat(INTEGRATOR_DIGEST_FIELD_BYTES + 1));
        assert!(validate_integrator_digest(&bad).is_err());
        let mut bad = digest(ASSIGNMENT);
        bad.assignment_id = "short".to_string();
        assert!(validate_integrator_digest(&bad).is_err());
        let mut bad = digest(ASSIGNMENT);
        bad.inspected_snapshot = "short".to_string();
        assert!(validate_integrator_digest(&bad).is_err());
    }

    fn assignment_profile() -> IntegratorProfile {
        IntegratorProfile::Assignment {
            assignment_id: ASSIGNMENT.to_string(),
            attempt: 0,
            selected: "agent-b".to_string(),
            about_snapshot: SNAP_A.to_string(),
            roster_fingerprint: "b".repeat(64),
            neutral_integrator: true,
            task: "Integrate parser implementation and tests".to_string(),
        }
    }

    #[test]
    fn ffint1_profile_roundtrips_through_canonical_json() {
        for profile in [
            assignment_profile(),
            IntegratorProfile::Accepted {
                assignment_id: ASSIGNMENT.to_string(),
                attempt: 0,
                about_snapshot: SNAP_A.to_string(),
            },
            IntegratorProfile::Result {
                assignment_id: ASSIGNMENT.to_string(),
                attempt: 0,
                about_snapshot: SNAP_A.to_string(),
                digest: digest(ASSIGNMENT),
            },
            IntegratorProfile::Blocked {
                assignment_id: ASSIGNMENT.to_string(),
                attempt: 0,
                about_snapshot: SNAP_A.to_string(),
                reason: "Missing iOS toolchain".to_string(),
            },
        ] {
            let encoded = encode_integrator_profile(&profile).unwrap();
            assert!(encoded.starts_with("ffint1:"));
            assert!(encoded.len() <= crate::AGENT_MESSAGE_MAX_BODY_BYTES);
            assert_eq!(parse_integrator_profile(&encoded), Some(profile));
        }
    }

    #[test]
    fn ffint1_is_exact_canonical_compact_json() {
        let encoded = encode_integrator_profile(&assignment_profile()).unwrap();
        assert_eq!(
            encoded,
            concat!(
                "ffint1:{\"type\":\"assignment\",\"assignment_id\":\"0123456789abcdef0123456789abcdef\",",
                "\"attempt\":0,\"selected\":\"agent-b\",",
                "\"about_snapshot\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
                "\"roster_fingerprint\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",",
                "\"neutral_integrator\":true,",
                "\"task\":\"Integrate parser implementation and tests\"}"
            )
        );
    }

    #[test]
    fn ffint1_unknown_versions_and_malformed_payloads_are_harmless() {
        assert!(parse_integrator_profile("ffint2:{\"type\":\"assignment\"}").is_none());
        assert!(parse_integrator_profile("ffint1:not-json").is_none());
        assert!(parse_integrator_profile("plain body text").is_none());
        assert!(parse_integrator_profile("").is_none());
        let encoded = encode_integrator_profile(&assignment_profile()).unwrap();
        let spaced = encoded.replacen('"', " ", 1);
        assert!(parse_integrator_profile(&spaced).is_none());
        let malformed = IntegratorProfile::Accepted {
            assignment_id: "short".to_string(),
            attempt: 0,
            about_snapshot: SNAP_A.to_string(),
        };
        assert!(encode_integrator_profile(&malformed).is_err());
        let json = serde_json::to_string(&malformed).unwrap();
        assert!(parse_integrator_profile(&format!("ffint1:{json}")).is_none());
    }

    #[test]
    fn materialize_entry_types_serialize() {
        let result = ConflictMaterializeResult {
            about_snapshot: SNAP_A.to_string(),
            conflict_dir: "~/.feanorfs/workspaces/opaque/conflicts/materialize_1".to_string(),
            entries: vec![ConflictMaterializeEntry {
                path: "src/main.rs".to_string(),
                kind: ConflictKind::EditEdit,
                original_available: true,
                local_available: true,
                cloud_available: true,
                is_binary: false,
                already_materialized: false,
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ConflictMaterializeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, result);
        assert!(json.contains("edit_edit"));
    }

    #[test]
    fn assign_result_serde_roundtrip() {
        let result = IntegratorAssignResult {
            assignment_id: ASSIGNMENT.to_string(),
            about_snapshot: SNAP_A.to_string(),
            selected: "agent-b".to_string(),
            fallback_order: vec!["agent-a".to_string()],
            neutral_integrator: true,
            roster_fingerprint: "b".repeat(64),
            attempt: 0,
            request_message_id: SNAP_A.to_string(),
            state: IntegratorAssignmentState::Offered,
            task_summary: "Integrate parser implementation and tests".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: IntegratorAssignResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, result);
        assert!(json.contains("\"state\":\"offered\""));
    }
}
