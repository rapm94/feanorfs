//! Canonical encrypted work-intent contracts (`ffwork1`, schema 1).
//!
//! This module owns every type shared by the Rust SDK, CLI, JSON, C FFI,
//! TypeScript, MCP, and NDJSON event surfaces for the work-intent protocol:
//! the `ffwork1` profiles carried inside `ffmsg1` bodies, the explicit bound
//! constants, canonical encode/parse, exact-path / directory / supported-glob
//! overlap evaluation, and typed transition rejection. Adapters must never
//! reimplement canonicalization, validation, overlap, or lifecycle rules.
//!
//! The module is pure: no I/O, no wall clocks, no side effects. Parsing
//! returns `None` for unknown discriminators, unknown versions, malformed
//! payloads, or non-canonical JSON, and never partially applies a profile.

use crate::{is_safe_rel_path, is_valid_agent_name, is_valid_hash, AGENT_MESSAGE_MAX_BODY_BYTES};
use anyhow::{ensure, Context as _, Result};
use serde::{Deserialize, Serialize};

/// Discriminator prefix for work-intent profiles carried inside `ffmsg1.body`.
pub const WORK_PROFILE_DISCRIMINATOR: &str = "ffwork1";

/// Current wire schema version. Adding variants or changing field shapes
/// requires a schema/version decision and a fixture update in one change.
pub const WORK_SCHEMA_VERSION: u32 = 1;

/// Maximum encoded profile bytes: the whole profile must fit inside the
/// existing 8 KiB `ffmsg1` body bound.
pub const WORK_MAX_PROFILE_BYTES: usize = AGENT_MESSAGE_MAX_BODY_BYTES;

/// Maximum UTF-8 byte length of one canonical task id.
pub const WORK_MAX_TASK_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length of one concern.
pub const WORK_MAX_CONCERN_BYTES: usize = 256;
/// Maximum UTF-8 byte length of one capability identifier.
pub const WORK_MAX_CAPABILITY_BYTES: usize = 32;
/// Maximum UTF-8 byte length of one bounded reason.
pub const WORK_MAX_REASON_BYTES: usize = 512;
/// Maximum UTF-8 byte length of one completion outcome.
pub const WORK_MAX_OUTCOME_BYTES: usize = 512;
/// Maximum UTF-8 byte length of one verification summary.
pub const WORK_MAX_VERIFICATION_SUMMARY_BYTES: usize = 512;
/// Maximum number of paths in one scope.
pub const WORK_MAX_PATHS: usize = 64;
/// Maximum UTF-8 byte length of one exact/glob path entry.
pub const WORK_MAX_PATH_BYTES: usize = 1024;
/// Maximum aggregate UTF-8 bytes across all paths in one scope.
pub const WORK_MAX_PATHS_TOTAL_BYTES: usize = 4096;
/// Maximum number of concerns in one scope.
pub const WORK_MAX_CONCERNS: usize = 32;
/// Maximum number of dependencies in one scope.
pub const WORK_MAX_DEPENDENCIES: usize = 32;
/// Maximum number of capabilities in one proposal.
pub const WORK_MAX_CAPABILITIES: usize = 64;
/// Maximum number of causal/source references retained per applied record.
pub const WORK_MAX_SOURCE_REFS: usize = 16;
/// Maximum number of overlap entries in one explicit overlap acceptance.
pub const WORK_MAX_OVERLAP_ENTRIES: usize = 32;
/// Maximum number of active (non-terminal) tasks retained in the local
/// projection.
pub const WORK_MAX_ACTIVE_TASKS: usize = 64;
/// Maximum number of terminal tasks retained in bounded local history.
pub const WORK_MAX_TERMINAL_TASKS: usize = 512;
/// Maximum combined active + terminal projection entries retained locally.
pub const WORK_MAX_PROJECTION_ENTRIES: usize = WORK_MAX_ACTIVE_TASKS + WORK_MAX_TERMINAL_TASKS;
/// Maximum number of evidence records (losing branches, invalid decisions,
/// superseded transitions) retained locally.
pub const WORK_MAX_EVIDENCE: usize = 1024;
/// Maximum number of amendment records retained per proposal.
pub const WORK_MAX_AMENDMENTS: usize = 16;
/// Maximum number of seen message ids retained for causal-base satisfaction.
pub const WORK_MAX_SEEN: usize = 4096;
/// Maximum number of pending (not-yet-appliable) transitions retained for
/// re-examination on later observations.
pub const WORK_MAX_PENDING: usize = 256;
/// Maximum number of proposal records retained per task.
pub const WORK_MAX_PROPOSALS_PER_TASK: usize = 8;

/// Whether `value` is a canonical work task id: 1..=128 lowercase ASCII
/// letters, digits, `-`, or `_`.
#[must_use]
pub fn is_valid_task_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= WORK_MAX_TASK_ID_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether `value` is a canonical capability identifier (lowercase ASCII
/// letters, digits, or `-`).
#[must_use]
pub fn is_valid_capability(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= WORK_MAX_CAPABILITY_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether a scope entry is a supported directory-containment glob of the
/// exact form `<safe-rel-path>/**`. The `**` must be the final component and
/// the root must itself be a canonical portable path (which cannot contain
/// `*`). This is the entire supported glob subset: no other metacharacters,
/// no framework semantics.
#[must_use]
pub fn is_directory_glob(entry: &str) -> bool {
    let Some(root) = entry.strip_suffix("/**") else {
        return false;
    };
    !root.is_empty() && is_safe_rel_path(root)
}

/// Whether a scope entry is either an exact canonical path or the supported
/// directory-containment glob form.
#[must_use]
pub fn is_valid_scope_entry(entry: &str) -> bool {
    is_safe_rel_path(entry) || is_directory_glob(entry)
}

fn ensure_sorted_unique(list: &[String], label: &str, maximum: usize) -> Result<()> {
    ensure!(list.len() <= maximum, "{label} exceeds {maximum} entries");
    for pair in list.windows(2) {
        ensure!(
            pair[0] < pair[1],
            "{label} must be sorted ascending with no duplicates"
        );
    }
    Ok(())
}

fn ensure_bounded_text(
    value: &str,
    label: &str,
    maximum: usize,
    allow_control: bool,
) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum,
        "{label} must be non-empty and at most {maximum} bytes"
    );
    if !allow_control {
        ensure!(
            !value.chars().any(char::is_control),
            "{label} must not contain control characters"
        );
    }
    Ok(())
}

/// Validates and bounds one scope (paths, concerns, dependencies,
/// capabilities). Collections must already be sorted and unique; an invalid
/// signed profile is rejected, never silently normalized.
///
/// # Errors
/// Returns an error for any out-of-bounds, unsafe, duplicate, or unsorted
/// collection entry.
pub fn validate_work_scope(
    paths: &[String],
    concerns: &[String],
    dependencies: &[String],
    capabilities: &[String],
) -> Result<()> {
    ensure_sorted_unique(paths, "paths", WORK_MAX_PATHS)?;
    let mut total_path_bytes = 0usize;
    for path in paths {
        ensure!(
            is_valid_scope_entry(path),
            "path {path:?} is not a canonical portable path or supported `dir/**` glob"
        );
        ensure!(
            path.len() <= WORK_MAX_PATH_BYTES,
            "path {path:?} exceeds {WORK_MAX_PATH_BYTES} bytes"
        );
        total_path_bytes = total_path_bytes.saturating_add(path.len());
    }
    ensure!(
        total_path_bytes <= WORK_MAX_PATHS_TOTAL_BYTES,
        "aggregate path bytes exceed {WORK_MAX_PATHS_TOTAL_BYTES}"
    );
    ensure_sorted_unique(concerns, "concerns", WORK_MAX_CONCERNS)?;
    for concern in concerns {
        ensure!(
            concern.trim() == concern,
            "concern must be in canonical unpadded form"
        );
        ensure_bounded_text(concern, "concern", WORK_MAX_CONCERN_BYTES, false)?;
    }
    ensure_sorted_unique(dependencies, "dependencies", WORK_MAX_DEPENDENCIES)?;
    for dependency in dependencies {
        ensure!(
            is_valid_task_id(dependency),
            "dependency {dependency:?} is not a valid task id"
        );
    }
    ensure_sorted_unique(capabilities, "capabilities", WORK_MAX_CAPABILITIES)?;
    for capability in capabilities {
        ensure!(
            is_valid_capability(capability),
            "capability {capability:?} is not a valid lowercase identifier"
        );
    }
    Ok(())
}

/// State of one work proposal or derived task-level state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskState {
    /// Intent recorded; awaiting an applied coordinator decision.
    Proposed,
    /// A coordinator decision (accept/narrow/order/accept_overlap) applied.
    Accepted,
    /// Accepted changes reconciled with verification evidence attached.
    Settled,
    /// Terminal success.
    Completed,
    /// Terminal automation blocker (does not imply human escalation).
    Blocked,
    /// Author explicitly relinquished accepted overlap, preserving local work.
    Yielded,
    /// Coordinator explicitly rejected the proposal.
    Rejected,
}

impl WorkTaskState {
    /// Stable wire string for this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Accepted => "accepted",
            Self::Settled => "settled",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Yielded => "yielded",
            Self::Rejected => "rejected",
        }
    }
}

/// Bounded verification evidence attached by a settled profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkVerification {
    pub status: WorkVerificationStatus,
    pub summary: String,
}

/// Verification outcome status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkVerificationStatus {
    Passed,
    Failed,
    Skipped,
}

impl WorkVerificationStatus {
    /// Stable wire string for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Kind of one pure overlap between two scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkOverlapKind {
    /// The same exact path in both scopes.
    ExactPath,
    /// One scope's exact path contains the other's at a component boundary.
    DirectoryContainment,
    /// Supported `dir/**` glob overlap (root containment or identical root).
    GlobMatch,
    /// Identical non-empty concern in both scopes.
    SameConcern,
}

impl WorkOverlapKind {
    /// Stable wire string for this overlap kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactPath => "exact_path",
            Self::DirectoryContainment => "directory_containment",
            Self::GlobMatch => "glob_match",
            Self::SameConcern => "same_concern",
        }
    }
}

/// One pure overlap entry between two scopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlap {
    pub kind: WorkOverlapKind,
    /// Path entry from the first scope (None for concern overlaps).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_a: Option<String>,
    /// Path entry from the second scope (None for concern overlaps).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_b: Option<String>,
    /// Identical concern (only for `same_concern` overlaps).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concern: Option<String>,
}

fn is_path_under(child: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    child == dir
        || child
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn path_overlap_kind(a: &str, b: &str) -> Option<WorkOverlapKind> {
    let a_glob = a.strip_suffix("/**");
    let b_glob = b.strip_suffix("/**");
    match (a_glob, b_glob) {
        (None, None) => {
            if a == b {
                Some(WorkOverlapKind::ExactPath)
            } else if is_path_under(b, a) || is_path_under(a, b) {
                Some(WorkOverlapKind::DirectoryContainment)
            } else {
                None
            }
        }
        (Some(root_a), Some(root_b)) => {
            if root_a == root_b || is_path_under(root_b, root_a) || is_path_under(root_a, root_b) {
                Some(WorkOverlapKind::GlobMatch)
            } else {
                None
            }
        }
        (Some(root_a), None) => {
            if b == root_a || is_path_under(b, root_a) {
                Some(WorkOverlapKind::GlobMatch)
            } else {
                None
            }
        }
        (None, Some(root_b)) => {
            if a == root_b || is_path_under(a, root_b) {
                Some(WorkOverlapKind::GlobMatch)
            } else {
                None
            }
        }
    }
}

/// Pure overlap evaluation between two scopes: exact path matches, directory
/// containment, the supported `dir/**` glob subset, and identical non-empty
/// concerns. Deterministic: exact-path matches dominate containment matches
/// for the same pair, and duplicate entries are removed.
#[must_use]
pub fn evaluate_scope_overlap(
    paths_a: &[String],
    concerns_a: &[String],
    paths_b: &[String],
    concerns_b: &[String],
) -> Vec<WorkOverlap> {
    let mut out: Vec<WorkOverlap> = Vec::new();
    for pa in paths_a {
        for pb in paths_b {
            if let Some(kind) = path_overlap_kind(pa, pb) {
                let entry = WorkOverlap {
                    kind,
                    path_a: Some(pa.clone()),
                    path_b: Some(pb.clone()),
                    concern: None,
                };
                if !out.contains(&entry) {
                    out.push(entry);
                }
            }
        }
    }
    for ca in concerns_a {
        if concerns_b.iter().any(|cb| cb == ca) {
            let entry = WorkOverlap {
                kind: WorkOverlapKind::SameConcern,
                path_a: None,
                path_b: None,
                concern: Some(ca.clone()),
            };
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out
}

/// Whether one canonical changed path is covered by one scope entry.
///
/// The entry must be a canonical exact path or the supported `dir/**`
/// containment glob. A `dir/**` entry covers `dir` itself and every
/// descendant at any depth; an exact entry covers only that exact path.
#[must_use]
pub fn scope_entry_covers_path(entry: &str, path: &str) -> bool {
    if let Some(root) = entry.strip_suffix("/**") {
        is_path_under(path, root)
    } else {
        entry == path
    }
}

/// Whether one canonical changed path is covered by an accepted scope
/// (any exact path entry or supported `dir/**` containment glob).
#[must_use]
pub fn scope_covers_path(scope: &WorkScope, path: &str) -> bool {
    scope
        .paths
        .iter()
        .any(|entry| scope_entry_covers_path(entry, path))
}

/// One pure partition of canonical changed paths against an accepted scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopePathPartition {
    /// Paths covered by the accepted scope (safe to publish).
    pub in_scope: Vec<String>,
    /// Paths not covered (must remain local and unlanded).
    pub out_of_scope: Vec<String>,
}

/// Partitions canonical changed paths into in-scope and out-of-scope sets,
/// preserving the caller's deterministic order.
#[must_use]
pub fn partition_scope_paths(paths: &[String], scope: &WorkScope) -> ScopePathPartition {
    let mut partition = ScopePathPartition::default();
    for path in paths {
        if scope_covers_path(scope, path) {
            partition.in_scope.push(path.clone());
        } else {
            partition.out_of_scope.push(path.clone());
        }
    }
    partition
}

/// One change kind a scope expansion request is blocked on. Deduplication
/// and approval key on the exact operation set, never on the path set alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeChangeOperation {
    Add,
    Modify,
    Delete,
    ModeChange,
}

impl ScopeChangeOperation {
    /// Stable wire string of this operation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Modify => "modify",
            Self::Delete => "delete",
            Self::ModeChange => "mode_change",
        }
    }
}

/// Maximum number of operations in one scope change request.
pub const WORK_MAX_SCOPE_CHANGE_OPERATIONS: usize = 4;

/// Bounded body of one deduplicated `scope_change_requested` signal.
///
/// This is an ordinary encrypted `ffmsg1` message — never a reducer
/// transition. The reducer observes it as plain signal text (its id enters
/// the seen set, so later amendments remain causally linked), while the
/// runner, CLI, and human tooling parse it as this typed, bounded profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeChangeRequestProfile {
    /// Task whose accepted scope cannot cover the agent's local changes.
    pub task_id: String,
    /// Exact accepted intent message id the request refers to.
    pub intent_message_id: String,
    /// Closed operation set the request is blocked on (sorted, unique).
    #[serde(default)]
    pub operations: Vec<ScopeChangeOperation>,
    /// Bounded canonical paths/globs observed outside the accepted scope.
    pub paths: Vec<String>,
    /// Bounded concerns observed outside the accepted scope.
    pub concerns: Vec<String>,
    /// Bounded reason (e.g. which publication the request blocks).
    pub reason: String,
}

/// Discriminator prefix for `scope_change_requested` signal bodies.
pub const SCOPE_CHANGE_REQUEST_DISCRIMINATOR: &str = "ffwork1:scope_change_requested";

/// Validates and bounds one `scope_change_requested` profile.
///
/// # Errors
/// Returns an error for invalid ids, unsafe paths, or out-of-bounds fields.
pub fn validate_scope_change_request(profile: &ScopeChangeRequestProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "scope change request has an invalid task id"
    );
    ensure!(
        is_valid_hash(&profile.intent_message_id),
        "scope change request references an invalid intent message id"
    );
    ensure!(
        profile.operations.len() <= WORK_MAX_SCOPE_CHANGE_OPERATIONS,
        "scope change request exceeds the operation bound"
    );
    for pair in profile.operations.windows(2) {
        ensure!(
            pair[0] < pair[1],
            "scope change request operations must be sorted and unique"
        );
    }
    ensure!(
        profile.paths.len() <= WORK_MAX_PATHS,
        "scope change request exceeds the path bound"
    );
    let mut total = 0usize;
    for path in &profile.paths {
        ensure!(
            is_valid_scope_entry(path),
            "scope change request has an invalid path entry"
        );
        total = total.saturating_add(path.len());
    }
    ensure!(
        total <= WORK_MAX_PATHS_TOTAL_BYTES,
        "scope change request paths exceed their aggregate bound"
    );
    ensure!(
        profile.concerns.len() <= WORK_MAX_CONCERNS,
        "scope change request exceeds the concern bound"
    );
    for concern in &profile.concerns {
        ensure!(
            concern.len() <= WORK_MAX_CONCERN_BYTES,
            "scope change request concern exceeds its bound"
        );
    }
    ensure!(
        profile.reason.len() <= WORK_MAX_REASON_BYTES,
        "scope change request reason exceeds its bound"
    );
    Ok(())
}

/// Encodes one `scope_change_requested` profile as its typed signal body.
///
/// # Errors
/// Returns an error for invalid or out-of-bounds fields.
pub fn encode_scope_change_request(profile: &ScopeChangeRequestProfile) -> Result<String> {
    validate_scope_change_request(profile)?;
    let bytes = serde_json::to_vec(profile).context("serialize scope change request profile")?;
    ensure!(
        bytes.len() <= WORK_MAX_PROFILE_BYTES,
        "scope change request profile exceeds the signal body bound"
    );
    let mut body =
        String::with_capacity(SCOPE_CHANGE_REQUEST_DISCRIMINATOR.len() + 1 + bytes.len());
    body.push_str(SCOPE_CHANGE_REQUEST_DISCRIMINATOR);
    body.push(':');
    body.push_str(std::str::from_utf8(&bytes).expect("serialized profile is valid UTF-8"));
    Ok(body)
}

/// Parses one `scope_change_requested` signal body. Returns `None` for any
/// other discriminator, malformed JSON, unknown fields, or unsafe entries.
#[must_use]
pub fn parse_scope_change_request(body: &str) -> Option<ScopeChangeRequestProfile> {
    let rest = body.strip_prefix(SCOPE_CHANGE_REQUEST_DISCRIMINATOR)?;
    let payload = rest.strip_prefix(':')?;
    let profile: ScopeChangeRequestProfile = serde_json::from_str(payload).ok()?;
    validate_scope_change_request(&profile).ok()?;
    Some(profile)
}

/// One accepted overlap entry (explicit coordinator acceptance of elevated
/// overlap risk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapAcceptance {
    pub kind: WorkOverlapKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_a: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_b: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concern: Option<String>,
}

impl WorkOverlapAcceptance {
    /// Converts a computed overlap entry into an acceptance entry.
    #[must_use]
    pub fn from_overlap(overlap: &WorkOverlap) -> Self {
        Self {
            kind: overlap.kind,
            path_a: overlap.path_a.clone(),
            path_b: overlap.path_b.clone(),
            concern: overlap.concern.clone(),
        }
    }
}

/// Coordinator decision kinds for one exact proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkDecisionKind {
    /// Accept the proposal's scope as-is.
    Accept(WorkDecisionAccept),
    /// Reject the proposal; the author may re-propose with a higher sequence.
    Reject(WorkDecisionReject),
    /// Accept with an explicitly reduced scope (must stay within the
    /// proposal's declared scope).
    Narrow(WorkDecisionNarrow),
    /// Accept and record an explicit relative ordering against another
    /// proposal.
    Order(WorkDecisionOrder),
    /// Accept while explicitly accepting elevated overlap risk.
    AcceptOverlap(WorkDecisionAcceptOverlap),
}

impl WorkDecisionKind {
    /// Stable wire tag of this decision kind.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Accept(_) => "accept",
            Self::Reject(_) => "reject",
            Self::Narrow(_) => "narrow",
            Self::Order(_) => "order",
            Self::AcceptOverlap(_) => "accept_overlap",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecisionAccept {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecisionReject {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecisionNarrow {
    pub paths: Vec<String>,
    pub concerns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecisionOrder {
    /// Proposal message id this proposal is sequenced after (None = no
    /// ordering constraint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecisionAcceptOverlap {
    pub overlap: Vec<WorkOverlapAcceptance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Bounded scope carried by proposals and accepted projections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkScope {
    pub paths: Vec<String>,
    pub concerns: Vec<String>,
    pub dependencies: Vec<String>,
}

/// `work_intent` profile: proposes task, agent, causal base, paths,
/// concerns, dependencies, capabilities, and author sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkIntentProfile {
    pub task_id: String,
    pub agent: String,
    pub sequence: u64,
    /// Immutable message id this proposal builds on; None for a fresh
    /// proposal. Must be a full 64-hex signal id when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causal_base: Option<String>,
    /// Named coordinator/dispatcher identity; decisions from this identity
    /// (or the operating context) are authorized for this proposal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    pub paths: Vec<String>,
    #[serde(default)]
    pub concerns: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// `work_decision` profile: coordinator accepts, rejects, narrows, sequences,
/// or explicitly accepts elevated overlap risk for one exact proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecisionProfile {
    /// Exact proposal message id this decision targets.
    pub proposal_message_id: String,
    pub kind: WorkDecisionKind,
}

/// `work_amendment` profile: proposes changed scope/dependencies against one
/// accepted intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkAmendmentProfile {
    pub task_id: String,
    /// Exact accepted intent message id being amended.
    pub intent_message_id: String,
    pub sequence: u64,
    /// Replacement paths; None keeps the accepted scope unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concerns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<String>>,
    /// Exact applied coordinator decision message id approving an expansion
    /// beyond the original declared scope. Required only when the amended
    /// scope is not contained in the original scope; a proposal can never
    /// enlarge its own grant without this reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_decision_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `work_yield` profile: author explicitly relinquishes accepted overlap
/// while preserving local work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkYieldProfile {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `work_settled` profile: accepted changes are reconciled and verification
/// evidence is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSettledProfile {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    /// Full 64-hex snapshot actually inspected/reconciled.
    pub inspected_snapshot: String,
    pub verification: WorkVerification,
}

/// `work_completed` profile: terminal success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCompletedProfile {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    pub outcome: String,
}

/// `work_blocked` profile: terminal automation blocker; does not imply human
/// escalation by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBlockedProfile {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    pub reason: String,
}

/// `work_superseded` profile: authorized coordinator replacement of a prior
/// decision, returning the proposal to the pending state so a new decision
/// can be issued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSupersededProfile {
    pub task_id: String,
    /// Exact proposal message id whose decision is replaced.
    pub proposal_message_id: String,
    /// Exact decision message id being replaced; must be the currently
    /// applied decision for the proposal.
    pub superseded_decision_message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `ffwork1` profile carried inside an `ffmsg1` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkProfile {
    WorkIntent(WorkIntentProfile),
    WorkDecision(WorkDecisionProfile),
    WorkAmendment(WorkAmendmentProfile),
    WorkYield(WorkYieldProfile),
    WorkSettled(WorkSettledProfile),
    WorkCompleted(WorkCompletedProfile),
    WorkBlocked(WorkBlockedProfile),
    WorkSuperseded(WorkSupersededProfile),
}

impl WorkProfile {
    /// Stable wire tag of this variant.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::WorkIntent(_) => "work_intent",
            Self::WorkDecision(_) => "work_decision",
            Self::WorkAmendment(_) => "work_amendment",
            Self::WorkYield(_) => "work_yield",
            Self::WorkSettled(_) => "work_settled",
            Self::WorkCompleted(_) => "work_completed",
            Self::WorkBlocked(_) => "work_blocked",
            Self::WorkSuperseded(_) => "work_superseded",
        }
    }

    /// Task id this profile concerns.
    #[must_use]
    pub fn task_id(&self) -> &str {
        match self {
            Self::WorkIntent(p) => &p.task_id,
            Self::WorkDecision(_) => "",
            Self::WorkAmendment(p) => &p.task_id,
            Self::WorkYield(p) => &p.task_id,
            Self::WorkSettled(p) => &p.task_id,
            Self::WorkCompleted(p) => &p.task_id,
            Self::WorkBlocked(p) => &p.task_id,
            Self::WorkSuperseded(p) => &p.task_id,
        }
    }

    /// Author sequence for author-authored profiles; decisions and
    /// superseded profiles do not carry one (they key by message id).
    #[must_use]
    pub fn sequence(&self) -> Option<u64> {
        match self {
            Self::WorkIntent(p) => Some(p.sequence),
            Self::WorkAmendment(p) => Some(p.sequence),
            Self::WorkYield(p) => Some(p.sequence),
            Self::WorkSettled(p) => Some(p.sequence),
            Self::WorkCompleted(p) => Some(p.sequence),
            Self::WorkBlocked(p) => Some(p.sequence),
            Self::WorkDecision(_) | Self::WorkSuperseded(_) => None,
        }
    }
}

fn validate_reason(value: &Option<String>, label: &str) -> Result<()> {
    if let Some(reason) = value {
        ensure!(
            !reason.trim().is_empty() && reason.len() <= WORK_MAX_REASON_BYTES,
            "{label} must be non-empty and at most {WORK_MAX_REASON_BYTES} bytes"
        );
        ensure!(
            !reason.chars().any(char::is_control),
            "{label} must not contain control characters"
        );
    }
    Ok(())
}

/// Validates a `work_intent` profile.
///
/// # Errors
/// Returns an error for invalid ids, names, sequences, or scope fields.
pub fn validate_work_intent(profile: &WorkIntentProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "work intent task_id must be a canonical task id"
    );
    ensure!(
        is_valid_agent_name(&profile.agent),
        "work intent agent must be a valid agent name"
    );
    ensure!(profile.sequence >= 1, "work intent sequence must be >= 1");
    if let Some(base) = &profile.causal_base {
        ensure!(
            is_valid_hash(base),
            "work intent causal_base must be a full 64-hex message id"
        );
    }
    if let Some(coordinator) = &profile.coordinator {
        ensure!(
            is_valid_agent_name(coordinator) && coordinator != "*",
            "work intent coordinator must be a valid agent name"
        );
    }
    validate_work_scope(
        &profile.paths,
        &profile.concerns,
        &profile.dependencies,
        &profile.capabilities,
    )
}

fn validate_overlap_entries(entries: &[WorkOverlapAcceptance]) -> Result<()> {
    ensure!(
        entries.len() <= WORK_MAX_OVERLAP_ENTRIES,
        "overlap acceptance exceeds {WORK_MAX_OVERLAP_ENTRIES} entries"
    );
    for entry in entries {
        match entry.kind {
            WorkOverlapKind::SameConcern => {
                ensure!(
                    entry.path_a.is_none() && entry.path_b.is_none(),
                    "concern overlap entries must not carry paths"
                );
                ensure!(
                    entry
                        .concern
                        .as_deref()
                        .is_some_and(|c| !c.is_empty() && c.len() <= WORK_MAX_CONCERN_BYTES),
                    "concern overlap entry must carry a bounded concern"
                );
            }
            _ => {
                ensure!(
                    entry.concern.is_none()
                        && entry.path_a.is_some()
                        && entry.path_b.is_some()
                        && entry.path_a.as_deref().is_some_and(is_valid_scope_entry)
                        && entry.path_b.as_deref().is_some_and(is_valid_scope_entry),
                    "path overlap entries must carry two valid scope entries"
                );
            }
        }
    }
    Ok(())
}

fn validate_decision_kind(kind: &WorkDecisionKind) -> Result<()> {
    match kind {
        WorkDecisionKind::Accept(inner) => validate_reason(&inner.reason, "accept reason"),
        WorkDecisionKind::Reject(inner) => {
            ensure_bounded_text(&inner.reason, "reject reason", WORK_MAX_REASON_BYTES, false)
        }
        WorkDecisionKind::Narrow(inner) => {
            validate_work_scope(&inner.paths, &inner.concerns, &[], &[])?;
            validate_reason(&inner.reason, "narrow reason")
        }
        WorkDecisionKind::Order(inner) => {
            if let Some(after) = &inner.after {
                ensure!(
                    is_valid_hash(after),
                    "order after must be a full 64-hex proposal message id"
                );
            }
            validate_reason(&inner.reason, "order reason")
        }
        WorkDecisionKind::AcceptOverlap(inner) => {
            validate_overlap_entries(&inner.overlap)?;
            validate_reason(&inner.reason, "overlap reason")
        }
    }
}

/// Validates a `work_decision` profile.
///
/// # Errors
/// Returns an error for invalid proposal references or decision fields.
pub fn validate_work_decision(profile: &WorkDecisionProfile) -> Result<()> {
    ensure!(
        is_valid_hash(&profile.proposal_message_id),
        "work decision proposal_message_id must be a full 64-hex message id"
    );
    validate_decision_kind(&profile.kind)
}

fn validate_amendment_scope(
    value: &Option<Vec<String>>,
    label: &str,
    maximum: usize,
) -> Result<()> {
    if let Some(list) = value {
        ensure_sorted_unique(list, label, maximum)?;
        for entry in list {
            ensure!(
                is_valid_scope_entry(entry),
                "amendment {label} entry {entry:?} is not a canonical path or supported glob"
            );
            ensure!(
                entry.len() <= WORK_MAX_PATH_BYTES,
                "amendment {label} entry exceeds {WORK_MAX_PATH_BYTES} bytes"
            );
        }
        let total: usize = list.iter().map(String::len).sum();
        ensure!(
            total <= WORK_MAX_PATHS_TOTAL_BYTES,
            "amendment aggregate {label} bytes exceed {WORK_MAX_PATHS_TOTAL_BYTES}"
        );
    }
    Ok(())
}

/// Validates a `work_amendment` profile.
///
/// # Errors
/// Returns an error for invalid ids, sequences, or scope fields.
pub fn validate_work_amendment(profile: &WorkAmendmentProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "work amendment task_id must be a canonical task id"
    );
    ensure!(
        is_valid_hash(&profile.intent_message_id),
        "work amendment intent_message_id must be a full 64-hex message id"
    );
    ensure!(
        profile.sequence >= 1,
        "work amendment sequence must be >= 1"
    );
    validate_amendment_scope(&profile.paths, "paths", WORK_MAX_PATHS)?;
    if let Some(concerns) = &profile.concerns {
        ensure_sorted_unique(concerns, "concerns", WORK_MAX_CONCERNS)?;
        for concern in concerns {
            ensure!(
                concern.trim() == concern,
                "amendment concern must be in canonical unpadded form"
            );
            ensure_bounded_text(concern, "concern", WORK_MAX_CONCERN_BYTES, false)?;
        }
    }
    if let Some(dependencies) = &profile.dependencies {
        ensure_sorted_unique(dependencies, "dependencies", WORK_MAX_DEPENDENCIES)?;
        for dependency in dependencies {
            ensure!(
                is_valid_task_id(dependency),
                "amendment dependency {dependency:?} is not a valid task id"
            );
        }
    }
    if let Some(approval) = &profile.approval_decision_id {
        ensure!(
            is_valid_hash(approval),
            "amendment approval_decision_id must be a full 64-hex message id"
        );
    }
    validate_reason(&profile.reason, "amendment reason")
}

/// Validates a `work_yield` profile.
///
/// # Errors
/// Returns an error for invalid ids, sequences, or reasons.
pub fn validate_work_yield(profile: &WorkYieldProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "work yield task_id must be a canonical task id"
    );
    ensure!(
        is_valid_hash(&profile.intent_message_id),
        "work yield intent_message_id must be a full 64-hex message id"
    );
    ensure!(profile.sequence >= 1, "work yield sequence must be >= 1");
    validate_reason(&profile.reason, "yield reason")
}

fn validate_verification(verification: &WorkVerification) -> Result<()> {
    ensure_bounded_text(
        &verification.summary,
        "verification summary",
        WORK_MAX_VERIFICATION_SUMMARY_BYTES,
        false,
    )
}

/// Validates a `work_settled` profile.
///
/// # Errors
/// Returns an error for invalid ids, sequences, snapshots, or verification.
pub fn validate_work_settled(profile: &WorkSettledProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "work settled task_id must be a canonical task id"
    );
    ensure!(
        is_valid_hash(&profile.intent_message_id),
        "work settled intent_message_id must be a full 64-hex message id"
    );
    ensure!(profile.sequence >= 1, "work settled sequence must be >= 1");
    ensure!(
        is_valid_hash(&profile.inspected_snapshot),
        "work settled inspected_snapshot must be a full 64-hex snapshot id"
    );
    validate_verification(&profile.verification)
}

/// Validates a `work_completed` profile.
///
/// # Errors
/// Returns an error for invalid ids, sequences, or outcomes.
pub fn validate_work_completed(profile: &WorkCompletedProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "work completed task_id must be a canonical task id"
    );
    ensure!(
        is_valid_hash(&profile.intent_message_id),
        "work completed intent_message_id must be a full 64-hex message id"
    );
    ensure!(
        profile.sequence >= 1,
        "work completed sequence must be >= 1"
    );
    ensure_bounded_text(
        &profile.outcome,
        "completion outcome",
        WORK_MAX_OUTCOME_BYTES,
        false,
    )
}

/// Validates a `work_blocked` profile.
///
/// # Errors
/// Returns an error for invalid ids, sequences, or reasons.
pub fn validate_work_blocked(profile: &WorkBlockedProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "work blocked task_id must be a canonical task id"
    );
    ensure!(
        is_valid_hash(&profile.intent_message_id),
        "work blocked intent_message_id must be a full 64-hex message id"
    );
    ensure!(profile.sequence >= 1, "work blocked sequence must be >= 1");
    ensure_bounded_text(
        &profile.reason,
        "block reason",
        WORK_MAX_REASON_BYTES,
        false,
    )
}

/// Validates a `work_superseded` profile.
///
/// # Errors
/// Returns an error for invalid ids or reasons.
pub fn validate_work_superseded(profile: &WorkSupersededProfile) -> Result<()> {
    ensure!(
        is_valid_task_id(&profile.task_id),
        "work superseded task_id must be a canonical task id"
    );
    ensure!(
        is_valid_hash(&profile.proposal_message_id),
        "work superseded proposal_message_id must be a full 64-hex message id"
    );
    ensure!(
        is_valid_hash(&profile.superseded_decision_message_id),
        "work superseded decision reference must be a full 64-hex message id"
    );
    validate_reason(&profile.reason, "supersede reason")
}

/// Validates one complete `ffwork1` profile.
///
/// # Errors
/// Returns an error for any invalid or out-of-bounds field.
pub fn validate_work_profile(profile: &WorkProfile) -> Result<()> {
    match profile {
        WorkProfile::WorkIntent(inner) => validate_work_intent(inner),
        WorkProfile::WorkDecision(inner) => validate_work_decision(inner),
        WorkProfile::WorkAmendment(inner) => validate_work_amendment(inner),
        WorkProfile::WorkYield(inner) => validate_work_yield(inner),
        WorkProfile::WorkSettled(inner) => validate_work_settled(inner),
        WorkProfile::WorkCompleted(inner) => validate_work_completed(inner),
        WorkProfile::WorkBlocked(inner) => validate_work_blocked(inner),
        WorkProfile::WorkSuperseded(inner) => validate_work_superseded(inner),
    }
}

/// Encodes one `ffwork1` profile as `ffwork1:` + canonical compact JSON.
/// The resulting string must fit inside the 8 KiB `ffmsg1` body bound.
///
/// # Errors
/// Returns an error for invalid ids, names, sequences, or oversized fields.
pub fn encode_work_profile(profile: &WorkProfile) -> Result<String> {
    validate_work_profile(profile)?;
    let json = serde_json::to_string(profile)?;
    let encoded = format!("{WORK_PROFILE_DISCRIMINATOR}:{json}");
    ensure!(
        encoded.len() <= WORK_MAX_PROFILE_BYTES,
        "ffwork1 profile exceeds the 8 KiB signal body bound"
    );
    Ok(encoded)
}

/// Parses an `ffwork1` profile. Returns `None` for unknown versions,
/// malformed payloads, unknown fields, unsafe paths, duplicate/unsorted
/// collections, invalid ids, or non-canonical JSON. Unknown future versions
/// remain ordinary signal text and cannot break typed inbox reads.
#[must_use]
pub fn parse_work_profile(body: &str) -> Option<WorkProfile> {
    if body.len() > WORK_MAX_PROFILE_BYTES {
        return None;
    }
    let json = body.strip_prefix(WORK_PROFILE_DISCRIMINATOR)?;
    let json = json.strip_prefix(':')?;
    let profile: WorkProfile = serde_json::from_str(json).ok()?;
    if serde_json::to_string(&profile).ok()? != json {
        return None;
    }
    if validate_work_profile(&profile).is_err() {
        return None;
    }
    Some(profile)
}

/// Typed transition rejection reasons (internal; never on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRejectReason {
    /// The transition references a task with no applied proposal chain.
    UnknownTask,
    /// The transition is not allowed from the proposal's current state.
    WrongState {
        expected: &'static str,
        found: Option<WorkTaskState>,
    },
    /// The transition's sender is not the proposal's author.
    NotAuthor,
    /// The intent's causal base is not satisfiable within the closure.
    MissingCausalBase,
    /// A decision targets a proposal whose intent is not applied.
    MissingProposal,
    /// An author transition targets an intent message id that is not applied.
    MissingIntent,
    /// The decision/supersede sender is not the authorized coordinator.
    UnauthorizedCoordinator,
    /// The proposal already has an applied decision.
    DecisionAlreadyApplied,
    /// The supersede reference does not match the applied decision.
    SupersededDecisionUnknown,
    /// A dependency names the proposal's own task.
    SelfDependency,
    /// Applying the dependency edge would create a cycle.
    DependencyCycle,
    /// An overlap claim is not derivable from the pure overlap evaluation.
    InvalidOverlapClaim,
    /// A narrow decision extends beyond the proposal's declared scope.
    NarrowOutsideScope,
    /// An author transition would decrease or repeat the author sequence.
    SequenceDecreased,
    /// An amendment expands the accepted scope beyond the original declared
    /// scope without referencing an applied coordinator approval decision.
    AmendmentExpandsScopeWithoutApproval,
}

impl WorkRejectReason {
    /// Stable evidence disposition string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::UnknownTask => "unknown_task",
            Self::WrongState { .. } => "wrong_state",
            Self::MissingCausalBase => "missing_causal_base",
            Self::NotAuthor => "not_author",
            Self::MissingProposal => "missing_proposal",
            Self::MissingIntent => "missing_intent",
            Self::UnauthorizedCoordinator => "unauthorized_coordinator",
            Self::DecisionAlreadyApplied => "decision_already_applied",
            Self::SupersededDecisionUnknown => "superseded_decision_unknown",
            Self::SelfDependency => "self_dependency",
            Self::DependencyCycle => "dependency_cycle",
            Self::InvalidOverlapClaim => "invalid_overlap_claim",
            Self::NarrowOutsideScope => "narrow_outside_scope",
            Self::SequenceDecreased => "sequence_decreased",
            Self::AmendmentExpandsScopeWithoutApproval => {
                "amendment_expands_scope_without_approval"
            }
        }
    }
}

/// Pure transition validation for author-authored transitions.
///
/// `current` is the state of the proposal record the transition targets
/// (None when the proposal/intent is unknown). Returns the typed rejection
/// reason, or `None` when the transition is structurally allowed from that
/// state. The reducer additionally enforces key/sequence/causal rules.
#[must_use]
pub fn transition_rejection(
    current: Option<WorkTaskState>,
    profile: &WorkProfile,
) -> Option<WorkRejectReason> {
    match profile {
        WorkProfile::WorkIntent(_) => None,
        WorkProfile::WorkDecision(_) => match current {
            Some(WorkTaskState::Proposed) => None,
            Some(_) => Some(WorkRejectReason::DecisionAlreadyApplied),
            None => Some(WorkRejectReason::MissingProposal),
        },
        WorkProfile::WorkAmendment(_) => match current {
            Some(WorkTaskState::Accepted) => None,
            Some(found) => Some(WorkRejectReason::WrongState {
                expected: "accepted",
                found: Some(found),
            }),
            None => Some(WorkRejectReason::MissingIntent),
        },
        WorkProfile::WorkYield(_) => match current {
            Some(WorkTaskState::Accepted) => None,
            Some(found) => Some(WorkRejectReason::WrongState {
                expected: "accepted",
                found: Some(found),
            }),
            None => Some(WorkRejectReason::MissingIntent),
        },
        WorkProfile::WorkSettled(_) => match current {
            Some(WorkTaskState::Accepted | WorkTaskState::Yielded) => None,
            Some(found) => Some(WorkRejectReason::WrongState {
                expected: "accepted or yielded",
                found: Some(found),
            }),
            None => Some(WorkRejectReason::MissingIntent),
        },
        WorkProfile::WorkCompleted(_) => match current {
            Some(WorkTaskState::Settled) => None,
            Some(found) => Some(WorkRejectReason::WrongState {
                expected: "settled",
                found: Some(found),
            }),
            None => Some(WorkRejectReason::MissingIntent),
        },
        WorkProfile::WorkBlocked(_) => match current {
            Some(WorkTaskState::Accepted | WorkTaskState::Yielded) => None,
            Some(found) => Some(WorkRejectReason::WrongState {
                expected: "accepted or yielded",
                found: Some(found),
            }),
            None => Some(WorkRejectReason::MissingIntent),
        },
        WorkProfile::WorkSuperseded(_) => match current {
            Some(WorkTaskState::Accepted | WorkTaskState::Rejected) => None,
            Some(_) => Some(WorkRejectReason::DecisionAlreadyApplied),
            None => Some(WorkRejectReason::MissingProposal),
        },
    }
}

/// Sender-side input for `Workspace::work_propose`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkProposeInput {
    pub task_id: String,
    /// Proposal author; defaults to the sender identity when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Author sequence; must be greater than every prior intent sequence for
    /// the same (task, agent).
    pub sequence: u64,
    /// Immutable message id this proposal builds on (causal base).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causal_base: Option<String>,
    /// Named coordinator identity whose decisions are authorized for this
    /// proposal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    pub paths: Vec<String>,
    #[serde(default)]
    pub concerns: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Snapshot this proposal concerns; defaults to the current workspace head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_snapshot: Option<String>,
    /// Recipient override; defaults to the named coordinator or `*`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// Sender-side input for `Workspace::work_decide`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecideInput {
    /// Exact proposal message id this decision targets.
    pub proposal_message_id: String,
    pub kind: WorkDecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_snapshot: Option<String>,
    /// Recipient override; defaults to `*`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Explicit sender for controlled automation; CLI defaults to
    /// FEANORFS_AGENT or human.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sender-side input for `Workspace::work_amend`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkAmendInput {
    pub task_id: String,
    /// Exact accepted intent message id being amended.
    pub intent_message_id: String,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concerns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Explicit sender for controlled automation; CLI defaults to
    /// FEANORFS_AGENT or human.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sender-side input for `Workspace::work_yield`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkYieldInput {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Explicit sender for controlled automation; CLI defaults to
    /// FEANORFS_AGENT or human.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sender-side input for `Workspace::work_settle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSettleInput {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    /// Full 64-hex snapshot actually inspected/reconciled.
    pub inspected_snapshot: String,
    pub verification: WorkVerification,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Explicit sender for controlled automation; CLI defaults to
    /// FEANORFS_AGENT or human.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sender-side input for `Workspace::work_complete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCompleteInput {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Explicit sender for controlled automation; CLI defaults to
    /// FEANORFS_AGENT or human.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sender-side input for `Workspace::work_block`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBlockInput {
    pub task_id: String,
    pub intent_message_id: String,
    pub sequence: u64,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Explicit sender for controlled automation; CLI defaults to
    /// FEANORFS_AGENT or human.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sender-side input for `Workspace::work_status`.
///
/// Coordinator authority is never derived from this input: it comes from
/// the authenticated protocol state (the proposal-named coordinator, or the
/// proposal author for coordinator-less proposals).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkStatusInput {
    /// Request a bounded deterministic rebuild from an empty projection
    /// (cursor reset). The rebuilt projection becomes complete again when
    /// the bounded scan covered the closure without bound exhaustion.
    #[serde(default)]
    pub rebuild: bool,
}

/// Result of sending one work-intent profile (propose/decide/amend/yield/
/// settle/complete/block). The `state` is the state this profile transitions
/// to *when applied by the reducer*; a sent proposal is never a claim of
/// acceptance until a coordinator decision is observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSendResult {
    /// Immutable signal snapshot id of the sent profile.
    pub message_id: String,
    /// Caller-selected snapshot context (defaults to the observed head).
    pub about_snapshot: String,
    pub task_id: String,
    /// Sender identity used for the signal.
    pub agent: String,
    /// Wire tag of the sent profile (e.g. `work_intent`, `work_decision`).
    pub profile: String,
    /// Target state this profile expresses when applied.
    pub state: WorkTaskState,
    /// Scope the profile itself declares (narrowed/amended fields; empty for
    /// profiles without scope fields).
    pub scope: WorkScope,
    /// Exact message ids this profile references (causal base, intent,
    /// proposal, ordering, superseded decision).
    pub causal_refs: Vec<String>,
    /// Explicitly accepted overlap entries (accept_overlap decisions only).
    pub overlap: Vec<WorkOverlapAcceptance>,
    /// Always false for sends; status reports the projection's completeness.
    pub projection_incomplete: bool,
    /// Authenticated applied ancestry: exact message ids whose transitions
    /// were applied by the reducer. Causal-base satisfaction and admission
    /// use this set, never the bounded observation cache. Bound exhaustion
    /// marks the projection incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_message_ids: Vec<String>,
}

/// One proposal record rendered for status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkProposalStatus {
    pub agent: String,
    pub state: WorkTaskState,
    pub sequence: u64,
    /// Exact intent message id (the proposal id decisions reference).
    pub intent_message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    /// Current accepted scope (post-amendment).
    pub accepted_scope: WorkScope,
    /// Advertised capabilities from the applied intent, preserved through
    /// every transition and rebuild so admission and designation can use the
    /// current authenticated capability set.
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<WorkDecisionStatus>,
    pub accepted_overlap: Vec<WorkOverlapAcceptance>,
    pub amendments: Vec<WorkAmendmentStatus>,
    /// Exact message ids this record's transitions referenced.
    pub causal_refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspected_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<WorkVerification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Exact message id of the last applied transition.
    pub source_message_id: String,
    /// Display/liveness hint only; never drives decisions.
    pub updated_at_ms: i64,
}

/// One applied coordinator decision rendered for status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkDecisionStatus {
    pub message_id: String,
    pub coordinator: String,
    pub kind: WorkDecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordered_after: Option<String>,
}

/// One applied amendment rendered for status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAmendmentStatus {
    pub message_id: String,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One task rendered for status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkTaskStatus {
    pub task_id: String,
    /// Derived task-level state (highest-priority proposal state; terminal
    /// states dominate; ties break by sequence then intent message id).
    pub state: WorkTaskState,
    pub proposals: Vec<WorkProposalStatus>,
}

/// Result of `Workspace::work_status` (observe + reducer projection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStatusResult {
    /// Workspace head observed by this read; pass back as the next cursor.
    pub cursor: String,
    pub cursor_reset: bool,
    /// True when the closure was truncated (cursor reset or bound
    /// exhaustion); acceptance is not fully provable while true.
    pub projection_incomplete: bool,
    /// Number of new signals processed by this pass.
    pub messages_processed: usize,
    pub tasks: Vec<WorkTaskStatus>,
    /// Number of retained evidence records (losing branches, invalid
    /// transitions, superseded decisions).
    pub evidence_count: usize,
    /// Number of transitions dropped by bound exhaustion.
    pub dropped_count: u64,
    /// Display/liveness hint only; never drives decisions.
    pub updated_at_ms: i64,
    /// Authenticated applied ancestry: exact message ids whose transitions
    /// were applied by the reducer. Causal-base satisfaction and admission
    /// use this set, never the bounded observation cache. Bound exhaustion
    /// marks the projection incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_message_ids: Vec<String>,
}

/// Canonical `ffwork1` fixtures — update only with a schema/version decision.
pub mod work_fixtures {
    use super::*;

    pub(crate) fn hex64(byte: u8) -> String {
        std::iter::repeat_n(byte as char, 64).collect()
    }

    fn encode(profile: WorkProfile) -> String {
        encode_work_profile(&profile).expect("fixture profile must encode")
    }

    /// Canonical `work_intent` proposal by `linux-dev` for `parser-impl`.
    pub fn work_intent_json() -> String {
        encode(WorkProfile::WorkIntent(WorkIntentProfile {
            task_id: "parser-impl".to_string(),
            agent: "linux-dev".to_string(),
            sequence: 1,
            causal_base: None,
            coordinator: Some("human".to_string()),
            paths: vec!["src/parser.rs".to_string(), "tests/parser.rs".to_string()],
            concerns: vec!["parser behavior".to_string()],
            dependencies: vec![],
            capabilities: vec!["rust".to_string()],
        }))
    }

    /// Canonical accept decision for the canonical intent.
    pub fn work_decision_accept_json() -> String {
        encode(WorkProfile::WorkDecision(WorkDecisionProfile {
            proposal_message_id: hex64(b'a'),
            kind: WorkDecisionKind::Accept(WorkDecisionAccept { reason: None }),
        }))
    }

    /// Canonical reject decision.
    pub fn work_decision_reject_json() -> String {
        encode(WorkProfile::WorkDecision(WorkDecisionProfile {
            proposal_message_id: hex64(b'a'),
            kind: WorkDecisionKind::Reject(WorkDecisionReject {
                reason: "scope too broad".to_string(),
            }),
        }))
    }

    /// Canonical narrow decision reducing the canonical intent's paths.
    pub fn work_decision_narrow_json() -> String {
        encode(WorkProfile::WorkDecision(WorkDecisionProfile {
            proposal_message_id: hex64(b'a'),
            kind: WorkDecisionKind::Narrow(WorkDecisionNarrow {
                paths: vec!["src/parser.rs".to_string()],
                concerns: vec!["parser behavior".to_string()],
                reason: Some("drop tests from accepted scope".to_string()),
            }),
        }))
    }

    /// Canonical order decision sequencing the proposal after another one.
    pub fn work_decision_order_json() -> String {
        encode(WorkProfile::WorkDecision(WorkDecisionProfile {
            proposal_message_id: hex64(b'a'),
            kind: WorkDecisionKind::Order(WorkDecisionOrder {
                after: Some(hex64(b'f')),
                reason: Some("land after lexer work".to_string()),
            }),
        }))
    }

    /// Canonical accept-overlap decision (elevated overlap risk accepted).
    pub fn work_decision_accept_overlap_json() -> String {
        encode(WorkProfile::WorkDecision(WorkDecisionProfile {
            proposal_message_id: hex64(b'a'),
            kind: WorkDecisionKind::AcceptOverlap(WorkDecisionAcceptOverlap {
                overlap: vec![WorkOverlapAcceptance {
                    kind: WorkOverlapKind::SameConcern,
                    path_a: None,
                    path_b: None,
                    concern: Some("parser behavior".to_string()),
                }],
                reason: Some("shared parser behavior accepted".to_string()),
            }),
        }))
    }

    /// Canonical amendment changing the accepted intent's paths.
    pub fn work_amendment_json() -> String {
        encode(WorkProfile::WorkAmendment(WorkAmendmentProfile {
            task_id: "parser-impl".to_string(),
            intent_message_id: hex64(b'a'),
            sequence: 2,
            paths: Some(vec![
                "src/parser.rs".to_string(),
                "src/parser/ast.rs".to_string(),
            ]),
            concerns: None,
            dependencies: None,
            approval_decision_id: None,
            reason: Some("split parser module".to_string()),
        }))
    }

    /// Canonical yield relinquishing accepted overlap.
    pub fn work_yield_json() -> String {
        encode(WorkProfile::WorkYield(WorkYieldProfile {
            task_id: "parser-impl".to_string(),
            intent_message_id: hex64(b'a'),
            sequence: 2,
            reason: Some("lexer owns tests/parser.rs".to_string()),
        }))
    }

    /// Canonical settled profile with verification evidence.
    pub fn work_settled_json() -> String {
        encode(WorkProfile::WorkSettled(WorkSettledProfile {
            task_id: "parser-impl".to_string(),
            intent_message_id: hex64(b'a'),
            sequence: 3,
            inspected_snapshot: hex64(b'b'),
            verification: WorkVerification {
                status: WorkVerificationStatus::Passed,
                summary: "84 parser tests passed".to_string(),
            },
        }))
    }

    /// Canonical completed terminal.
    pub fn work_completed_json() -> String {
        encode(WorkProfile::WorkCompleted(WorkCompletedProfile {
            task_id: "parser-impl".to_string(),
            intent_message_id: hex64(b'a'),
            sequence: 4,
            outcome: "Parser implemented and verified.".to_string(),
        }))
    }

    /// Canonical blocked terminal.
    pub fn work_blocked_json() -> String {
        encode(WorkProfile::WorkBlocked(WorkBlockedProfile {
            task_id: "parser-impl".to_string(),
            intent_message_id: hex64(b'a'),
            sequence: 2,
            reason: "CI runner unavailable".to_string(),
        }))
    }

    /// Canonical supersede replacing an applied decision.
    pub fn work_superseded_json() -> String {
        encode(WorkProfile::WorkSuperseded(WorkSupersededProfile {
            task_id: "parser-impl".to_string(),
            proposal_message_id: hex64(b'a'),
            superseded_decision_message_id: hex64(b'c'),
            reason: Some("narrow after review".to_string()),
        }))
    }

    /// Every canonical profile variant, labeled by wire tag.
    pub fn canonical_profiles() -> Vec<(&'static str, String)> {
        vec![
            ("work_intent", work_intent_json()),
            ("work_decision", work_decision_accept_json()),
            ("work_amendment", work_amendment_json()),
            ("work_yield", work_yield_json()),
            ("work_settled", work_settled_json()),
            ("work_completed", work_completed_json()),
            ("work_blocked", work_blocked_json()),
            ("work_superseded", work_superseded_json()),
        ]
    }

    /// Canonical lifecycle transition sequences (each message parses through
    /// the real decoder and applies in order).
    pub fn lifecycle_fixtures() -> Vec<(&'static str, Vec<String>)> {
        vec![
            (
                "propose_accept_settle_complete",
                vec![
                    work_intent_json(),
                    work_decision_accept_json(),
                    work_settled_json(),
                    work_completed_json(),
                ],
            ),
            (
                "propose_reject",
                vec![work_intent_json(), work_decision_reject_json()],
            ),
            (
                "propose_narrow_settle_blocked",
                vec![
                    work_intent_json(),
                    work_decision_narrow_json(),
                    work_settled_json(),
                    work_blocked_json(),
                ],
            ),
            (
                "propose_order_yield",
                vec![
                    work_intent_json(),
                    work_decision_order_json(),
                    work_yield_json(),
                ],
            ),
            (
                "propose_accept_overlap_amend",
                vec![
                    work_intent_json(),
                    work_decision_accept_overlap_json(),
                    work_amendment_json(),
                ],
            ),
            (
                "propose_accept_supersede_accept",
                vec![
                    work_intent_json(),
                    work_decision_accept_json(),
                    work_superseded_json(),
                    work_decision_narrow_json(),
                ],
            ),
        ]
    }

    /// Unknown future discriminator/version must parse to None.
    pub fn invalid_unknown_version() -> String {
        format!(
            "ffwork2:{}",
            work_intent_json().strip_prefix("ffwork1:").unwrap()
        )
    }

    /// Unknown profile type must parse to None.
    pub fn invalid_unknown_type() -> String {
        "ffwork1:{\"type\":\"work_whatever\",\"task_id\":\"parser-impl\"}".to_string()
    }

    /// Unknown field inside a variant must parse to None.
    pub fn invalid_unknown_field() -> String {
        "ffwork1:{\"type\":\"work_intent\",\"task_id\":\"parser-impl\",\"agent\":\"linux-dev\",\"sequence\":1,\"paths\":[],\"concerns\":[],\"dependencies\":[],\"capabilities\":[],\"sneaky\":true}".to_string()
    }

    /// Key reordering is non-canonical and must parse to None.
    pub fn invalid_noncanonical() -> String {
        let canonical = work_intent_json();
        let json = canonical.strip_prefix("ffwork1:").unwrap();
        let mut object: serde_json::Value = serde_json::from_str(json).unwrap();
        let map = object.as_object_mut().unwrap();
        let mut entries: Vec<(String, serde_json::Value)> =
            std::mem::take(map).into_iter().collect();
        entries.reverse();
        let reordered = serde_json::Value::Object(entries.into_iter().collect());
        format!("ffwork1:{reordered}")
    }

    /// Absolute and traversal paths must parse to None.
    pub fn invalid_unsafe_paths() -> String {
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace(
            "\"paths\":[\"src/parser.rs\",\"tests/parser.rs\"]",
            "\"paths\":[\"/etc/passwd\",\"../escape\"]",
        )
    }

    /// Backslash and glob metacharacter paths must parse to None.
    pub fn invalid_unsupported_paths() -> String {
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace(
            "\"paths\":[\"src/parser.rs\",\"tests/parser.rs\"]",
            "\"paths\":[\"src\\\\parser.rs\",\"src/*.rs\"]",
        )
    }

    /// Duplicate paths must parse to None.
    pub fn invalid_duplicate_paths() -> String {
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace(
            "\"paths\":[\"src/parser.rs\",\"tests/parser.rs\"]",
            "\"paths\":[\"src/parser.rs\",\"src/parser.rs\"]",
        )
    }

    /// Unsorted paths must parse to None.
    pub fn invalid_unsorted_paths() -> String {
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace(
            "\"paths\":[\"src/parser.rs\",\"tests/parser.rs\"]",
            "\"paths\":[\"tests/parser.rs\",\"src/parser.rs\"]",
        )
    }

    /// Aggregate path bytes beyond the bound must parse to None. Five
    /// distinct ~1 KiB entries (each within per-path limits) exceed the 4 KiB
    /// aggregate bound while staying inside the 8 KiB profile body.
    pub fn invalid_over_bound_paths() -> String {
        let component = "a".repeat(200);
        let paths: Vec<String> = (0..5)
            .map(|index| format!("{component}/{component}/{component}/{component}/p{index}"))
            .collect();
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace(
            "\"paths\":[\"src/parser.rs\",\"tests/parser.rs\"]",
            &format!(
                "\"paths\":[{}]",
                paths
                    .iter()
                    .map(|p| serde_json::to_string(p).unwrap())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        )
    }

    /// Invalid task ids must parse to None.
    pub fn invalid_task_id() -> String {
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace("\"task_id\":\"parser-impl\"", "\"task_id\":\"Parser Impl\"")
    }

    /// Malformed causal references must parse to None (the canonical intent
    /// omits absent `causal_base` via skip_serializing_if, so inject one that
    /// is not a full 64-hex id).
    pub fn invalid_malformed_causal_ref() -> String {
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace("\"coordinator\":\"human\"", "\"causal_base\":\"short\"")
    }

    /// Decision with a malformed proposal reference must parse to None.
    pub fn invalid_malformed_decision_ref() -> String {
        let json = work_decision_accept_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace(
            "\"proposal_message_id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            "\"proposal_message_id\":\"nope\"",
        )
    }

    /// Over-bound concern list must parse to None.
    pub fn invalid_over_bound_concerns() -> String {
        let concerns: Vec<String> = (0..WORK_MAX_CONCERNS + 1)
            .map(|index| format!("concern-{index:02}"))
            .collect();
        let json = work_intent_json();
        let json = json.strip_prefix("ffwork1:").unwrap();
        json.replace(
            "\"concerns\":[\"parser behavior\"]",
            &format!(
                "\"concerns\":[{}]",
                concerns
                    .iter()
                    .map(|c| serde_json::to_string(c).unwrap())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_contract::work_fixtures as fixtures;

    #[test]
    fn every_canonical_profile_round_trips_byte_exact() {
        for (label, json) in fixtures::canonical_profiles() {
            let profile = parse_work_profile(&json).unwrap_or_else(|| {
                panic!("{label} fixture must parse: {json}");
            });
            assert_eq!(
                encode_work_profile(&profile).unwrap(),
                json,
                "{label} must round-trip byte-exact"
            );
            assert_eq!(profile.type_name(), label);
        }
    }

    #[test]
    fn every_lifecycle_fixture_parses() {
        for (label, sequence) in fixtures::lifecycle_fixtures() {
            for json in &sequence {
                assert!(
                    parse_work_profile(json).is_some(),
                    "{label} sequence message must parse: {json}"
                );
            }
        }
    }

    #[test]
    fn invalid_fixtures_all_parse_to_none() {
        let invalid: Vec<(&str, String)> = vec![
            ("unknown version", fixtures::invalid_unknown_version()),
            ("unknown type", fixtures::invalid_unknown_type()),
            ("unknown field", fixtures::invalid_unknown_field()),
            ("noncanonical", fixtures::invalid_noncanonical()),
            ("unsafe paths", fixtures::invalid_unsafe_paths()),
            ("unsupported paths", fixtures::invalid_unsupported_paths()),
            ("duplicate paths", fixtures::invalid_duplicate_paths()),
            ("unsorted paths", fixtures::invalid_unsorted_paths()),
            ("over-bound paths", fixtures::invalid_over_bound_paths()),
            ("invalid task id", fixtures::invalid_task_id()),
            (
                "malformed causal ref",
                fixtures::invalid_malformed_causal_ref(),
            ),
            (
                "malformed decision ref",
                fixtures::invalid_malformed_decision_ref(),
            ),
            (
                "over-bound concerns",
                fixtures::invalid_over_bound_concerns(),
            ),
        ];
        for (label, json) in invalid {
            assert!(
                parse_work_profile(&json).is_none(),
                "{label} must parse to None: {json}"
            );
        }
    }

    #[test]
    fn docs_canonical_example_parses_through_the_real_decoder() {
        // Must stay byte-identical to the canonical example in
        // docs/agent-api.md and docs/agent-communication.md.
        let example = r#"ffwork1:{"type":"work_intent","task_id":"parser-impl","agent":"linux-dev","sequence":1,"coordinator":"human","paths":["src/parser.rs","tests/parser.rs"],"concerns":["parser behavior"],"dependencies":[],"capabilities":["rust"]}"#;
        let profile = parse_work_profile(example).expect("docs example must parse");
        assert_eq!(profile.type_name(), "work_intent");
        assert_eq!(
            encode_work_profile(&profile).unwrap(),
            example,
            "docs example must be canonical"
        );
    }

    #[test]
    fn plain_signal_text_without_discriminator_is_ignored() {
        assert!(parse_work_profile("Run iOS simulator tests").is_none());
        assert!(parse_work_profile("ffwork1").is_none());
        assert!(parse_work_profile("ffwork1:").is_none());
    }

    #[test]
    fn scope_validation_rejects_unsorted_duplicates_and_unsafe_entries() {
        assert!(validate_work_scope(&["a".to_string(), "b".to_string()], &[], &[], &[]).is_ok());
        assert!(validate_work_scope(&["b".to_string(), "a".to_string()], &[], &[], &[]).is_err());
        assert!(validate_work_scope(&["a".to_string(), "a".to_string()], &[], &[], &[]).is_err());
        assert!(validate_work_scope(&["a/../b".to_string()], &[], &[], &[]).is_err());
        assert!(validate_work_scope(&["a/**".to_string()], &[], &[], &[]).is_ok());
        assert!(validate_work_scope(&["a/*.rs".to_string()], &[], &[], &[]).is_err());
        assert!(validate_work_scope(&["a/**/b".to_string()], &[], &[], &[]).is_err());
        assert!(validate_work_scope(&["**".to_string()], &[], &[], &[]).is_err());
        assert!(validate_work_scope(&[".git/config".to_string()], &[], &[], &[]).is_err());
        assert!(validate_work_scope(&[".jj/state".to_string()], &[], &[], &[]).is_err());
    }

    #[test]
    fn scope_validation_requires_nfc_spellings_and_rejects_controls() {
        assert!(validate_work_scope(&["café.rs".to_string()], &[], &[], &[]).is_ok());
        // Non-NFC decomposition of é (e + combining acute) is rejected.
        let decomposed = "cafe\u{301}.rs".to_string();
        assert!(validate_work_scope(&[decomposed], &[], &[], &[]).is_err());
        assert!(validate_work_scope(
            &["ok.rs".to_string()],
            &["bad\u{7}concern".to_string()],
            &[],
            &[]
        )
        .is_err());
    }

    #[test]
    fn overlap_matrix_covers_exact_containment_glob_and_concern() {
        // Exact path overlap.
        let exact = evaluate_scope_overlap(
            &["src/a.rs".to_string()],
            &[],
            &["src/a.rs".to_string()],
            &[],
        );
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].kind, WorkOverlapKind::ExactPath);

        // Directory containment (canonical exact directory path "src").
        let contained =
            evaluate_scope_overlap(&["src".to_string()], &[], &["src/a.rs".to_string()], &[]);
        assert_eq!(contained.len(), 1);
        assert_eq!(contained[0].kind, WorkOverlapKind::DirectoryContainment);
        // Directory containment is symmetric.
        let contained_reverse =
            evaluate_scope_overlap(&["src/a.rs".to_string()], &[], &["src".to_string()], &[]);
        assert_eq!(
            contained_reverse[0].kind,
            WorkOverlapKind::DirectoryContainment
        );

        // Glob containment and identical glob roots.
        let glob = evaluate_scope_overlap(
            &["src/**".to_string()],
            &[],
            &["src/parser.rs".to_string()],
            &[],
        );
        assert_eq!(glob.len(), 1);
        assert_eq!(glob[0].kind, WorkOverlapKind::GlobMatch);
        let glob_same =
            evaluate_scope_overlap(&["src/**".to_string()], &[], &["src/**".to_string()], &[]);
        assert_eq!(glob_same[0].kind, WorkOverlapKind::GlobMatch);

        // Identical non-empty concerns.
        let concern = evaluate_scope_overlap(
            &["src/a.rs".to_string()],
            &["parser behavior".to_string()],
            &["src/b.rs".to_string()],
            &["parser behavior".to_string()],
        );
        assert_eq!(concern.len(), 1);
        assert_eq!(concern[0].kind, WorkOverlapKind::SameConcern);

        // No overlap at all.
        let none = evaluate_scope_overlap(
            &["src/a.rs".to_string()],
            &["alpha".to_string()],
            &["tests/b.rs".to_string()],
            &["beta".to_string()],
        );
        assert!(none.is_empty());
    }

    #[test]
    fn transition_validation_is_typed_and_state_aware() {
        use WorkTaskState::*;
        assert_eq!(
            transition_rejection(
                Some(Proposed),
                &parse_work_profile(&fixtures::work_decision_accept_json()).unwrap()
            ),
            None
        );
        assert_eq!(
            transition_rejection(
                Some(Accepted),
                &parse_work_profile(&fixtures::work_decision_accept_json()).unwrap()
            ),
            Some(WorkRejectReason::DecisionAlreadyApplied)
        );
        assert_eq!(
            transition_rejection(
                None,
                &parse_work_profile(&fixtures::work_decision_accept_json()).unwrap()
            ),
            Some(WorkRejectReason::MissingProposal)
        );
        assert_eq!(
            transition_rejection(
                Some(Accepted),
                &parse_work_profile(&fixtures::work_amendment_json()).unwrap()
            ),
            None
        );
        assert_eq!(
            transition_rejection(
                Some(Proposed),
                &parse_work_profile(&fixtures::work_amendment_json()).unwrap()
            ),
            Some(WorkRejectReason::WrongState {
                expected: "accepted",
                found: Some(Proposed)
            })
        );
        assert_eq!(
            transition_rejection(
                Some(Settled),
                &parse_work_profile(&fixtures::work_completed_json()).unwrap()
            ),
            None
        );
        assert_eq!(
            transition_rejection(
                Some(Accepted),
                &parse_work_profile(&fixtures::work_completed_json()).unwrap()
            ),
            Some(WorkRejectReason::WrongState {
                expected: "settled",
                found: Some(Accepted)
            })
        );
        assert_eq!(
            transition_rejection(
                Some(Accepted),
                &parse_work_profile(&fixtures::work_blocked_json()).unwrap()
            ),
            None
        );
        assert_eq!(
            transition_rejection(
                Some(Accepted),
                &parse_work_profile(&fixtures::work_yield_json()).unwrap()
            ),
            None
        );
        assert_eq!(
            transition_rejection(
                Some(Settled),
                &parse_work_profile(&fixtures::work_yield_json()).unwrap()
            ),
            Some(WorkRejectReason::WrongState {
                expected: "accepted",
                found: Some(Settled)
            })
        );
        assert_eq!(
            transition_rejection(
                Some(Accepted),
                &parse_work_profile(&fixtures::work_superseded_json()).unwrap()
            ),
            None
        );
        assert_eq!(
            transition_rejection(
                Some(Completed),
                &parse_work_profile(&fixtures::work_superseded_json()).unwrap()
            ),
            Some(WorkRejectReason::DecisionAlreadyApplied)
        );
    }

    #[test]
    fn task_id_and_capability_validators_are_bounded() {
        assert!(is_valid_task_id("parser-impl"));
        assert!(is_valid_task_id("task_42"));
        assert!(!is_valid_task_id(""));
        assert!(!is_valid_task_id("Parser"));
        assert!(!is_valid_task_id(&"a".repeat(WORK_MAX_TASK_ID_BYTES + 1)));
        assert!(is_valid_capability("rust"));
        assert!(!is_valid_capability("Rust"));
        assert!(!is_valid_capability("has space"));
        assert!(!is_valid_capability(
            &"x".repeat(WORK_MAX_CAPABILITY_BYTES + 1)
        ));
    }

    #[test]
    fn encode_rejects_over_bound_profile() {
        let big = "a".repeat(WORK_MAX_PATH_BYTES);
        let profile = WorkProfile::WorkIntent(WorkIntentProfile {
            task_id: "parser-impl".to_string(),
            agent: "linux-dev".to_string(),
            sequence: 1,
            causal_base: None,
            coordinator: None,
            paths: vec![big],
            concerns: vec![],
            dependencies: vec![],
            capabilities: vec![],
        });
        assert!(encode_work_profile(&profile).is_err());
    }

    #[test]
    fn scope_covers_exact_paths_and_directory_globs() {
        let scope = WorkScope {
            paths: vec!["src/parser.rs".to_string(), "tests/parser/**".to_string()],
            concerns: vec![],
            dependencies: vec![],
        };
        assert!(scope_covers_path(&scope, "src/parser.rs"));
        assert!(!scope_covers_path(&scope, "src/parser/ast.rs"));
        assert!(scope_covers_path(&scope, "tests/parser"));
        assert!(scope_covers_path(&scope, "tests/parser/ast.rs"));
        assert!(scope_covers_path(&scope, "tests/parser/a/b/c.rs"));
        assert!(!scope_covers_path(&scope, "tests/parser2/a.rs"));
        assert!(!scope_covers_path(&scope, "tests/parse"));
        assert!(!scope_covers_path(&scope, "src/parser.rs.bak"));
        let empty = WorkScope::default();
        assert!(!scope_covers_path(&empty, "anything.rs"));
    }

    #[test]
    fn partition_scope_paths_is_deterministic_and_preserves_order() {
        let scope = WorkScope {
            paths: vec!["src/parser/**".to_string()],
            concerns: vec![],
            dependencies: vec![],
        };
        let changed = vec![
            "src/lexer.rs".to_string(),
            "src/parser/ast.rs".to_string(),
            "src/parser.rs".to_string(),
            "README.md".to_string(),
            "src/parser/tests.rs".to_string(),
        ];
        let partition = partition_scope_paths(&changed, &scope);
        assert_eq!(
            partition.in_scope,
            ["src/parser/ast.rs", "src/parser/tests.rs"]
        );
        assert_eq!(
            partition.out_of_scope,
            ["src/lexer.rs", "src/parser.rs", "README.md"]
        );
        assert_eq!(
            partition_scope_paths(&[], &scope),
            ScopePathPartition::default()
        );
    }

    #[test]
    fn scope_change_request_round_trips_and_is_bounded() {
        let profile = ScopeChangeRequestProfile {
            task_id: "parser-impl".to_string(),
            intent_message_id: crate::work_contract::work_fixtures::hex64(b'c'),
            operations: vec![ScopeChangeOperation::Modify],
            paths: vec!["src/lexer.rs".to_string()],
            concerns: vec!["lexer behavior".to_string()],
            reason: "local edit outside accepted scope".to_string(),
        };
        let body = encode_scope_change_request(&profile).unwrap();
        assert!(body.starts_with(SCOPE_CHANGE_REQUEST_DISCRIMINATOR));
        assert!(body.len() <= WORK_MAX_PROFILE_BYTES);
        assert_eq!(parse_scope_change_request(&body).unwrap(), profile);

        assert!(parse_scope_change_request("scope_change_requested:{}").is_none());
        assert!(parse_scope_change_request("ffwork1:{\"type\":\"work_intent\"}").is_none());
        assert!(parse_scope_change_request("plain text").is_none());

        let mut oversized = profile.clone();
        oversized.reason = "r".repeat(WORK_MAX_REASON_BYTES + 1);
        assert!(encode_scope_change_request(&oversized).is_err());

        let mut bad_task = profile.clone();
        bad_task.task_id = "UPPER".to_string();
        assert!(encode_scope_change_request(&bad_task).is_err());

        let mut bad_path = profile;
        bad_path.paths = vec!["../escape".to_string()];
        assert!(encode_scope_change_request(&bad_path).is_err());
    }
}
