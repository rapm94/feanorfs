//! Randomized integrator assignment orchestration (INT-1..INT-10).
//!
//! One authorized dispatcher builds an eligible roster, performs an auditable
//! random draw, offers the selected candidate an `ffint1` assignment through
//! the existing `ffmsg1` signal channel, and tracks a crash-safe state
//! machine. The hub never selects an integrator; identity and assignment are
//! advisory, not security guarantees; FeanorFS never merges file content.
//!
//! Dispatcher state lives in protected global workspace state under
//! `orchestrator/integrator-state.json`, guarded by an advisory dispatcher
//! lock and atomic private-file replacement. Losing state never authorizes a
//! new integrator automatically: corrupt or unsupported state fails closed.

use crate::conflict_artifacts::{
    conflict_identity_from_edit, is_binary_content, resolve_artifact,
    write_conflict_triple_with_labels, ArtifactRole, IdentityBinding, MaterializationPolicy,
};
use crate::ctx::SyncCtx;
use crate::durable::{
    atomic_overwrite, create_lock_acquire_exclusive, read_file_required, DurableJson,
};
use crate::history::traversal;
use crate::lock::DispatcherLock;
use crate::messages::{inbox, send_message, signals_since};
use crate::paths::validate_name;
use crate::snapshot::SnapshotEngine;
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{
    classify_conflict_kind, encode_integrator_profile, filter_eligible, generate_assignment_id,
    generate_selection_nonce, is_safe_rel_path, is_valid_hash, is_valid_hex_id,
    normalize_capabilities, parse_integrator_profile, rank_candidates, roster_fingerprint,
    AgentInboxQuery, AgentMessageInput, AgentMessageKind, ConcurrentEdit, ConflictKind,
    ConflictMaterializeEntry, ConflictMaterializeResult, IntegratorAssignInput,
    IntegratorAssignResult, IntegratorAssignmentState, IntegratorAttempt, IntegratorAttemptState,
    IntegratorAttemptStatus, IntegratorDigest, IntegratorDraw, IntegratorObserveResult,
    IntegratorOutcomeState, IntegratorProfile, IntegratorStatusResult,
    INTEGRATOR_DIGEST_FIELD_BYTES, INTEGRATOR_MAX_HISTORY,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// Schema 2 (A4): the unreachable `IntegratorAssignmentState::Active` and
// `IntegratorAttemptState::Active` variants were removed; schema-1 files map
// persisted `"active"` states to `"accepted"` (see `migrate_legacy_state`).
const INTEGRATOR_STATE_SCHEMA_VERSION: u32 = 2;
const INTEGRATOR_STATE_FILE: &str = "integrator-state.json";
const INTEGRATOR_MAX_ATTEMPTS: u32 = 64;
const INTEGRATOR_OBSERVE_LIMIT: usize = feanorfs_common::AGENT_INBOX_MAX_LIMIT;

/// Maximum causal-ancestry walk depth for one owner designation (bounded
/// BFS over intent causal-base and applied-transition references). Longer
/// chains are treated as causally unordered rather than unboundedly walked.
const DESIGNATION_MAX_CAUSAL_DEPTH: usize = 32;

/// Fully persisted assignment record (schema version 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedIntegratorAssignment {
    pub assignment_id: String,
    pub selection_nonce: String,
    pub about_snapshot: String,
    pub roster_fingerprint: String,
    pub ranked: Vec<String>,
    pub neutral_integrator: bool,
    pub eligible: Vec<String>,
    pub task_summary: String,
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub ack_timeout_ms: Option<u64>,
    pub conflict_authors: Vec<String>,
    pub excluded: Vec<String>,
    pub state: IntegratorAssignmentState,
    pub attempts: Vec<IntegratorAttempt>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub inbox_cursor: Option<String>,
    pub digest: Option<IntegratorDigest>,
}

/// Durable dispatcher state file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegratorStateFile {
    pub schema_version: u32,
    pub active: Option<PersistedIntegratorAssignment>,
    pub history: Vec<PersistedIntegratorAssignment>,
}

/// Crash-safe dispatcher state store.
pub struct IntegratorStore {
    inner: DurableJson<IntegratorStateFile>,
    dir: PathBuf,
}

/// Migrates a schema-1 dispatcher state file to the current schema, in place.
///
/// Schema 2 (A4) removes the unreachable `Active` variants from both
/// `IntegratorAssignmentState` and `IntegratorAttemptState`; a persisted
/// `"active"` value is mapped to `"accepted"`, which is the exact
/// non-terminal state the dispatcher uses while an integrator owns the
/// assignment. The rewrite is surgical — only `state` keys with the literal
/// value `"active"` change — and runs under the same lock the store uses so a
/// concurrent open cannot interleave. Unknown or future schemas are left for
/// the store's fail-closed schema check; the file is never rewritten when the
/// schema is already current.
fn migrate_legacy_state(dir: &Path) -> Result<()> {
    let state_path = dir.join(INTEGRATOR_STATE_FILE);
    if !state_path.exists() {
        return Ok(());
    }
    let _lock = create_lock_acquire_exclusive(&dir.join(format!("{INTEGRATOR_STATE_FILE}.lock")))?;
    if !state_path.exists() {
        return Ok(());
    }
    let content = read_file_required(&state_path)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&content).context("parse integrator state JSON")?;
    let schema = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if schema == Some(INTEGRATOR_STATE_SCHEMA_VERSION as u64) {
        return Ok(());
    }
    if schema != Some(1) {
        // Unknown or future schema: the store's fail-closed check handles it.
        return Ok(());
    }
    rewrite_active_states(&mut value);
    if let Some(version) = value.get_mut("schema_version") {
        *version = serde_json::json!(INTEGRATOR_STATE_SCHEMA_VERSION);
    }
    let json =
        serde_json::to_string_pretty(&value).context("serialize migrated integrator state")?;
    atomic_overwrite(&state_path, json.as_bytes()).context("commit migrated integrator state")?;
    Ok(())
}

/// Rewrites every `state` value equal to `"active"` to `"accepted"` at any
/// depth: both assignment records and per-attempt records carry a `state` key.
fn rewrite_active_states(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if key == "state" && child.as_str() == Some("active") {
                    *child = serde_json::json!("accepted");
                } else {
                    rewrite_active_states(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_active_states(item);
            }
        }
        _ => {}
    }
}

impl IntegratorStore {
    /// Opens (creating when absent) the orchestrator state store for a
    /// workspace. Corrupt or unsupported-schema state fails closed; legacy
    /// schema-1 files are migrated to the current schema first.
    pub fn open(base: &Path) -> Result<Self> {
        let dir = crate::workspace_layout::ensure_workspace_state(base)?.join("orchestrator");
        migrate_legacy_state(&dir)?;
        let inner = DurableJson::open(
            &dir,
            INTEGRATOR_STATE_FILE,
            IntegratorStateFile {
                schema_version: INTEGRATOR_STATE_SCHEMA_VERSION,
                active: None,
                history: Vec::new(),
            },
        )?;
        inner.with_read(|state| {
            ensure!(
                state.schema_version == INTEGRATOR_STATE_SCHEMA_VERSION,
                "unsupported integrator dispatcher state schema {} (expected {INTEGRATOR_STATE_SCHEMA_VERSION}); \
                 do not infer assignment state from signal history alone",
                state.schema_version
            );
            Ok(())
        })?;
        Ok(Self { inner, dir })
    }

    pub fn load(&self) -> Result<IntegratorStateFile> {
        self.inner.with_read(|state| {
            ensure!(
                state.schema_version == INTEGRATOR_STATE_SCHEMA_VERSION,
                "unsupported integrator dispatcher state schema {} (expected {INTEGRATOR_STATE_SCHEMA_VERSION}); \
                 do not infer assignment state from signal history alone",
                state.schema_version
            );
            Ok(state.clone())
        })
    }

    pub fn update(
        &self,
        f: impl FnOnce(&mut IntegratorStateFile) -> Result<()>,
    ) -> Result<IntegratorStateFile> {
        self.inner.with_write(|state| {
            ensure!(
                state.schema_version == INTEGRATOR_STATE_SCHEMA_VERSION,
                "unsupported integrator dispatcher state schema {}",
                state.schema_version
            );
            f(state)?;
            state.schema_version = INTEGRATOR_STATE_SCHEMA_VERSION;
            Ok(())
        })?;
        self.load()
    }

    pub fn path(&self) -> PathBuf {
        self.inner.state_path.clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Options for one dispatcher observation pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct IntegratorObserveOptions {
    /// Pre-acceptance acknowledgement timeout; when elapsed for the current
    /// offered attempt, the next recorded candidate is offered. `None`
    /// disables automatic timeout fallback during this pass.
    pub ack_timeout_ms: Option<u64>,
    /// When true, a candidate-specific blocker may advance to the next
    /// recorded candidate. Defaults to false: after acceptance the dispatcher
    /// must explicitly revoke or stop before replacing the integrator.
    pub fallback_on_blocked: bool,
}

fn dispatcher_name(explicit: Option<&str>) -> Result<String> {
    let sender = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("human")
        .to_string();
    validate_name(&sender)?;
    ensure!(sender != "*", "dispatcher must not be the broadcast form");
    Ok(sender)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn attempt_status(attempt: &IntegratorAttempt) -> IntegratorAttemptStatus {
    IntegratorAttemptStatus {
        attempt: attempt.attempt,
        selected: attempt.selected.clone(),
        state: attempt.state,
        offered_at_ms: attempt.offered_at_ms,
        request_message_id: attempt.request_message_id.clone(),
        terminal_message_id: attempt.terminal_message_id.clone(),
        reason: attempt.reason.clone(),
    }
}

fn status_result(assignment: &PersistedIntegratorAssignment) -> IntegratorStatusResult {
    IntegratorStatusResult {
        assignment_id: assignment.assignment_id.clone(),
        about_snapshot: assignment.about_snapshot.clone(),
        state: assignment.state,
        selected: assignment
            .attempts
            .last()
            .map(|attempt| attempt.selected.clone()),
        attempt: assignment.attempts.len().saturating_sub(1) as u32,
        neutral_integrator: assignment.neutral_integrator,
        roster_fingerprint: assignment.roster_fingerprint.clone(),
        fallback_order: assignment.ranked[1..].to_vec(),
        task_summary: assignment.task_summary.clone(),
        created_at_ms: assignment.created_at_ms,
        updated_at_ms: assignment.updated_at_ms,
        attempts: assignment.attempts.iter().map(attempt_status).collect(),
        digest: assignment.digest.clone(),
        inbox_cursor: assignment.inbox_cursor.clone(),
    }
}

/// Validates that `about_snapshot` is reachable from the current workspace
/// head; returns the snapshot id when valid.
async fn ensure_reachable_snapshot(ctx: &SyncCtx<'_>, about_snapshot: &str) -> Result<String> {
    ensure!(
        is_valid_hash(about_snapshot),
        "about_snapshot must be a full 64-hex snapshot id"
    );
    let head = ctx
        .api
        .get_head(ctx.workspace_id())
        .await?
        .context("workspace has no snapshot to assign against")?;
    let engine = SnapshotEngine::new(ctx);
    let outcome = traversal::walk(
        &head,
        traversal::TraversalBudgets {
            node_budget: 10_000,
            ..traversal::TraversalBudgets::unlimited()
        },
        traversal::ParentOrder::LastFirst,
        &mut traversal::EngineLoader(&engine),
        &mut traversal::TargetFinder::new(about_snapshot),
    )
    .await?;
    match outcome {
        traversal::TraversalOutcome::Stopped { .. } => Ok(about_snapshot.to_string()),
        traversal::TraversalOutcome::Exhausted { reason, .. } => {
            bail!("about_snapshot is not reachable within the scan bound ({reason})")
        }
        traversal::TraversalOutcome::Complete { .. } => {
            bail!("about_snapshot is not reachable from the workspace head")
        }
    }
}

async fn send_assignment_request(
    ctx: &SyncCtx<'_>,
    dispatcher: &str,
    assignment: &PersistedIntegratorAssignment,
    attempt: u32,
    selected: &str,
    neutral_integrator: bool,
) -> Result<String> {
    let profile = encode_integrator_profile(&IntegratorProfile::Assignment {
        assignment_id: assignment.assignment_id.clone(),
        attempt,
        selected: selected.to_string(),
        about_snapshot: assignment.about_snapshot.clone(),
        roster_fingerprint: assignment.roster_fingerprint.clone(),
        neutral_integrator,
        task: assignment.task_summary.clone(),
    })?;
    let result = send_message(
        ctx,
        AgentMessageInput {
            to: selected.to_string(),
            kind: AgentMessageKind::Request,
            body: profile,
            about_snapshot: Some(assignment.about_snapshot.clone()),
            reply_to: None,
            from: Some(dispatcher.to_string()),
        },
    )
    .await?;
    Ok(result.message_id)
}

/// Assigns one batch to a randomly ranked integrator (INT-3..INT-7).
///
/// Persists the draw before the offer is considered active, then publishes
/// the `ffint1` assignment request and records its message id. Fails closed
/// when another dispatcher holds the orchestration lock or when a previous
/// assignment is still active.
///
/// # Errors
/// Returns an error for invalid rosters, no eligible candidate, unreachable
/// snapshots, lock contention, or failed signal publication.
pub async fn integrator_assign(
    ctx: &SyncCtx<'_>,
    input: IntegratorAssignInput,
) -> Result<IntegratorAssignResult> {
    let eligibility = filter_eligible(&input)?;
    if let Some(reason) = &eligibility.no_candidate_reason {
        bail!("cannot assign an integrator: {reason}");
    }
    if ctx.format_version() < 3 {
        return Err(crate::agent::continuous::unsupported_schema_failure(
            "integrator assignment requires format v3; run `feanorfs migrate` first",
        ));
    }
    ensure_reachable_snapshot(ctx, &input.about_snapshot).await?;

    // Serialize dispatchers before any selection work so a contended loser
    // fails fast instead of burning CSPRNG draws and ranking on a doomed
    // assignment.
    let _lock = DispatcherLock::acquire(ctx.base)?;
    let store = IntegratorStore::open(ctx.base)?;
    let selection_pool = if eligibility.neutral_integrator {
        &eligibility.neutral
    } else {
        &eligibility.eligible
    };
    let normalized_requirements = normalize_capabilities(&input.required_capabilities)?;
    let assignment_id = generate_assignment_id()?;
    let selection_nonce = generate_selection_nonce()?;
    let fingerprint = roster_fingerprint(selection_pool)?;
    let ranked = rank_candidates(
        ctx.workspace_id(),
        &input.about_snapshot,
        &assignment_id,
        &selection_nonce,
        &fingerprint,
        selection_pool,
    )?;

    let created = store.update(|state| {
        ensure!(
            state.active.is_none(),
            "an integrator assignment is already active ({}); revoke or complete it first",
            state
                .active
                .as_ref()
                .map(|a| a.assignment_id.as_str())
                .unwrap_or("unknown")
        );
        let now = now_ms();
        let assignment = PersistedIntegratorAssignment {
            assignment_id: assignment_id.clone(),
            selection_nonce: selection_nonce.clone(),
            about_snapshot: input.about_snapshot.clone(),
            roster_fingerprint: fingerprint.clone(),
            ranked: ranked.clone(),
            neutral_integrator: eligibility.neutral_integrator,
            eligible: selection_pool.clone(),
            task_summary: input.task_summary.clone(),
            required_capabilities: normalized_requirements.clone(),
            ack_timeout_ms: input.ack_timeout_ms,
            conflict_authors: input.conflict_authors.clone(),
            excluded: input.excluded.clone(),
            state: IntegratorAssignmentState::Created,
            attempts: Vec::new(),
            created_at_ms: now,
            updated_at_ms: now,
            inbox_cursor: None,
            digest: None,
        };
        state.active = Some(assignment);
        Ok(())
    })?;
    let mut assignment = created
        .active
        .context("active assignment disappeared after creation")?;

    // Offer attempt 0: persist the offer intent, publish the request, then
    // record the message id so crash recovery never re-offers blindly.
    offer_next(ctx, &store, &mut assignment, None).await?;
    store.update(|state| {
        if let Some(active) = &mut state.active {
            *active = assignment.clone();
        }
        Ok(())
    })?;

    let current = assignment
        .attempts
        .last()
        .context("assignment has no attempt after offer")?;
    Ok(IntegratorAssignResult {
        assignment_id,
        about_snapshot: assignment.about_snapshot.clone(),
        selected: current.selected.clone(),
        fallback_order: assignment.ranked[1..].to_vec(),
        neutral_integrator: assignment.neutral_integrator,
        roster_fingerprint: fingerprint,
        attempt: current.attempt,
        request_message_id: current
            .request_message_id
            .clone()
            .context("offer did not record a request message id")?,
        state: IntegratorAssignmentState::Offered,
        task_summary: assignment.task_summary.clone(),
    })
}

/// Offers the next ranked candidate for `assignment` (attempt = attempts.len
/// if < ranked.len), publishing the request and recording its message id.
/// Persists the offered attempt *before* publishing so a crash never leaves
/// the offer unrecorded; a subsequent resume completes the send.
async fn offer_next(
    ctx: &SyncCtx<'_>,
    store: &IntegratorStore,
    assignment: &mut PersistedIntegratorAssignment,
    supersede_reason: Option<&str>,
) -> Result<()> {
    let index = assignment.attempts.len();
    ensure!(
        index < assignment.ranked.len(),
        "no ranked candidate remains to offer"
    );
    ensure!(
        index < INTEGRATOR_MAX_ATTEMPTS as usize,
        "assignment exceeded the maximum attempt count"
    );
    if let Some(reason) = supersede_reason {
        if let Some(previous) = assignment.attempts.last_mut() {
            if previous.state.is_open() {
                previous.state = IntegratorAttemptState::Superseded;
                previous.reason = Some(reason.to_string());
            }
        }
    }
    let selected = assignment.ranked[index].clone();
    let attempt_number = u32::try_from(index).context("attempt count overflow")?;
    let now = now_ms();
    assignment.attempts.push(IntegratorAttempt {
        attempt: attempt_number,
        selected: selected.clone(),
        state: IntegratorAttemptState::Offered,
        offered_at_ms: now,
        request_message_id: None,
        acceptance_message_id: None,
        terminal_message_id: None,
        reason: None,
    });
    assignment.state = IntegratorAssignmentState::Offered;
    assignment.updated_at_ms = now;
    store.update(|state| {
        if let Some(active) = &mut state.active {
            *active = assignment.clone();
        }
        Ok(())
    })?;

    let dispatcher = dispatcher_name(None)?;
    let message_id = send_assignment_request(
        ctx,
        &dispatcher,
        assignment,
        attempt_number,
        &selected,
        assignment.neutral_integrator,
    )
    .await?;
    if let Some(attempt) = assignment.attempts.last_mut() {
        attempt.request_message_id = Some(message_id.clone());
    }
    // The immutable request snapshot is the exact causal boundary. Reading a
    // later head here can skip a fast reply published immediately after it.
    assignment.inbox_cursor = Some(message_id);
    store.update(|state| {
        if let Some(active) = &mut state.active {
            *active = assignment.clone();
        }
        Ok(())
    })?;
    Ok(())
}

/// Reads the current assignment status (active by default, or by id from
/// active + bounded terminal history).
///
/// # Errors
/// Returns an error when the assignment id is unknown.
pub async fn integrator_status(
    ctx: &SyncCtx<'_>,
    assignment_id: Option<&str>,
) -> Result<IntegratorStatusResult> {
    let store = IntegratorStore::open(ctx.base)?;
    let state = store.load()?;
    let assignment = match assignment_id {
        Some(id) => {
            ensure!(
                is_valid_hex_id(id, 32),
                "assignment_id must be exactly 32 lowercase hex chars"
            );
            state
                .active
                .as_ref()
                .filter(|a| a.assignment_id == id)
                .or_else(|| state.history.iter().find(|a| a.assignment_id == id))
                .context("assignment not found")?
        }
        None => state
            .active
            .as_ref()
            .context("no active integrator assignment")?,
    };
    Ok(status_result(assignment))
}

/// Explicitly revokes the active assignment (INT-5). An accepted integrator
/// is replaced by the next recorded candidate when one remains; revoking an
/// offered attempt cancels the assignment. The reason is recorded durably for
/// the audit trail.
///
/// # Errors
/// Returns an error for unknown ids or terminal assignments.
pub async fn integrator_revoke(
    ctx: &SyncCtx<'_>,
    assignment_id: &str,
    reason: &str,
) -> Result<IntegratorStatusResult> {
    ensure!(
        is_valid_hex_id(assignment_id, 32),
        "assignment_id must be exactly 32 lowercase hex chars"
    );
    let reason = reason.trim();
    ensure!(
        !reason.is_empty() && reason.len() <= feanorfs_common::INTEGRATOR_DIGEST_FIELD_BYTES,
        "revocation reason must be non-empty and bounded"
    );
    let _lock = DispatcherLock::acquire(ctx.base)?;
    let store = IntegratorStore::open(ctx.base)?;
    let state = store.load()?;
    let mut assignment = state
        .active
        .as_ref()
        .filter(|a| a.assignment_id == assignment_id)
        .cloned()
        .context("no active assignment with that id")?;
    ensure!(
        matches!(
            assignment.state,
            IntegratorAssignmentState::Created
                | IntegratorAssignmentState::Offered
                | IntegratorAssignmentState::Accepted
                | IntegratorAssignmentState::RequiresHuman
        ),
        "assignment is already terminal ({:?})",
        assignment.state
    );
    let revoked_open = assignment.state == IntegratorAssignmentState::Accepted;
    if let Some(attempt) = assignment.attempts.last_mut() {
        attempt.state = IntegratorAttemptState::Revoked;
        attempt.reason = Some(reason.to_string());
    }
    assignment.updated_at_ms = now_ms();
    if revoked_open && assignment.attempts.len() < assignment.ranked.len() {
        // Explicit dispatcher revocation replaces the integrator.
        offer_next(ctx, &store, &mut assignment, Some(reason)).await?;
    } else {
        assignment.state = IntegratorAssignmentState::Cancelled;
        if let Some(attempt) = assignment.attempts.last_mut() {
            attempt.reason = Some(reason.to_string());
        }
        let mut state = store.load()?;
        archive(&mut state, &assignment);
        state.active = None;
        store.update(|current| {
            *current = state;
            Ok(())
        })?;
    }
    Ok(status_result(&assignment))
}

/// Moves a terminal assignment into bounded history and clears the active slot.
fn archive(state: &mut IntegratorStateFile, assignment: &PersistedIntegratorAssignment) {
    state.history.push(assignment.clone());
    state.history.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.assignment_id.cmp(&left.assignment_id))
    });
    state.history.truncate(INTEGRATOR_MAX_HISTORY);
    state.active = None;
}

enum PendingOfferRecovery {
    Published(String),
    Absent,
    Uncertain(String),
}

fn is_assignment_request(
    assignment: &PersistedIntegratorAssignment,
    attempt: &IntegratorAttempt,
    dispatcher: &str,
    message: &feanorfs_common::AgentMessage,
) -> bool {
    if message.from != dispatcher
        || message.to != attempt.selected
        || message.kind != AgentMessageKind::Request
        || message.about_snapshot != assignment.about_snapshot
        || message.reply_to.is_some()
    {
        return false;
    }
    matches!(
        parse_integrator_profile(&message.body),
        Some(IntegratorProfile::Assignment {
            assignment_id,
            attempt: profile_attempt,
            selected,
            about_snapshot,
            roster_fingerprint,
            neutral_integrator,
            task,
        }) if assignment_id == assignment.assignment_id
            && profile_attempt == attempt.attempt
            && selected == attempt.selected
            && about_snapshot == assignment.about_snapshot
            && roster_fingerprint == assignment.roster_fingerprint
            && neutral_integrator == assignment.neutral_integrator
            && task == assignment.task_summary
    )
}

async fn recover_pending_offer(
    ctx: &SyncCtx<'_>,
    assignment: &PersistedIntegratorAssignment,
    attempt: &IntegratorAttempt,
    dispatcher: &str,
) -> Result<PendingOfferRecovery> {
    let read = signals_since(
        ctx,
        assignment.inbox_cursor.as_deref(),
        INTEGRATOR_OBSERVE_LIMIT,
    )
    .await?;
    if read.cursor_reset {
        return Ok(PendingOfferRecovery::Uncertain(
            "offer recovery scan was bounded or lost its cursor; refusing to republish blindly"
                .to_string(),
        ));
    }
    let matches = read
        .messages
        .iter()
        .filter(|message| is_assignment_request(assignment, attempt, dispatcher, message))
        .map(|message| message.message_id.clone())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(PendingOfferRecovery::Absent),
        [message_id] => Ok(PendingOfferRecovery::Published(message_id.clone())),
        _ => Ok(PendingOfferRecovery::Uncertain(
            "multiple matching assignment requests exist; human review is required".to_string(),
        )),
    }
}

/// One dispatcher observation pass (INT-5, INT-6): reads new ffint1 replies
/// since the persisted cursor, applies state transitions, and (with the
/// configured options) falls back on pre-acceptance timeout or a
/// candidate-specific blocker. Crash recovery uses the same path and never
/// re-sends a recorded request.
///
/// # Errors
/// Returns an error for corrupt state; a cursor reset or an unrecoverable
/// assignment fails closed into `RequiresHuman` instead of guessing.
pub async fn integrator_observe(
    ctx: &SyncCtx<'_>,
    options: IntegratorObserveOptions,
) -> Result<IntegratorObserveResult> {
    let _lock = DispatcherLock::acquire(ctx.base)?;
    let store = IntegratorStore::open(ctx.base)?;
    let dispatcher = dispatcher_name(None)?;
    let mut state = store.load()?;
    let mut assignment = match state.active.take() {
        Some(assignment) => assignment,
        None => {
            return Ok(IntegratorObserveResult {
                assignment_id: None,
                state: None,
                messages_processed: 0,
                cursor: None,
                cursor_reset: false,
                action: "none".to_string(),
            })
        }
    };

    let mut messages_processed = 0usize;
    let mut action = "none".to_string();
    let mut next_cursor: Option<String> = assignment.inbox_cursor.clone();

    // Recover an offer whose request was persisted but not yet published.
    let needs_send = assignment.attempts.last().is_some_and(|attempt| {
        attempt.state == IntegratorAttemptState::Offered && attempt.request_message_id.is_none()
    });
    if needs_send {
        let attempt = assignment.attempts.last().cloned().context("no attempt")?;
        let recovered = recover_pending_offer(ctx, &assignment, &attempt, &dispatcher).await?;
        let (message_id, recovered_existing) = match recovered {
            PendingOfferRecovery::Published(message_id) => (message_id, true),
            PendingOfferRecovery::Absent => (
                send_assignment_request(
                    ctx,
                    &dispatcher,
                    &assignment,
                    attempt.attempt,
                    &attempt.selected,
                    assignment.neutral_integrator,
                )
                .await?,
                false,
            ),
            PendingOfferRecovery::Uncertain(reason) => {
                assignment.state = IntegratorAssignmentState::RequiresHuman;
                assignment.updated_at_ms = now_ms();
                if let Some(current) = assignment.attempts.last_mut() {
                    current.reason = Some(reason);
                }
                let terminal = assignment.clone();
                store.update(|current| {
                    archive(current, &terminal);
                    Ok(())
                })?;
                return Ok(IntegratorObserveResult {
                    assignment_id: Some(terminal.assignment_id),
                    state: Some(IntegratorAssignmentState::RequiresHuman),
                    messages_processed: 0,
                    cursor: next_cursor,
                    cursor_reset: true,
                    action: "requires_human".to_string(),
                });
            }
        };
        if let Some(current) = assignment.attempts.last_mut() {
            current.request_message_id = Some(message_id.clone());
        }
        assignment.inbox_cursor = Some(message_id);
        next_cursor = assignment.inbox_cursor.clone();
        action = if recovered_existing {
            "recovered_offer"
        } else {
            "offered"
        }
        .to_string();
        store.update(|current| {
            if let Some(active) = &mut current.active {
                *active = assignment.clone();
            }
            Ok(())
        })?;
    }

    // Read new replies addressed to the dispatcher since the persisted cursor.
    let read = inbox(
        ctx,
        AgentInboxQuery {
            recipient: dispatcher.clone(),
            after: next_cursor.clone(),
            limit: INTEGRATOR_OBSERVE_LIMIT,
        },
    )
    .await?;
    if read.cursor_reset {
        // Fail closed: coordination history may be incomplete.
        assignment.state = IntegratorAssignmentState::RequiresHuman;
        assignment.updated_at_ms = now_ms();
        if let Some(attempt) = assignment.attempts.last_mut() {
            attempt.reason = Some(
                "inbox cursor reset: older coordination signals may have been missed; \
                 stop automatic mutation and recover dispatcher state"
                    .to_string(),
            );
        }
        store.update(|current| {
            if let Some(active) = &mut current.active {
                *active = assignment.clone();
            }
            Ok(())
        })?;
        return Ok(IntegratorObserveResult {
            assignment_id: Some(assignment.assignment_id.clone()),
            state: Some(IntegratorAssignmentState::RequiresHuman),
            messages_processed,
            cursor: next_cursor,
            cursor_reset: true,
            action: "requires_human".to_string(),
        });
    }

    let mut replies = read
        .messages
        .iter()
        .filter_map(|message| {
            parse_integrator_profile(&message.body).map(|profile| (message, profile))
        })
        .collect::<Vec<_>>();
    // Inbox delivery is newest-first. Apply acceptance checkpoints before
    // terminal replies from the same read so a fast Accepted -> Result pair
    // cannot be consumed in reverse and lost behind the advanced cursor.
    replies.sort_by(
        |(left_message, left_profile), (right_message, right_profile)| {
            profile_apply_order(left_profile)
                .cmp(&profile_apply_order(right_profile))
                .then_with(|| left_message.created_at_ms.cmp(&right_message.created_at_ms))
                .then_with(|| left_message.message_id.cmp(&right_message.message_id))
        },
    );
    for (message, profile) in replies {
        if !profile_message_matches(&assignment, &profile, message, &dispatcher) {
            continue;
        }
        if let IntegratorProfile::Result { digest, .. } = &profile {
            if ensure_reachable_snapshot(ctx, &digest.inspected_snapshot)
                .await
                .is_err()
            {
                continue;
            }
        }
        if apply_profile(&mut assignment, &profile, message, &dispatcher, now_ms())? {
            messages_processed += 1;
            action = assignment_action(assignment.state).to_string();
            store.update(|current| {
                if let Some(active) = &mut current.active {
                    *active = assignment.clone();
                }
                Ok(())
            })?;
        }
    }
    if !read.cursor.is_empty() {
        assignment.inbox_cursor = Some(read.cursor.clone());
        next_cursor = Some(read.cursor);
    }

    // Pre-acceptance timeout: advance to the next recorded candidate.
    if assignment.state == IntegratorAssignmentState::Offered {
        if let Some(timeout) = options.ack_timeout_ms.or(assignment.ack_timeout_ms) {
            let offered_at = assignment
                .attempts
                .last()
                .map(|attempt| attempt.offered_at_ms)
                .unwrap_or(0);
            let elapsed = now_ms().saturating_sub(offered_at).max(0) as u64;
            if elapsed >= timeout {
                if let Some(attempt) = assignment.attempts.last_mut() {
                    attempt.state = IntegratorAttemptState::TimedOut;
                }
                if assignment.attempts.len() < assignment.ranked.len() {
                    offer_next(
                        ctx,
                        &store,
                        &mut assignment,
                        Some("acknowledgement timed out"),
                    )
                    .await?;
                    action = "offered_next".to_string();
                } else {
                    assignment.state = IntegratorAssignmentState::Cancelled;
                    if let Some(attempt) = assignment.attempts.last_mut() {
                        attempt.reason =
                            Some("no candidate acknowledged within the timeout".to_string());
                    }
                    action = "cancelled".to_string();
                }
                store.update(|current| {
                    if let Some(active) = &mut current.active {
                        *active = assignment.clone();
                    }
                    Ok(())
                })?;
            }
        }
    }

    // Candidate-specific blocker with explicit dispatcher policy: fall back.
    if assignment.state == IntegratorAssignmentState::Blocked && options.fallback_on_blocked {
        if assignment.attempts.len() < assignment.ranked.len() {
            let reason = assignment
                .attempts
                .last()
                .and_then(|attempt| attempt.reason.clone())
                .unwrap_or_else(|| "candidate reported a blocker".to_string());
            offer_next(ctx, &store, &mut assignment, Some(&reason)).await?;
            action = "offered_next".to_string();
        } else {
            assignment.state = IntegratorAssignmentState::Blocked;
            if let Some(attempt) = assignment.attempts.last_mut() {
                attempt.reason = Some("no candidate could complete the assignment".to_string());
            }
            action = "blocked".to_string();
        }
        store.update(|current| {
            if let Some(active) = &mut current.active {
                *active = assignment.clone();
            }
            Ok(())
        })?;
    }

    // Terminal assignments move to bounded history.
    if matches!(
        assignment.state,
        IntegratorAssignmentState::Completed
            | IntegratorAssignmentState::Blocked
            | IntegratorAssignmentState::RequiresHuman
            | IntegratorAssignmentState::Cancelled
    ) {
        let terminal = assignment.clone();
        store.update(|current| {
            archive(current, &terminal);
            Ok(())
        })?;
        return Ok(IntegratorObserveResult {
            assignment_id: Some(terminal.assignment_id),
            state: Some(terminal.state),
            messages_processed,
            cursor: next_cursor,
            cursor_reset: false,
            action: match terminal.state {
                IntegratorAssignmentState::Completed => "completed".to_string(),
                IntegratorAssignmentState::Blocked => "blocked".to_string(),
                IntegratorAssignmentState::RequiresHuman => "requires_human".to_string(),
                _ => "cancelled".to_string(),
            },
        });
    }

    store.update(|current| {
        if let Some(active) = &mut current.active {
            *active = assignment.clone();
        }
        Ok(())
    })?;
    Ok(IntegratorObserveResult {
        assignment_id: Some(assignment.assignment_id),
        state: Some(assignment.state),
        messages_processed,
        cursor: next_cursor,
        cursor_reset: false,
        action,
    })
}

fn profile_apply_order(profile: &IntegratorProfile) -> u8 {
    match profile {
        IntegratorProfile::Accepted { .. } => 0,
        IntegratorProfile::Result { .. } | IntegratorProfile::Blocked { .. } => 1,
        IntegratorProfile::Assignment { .. } => 2,
    }
}

fn assignment_action(state: IntegratorAssignmentState) -> &'static str {
    match state {
        IntegratorAssignmentState::Accepted => "accepted",
        IntegratorAssignmentState::Completed => "completed",
        IntegratorAssignmentState::Blocked => "blocked",
        IntegratorAssignmentState::RequiresHuman => "requires_human",
        IntegratorAssignmentState::Cancelled => "cancelled",
        IntegratorAssignmentState::Revoked => "revoked",
        IntegratorAssignmentState::Created => "created",
        IntegratorAssignmentState::Offered => "offered",
    }
}

fn profile_message_matches(
    assignment: &PersistedIntegratorAssignment,
    profile: &IntegratorProfile,
    message: &feanorfs_common::AgentMessage,
    dispatcher: &str,
) -> bool {
    let (assignment_id, attempt_number, about_snapshot, expected_kind) = match profile {
        IntegratorProfile::Assignment { .. } => return false,
        IntegratorProfile::Accepted {
            assignment_id,
            attempt,
            about_snapshot,
        } => (
            assignment_id,
            *attempt,
            about_snapshot,
            AgentMessageKind::Status,
        ),
        IntegratorProfile::Result {
            assignment_id,
            attempt,
            about_snapshot,
            ..
        } => (
            assignment_id,
            *attempt,
            about_snapshot,
            AgentMessageKind::Result,
        ),
        IntegratorProfile::Blocked {
            assignment_id,
            attempt,
            about_snapshot,
            ..
        } => (
            assignment_id,
            *attempt,
            about_snapshot,
            AgentMessageKind::Blocked,
        ),
    };
    let Some(attempt) = assignment
        .attempts
        .iter()
        .find(|candidate| candidate.attempt == attempt_number)
    else {
        return false;
    };
    if assignment_id != &assignment.assignment_id
        || about_snapshot != &assignment.about_snapshot
        || message.about_snapshot != assignment.about_snapshot
        || message.from != attempt.selected
        || message.to != dispatcher
        || message.kind != expected_kind
        || message.reply_to.as_deref() != attempt.request_message_id.as_deref()
    {
        return false;
    }
    match profile {
        IntegratorProfile::Result { digest, .. } => {
            digest.assignment_id == assignment.assignment_id
                && digest.about_snapshot == assignment.about_snapshot
                && digest.integrator == attempt.selected
        }
        IntegratorProfile::Accepted { .. } | IntegratorProfile::Blocked { .. } => true,
        IntegratorProfile::Assignment { .. } => false,
    }
}

/// Applies one `ffint1` reply to the assignment state machine. Returns true
/// when the assignment changed. Stale, duplicate, and superseded replies are
/// harmless no-ops; terminal replies must reference the original request.
fn apply_profile(
    assignment: &mut PersistedIntegratorAssignment,
    profile: &IntegratorProfile,
    message: &feanorfs_common::AgentMessage,
    dispatcher: &str,
    now: i64,
) -> Result<bool> {
    if !profile_message_matches(assignment, profile, message, dispatcher) {
        return Ok(false);
    }
    let (profile_id, profile_attempt) = match profile {
        IntegratorProfile::Assignment { .. } => return Ok(false),
        IntegratorProfile::Accepted {
            assignment_id,
            attempt,
            ..
        }
        | IntegratorProfile::Result {
            assignment_id,
            attempt,
            ..
        }
        | IntegratorProfile::Blocked {
            assignment_id,
            attempt,
            ..
        } => (assignment_id, *attempt),
    };
    if profile_id != &assignment.assignment_id {
        return Ok(false);
    }
    let last_index = u32::try_from(assignment.attempts.len())
        .unwrap_or(u32::MAX)
        .saturating_sub(1);
    if profile_attempt != last_index || assignment.state == IntegratorAssignmentState::Revoked {
        return Ok(false);
    }
    let Some(attempt) = assignment
        .attempts
        .iter_mut()
        .find(|attempt| attempt.attempt == profile_attempt)
    else {
        return Ok(false);
    };
    if !attempt.state.is_open() {
        return Ok(false);
    }
    match profile {
        IntegratorProfile::Accepted { .. } => {
            if assignment.state != IntegratorAssignmentState::Offered {
                return Ok(false);
            }
            attempt.state = IntegratorAttemptState::Accepted;
            attempt.acceptance_message_id = Some(message.message_id.clone());
            assignment.state = IntegratorAssignmentState::Accepted;
        }
        IntegratorProfile::Result { digest, .. } => {
            if assignment.state != IntegratorAssignmentState::Accepted {
                return Ok(false);
            }
            // A terminal reply to an accepted request must reference it.
            if message.reply_to.as_deref() != attempt.request_message_id.as_deref() {
                return Ok(false);
            }
            if digest.assignment_id != assignment.assignment_id {
                return Ok(false);
            }
            attempt.state = match digest.state {
                IntegratorOutcomeState::Completed => IntegratorAttemptState::Completed,
                IntegratorOutcomeState::Blocked => IntegratorAttemptState::Blocked,
                IntegratorOutcomeState::RequiresHuman | IntegratorOutcomeState::Cancelled => {
                    IntegratorAttemptState::Blocked
                }
            };
            attempt.terminal_message_id = Some(message.message_id.clone());
            attempt.reason = Some(digest.outcome.clone());
            assignment.digest = Some(IntegratorDigest::clone(digest));
            assignment.state = match digest.state {
                IntegratorOutcomeState::Completed => IntegratorAssignmentState::Completed,
                IntegratorOutcomeState::Blocked => IntegratorAssignmentState::Blocked,
                IntegratorOutcomeState::RequiresHuman => IntegratorAssignmentState::RequiresHuman,
                IntegratorOutcomeState::Cancelled => IntegratorAssignmentState::Cancelled,
            };
        }
        IntegratorProfile::Blocked { reason, .. } => {
            if !matches!(
                assignment.state,
                IntegratorAssignmentState::Offered | IntegratorAssignmentState::Accepted
            ) {
                return Ok(false);
            }
            attempt.state = IntegratorAttemptState::Blocked;
            attempt.reason = Some(reason.clone());
            attempt.terminal_message_id = Some(message.message_id.clone());
            assignment.state = IntegratorAssignmentState::Blocked;
        }
        IntegratorProfile::Assignment { .. } => unreachable!(),
    }
    assignment.updated_at_ms = now;
    Ok(true)
}

/// Resumes observation after a dispatcher restart (INT-6): loads durable
/// state and performs one observation pass. Never re-sends a recorded
/// request; a lost or corrupt state file fails closed instead of authorizing
/// a new integrator automatically.
///
/// # Errors
/// Returns an error for missing/corrupt dispatcher state.
pub async fn integrator_resume(
    ctx: &SyncCtx<'_>,
    options: IntegratorObserveOptions,
) -> Result<IntegratorObserveResult> {
    let store = IntegratorStore::open(ctx.base)?;
    if store.load()?.active.is_none() {
        return Ok(IntegratorObserveResult {
            assignment_id: None,
            state: None,
            messages_processed: 0,
            cursor: None,
            cursor_reset: false,
            action: "none".to_string(),
        });
    }
    integrator_observe(ctx, options).await
}

/// Materializes the first-class encrypted conflict triple for the integrator
/// (INT-8): read-only, project-litter-free, and head-preserving.
pub async fn materialize_conflicts(
    ctx: &SyncCtx<'_>,
    about_snapshot: &str,
    paths: &[String],
) -> Result<ConflictMaterializeResult> {
    if ctx.format_version() < 3 {
        return Err(crate::agent::continuous::unsupported_schema_failure(
            "conflict materialization requires format v3; run `feanorfs migrate` first",
        ));
    }
    let about = ensure_reachable_snapshot(ctx, about_snapshot).await?;
    feanorfs_common::validate_path_list(paths)?;
    let engine = SnapshotEngine::new(ctx);
    let snapshot = engine.load_snapshot(&about).await?;
    let state = engine.objects.get_tree_state(&snapshot.root).await?;

    let requested: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let selected = state
        .conflicts
        .iter()
        .filter(|edit| {
            is_safe_rel_path(&edit.path)
                && (requested.is_empty() || requested.contains(edit.path.as_str()))
        })
        .collect::<Vec<_>>();
    ensure!(
        selected.len() <= feanorfs_common::INTEGRATOR_MAX_PATHS,
        "snapshot contains more than {} requested conflicts; supply an explicit bounded subset",
        feanorfs_common::INTEGRATOR_MAX_PATHS
    );
    ensure!(
        requested.is_empty() || selected.len() == requested.len(),
        "one or more requested conflict paths do not exist in the selected snapshot"
    );
    let mut edits: Vec<(ConcurrentEdit, ConflictKind)> = Vec::with_capacity(selected.len());
    for edit in selected {
        // Refuse stale materialization: the conflict must still exist with
        // identical legs in the current head.
        ensure_current_conflict(ctx, &about, edit).await?;
        let kind = classify_conflict_kind(
            edit.base.as_ref().unwrap_or(&legacy_missing(&edit.path)),
            edit.ours.as_ref(),
            edit.theirs.as_ref(),
            edit.theirs.is_none(),
        );
        edits.push((edit.clone(), kind));
    }

    let ts = chrono::Utc::now().timestamp_millis();
    let dir = crate::paths::conflicts_dir(ctx.base)?.join(format!("materialize_{ts}"));
    tokio::fs::create_dir_all(&dir).await?;

    let head = ctx.api.get_head(ctx.workspace_id()).await?;
    let mut entries = Vec::new();
    for (edit, kind) in &edits {
        let already = ctx.db.get_conflict_record(&edit.path).await?.is_some();
        if !already {
            write_materialized_triple(&dir, edit, ctx).await?;
            let dir_string = dir.to_string_lossy().to_string();
            match head.as_deref() {
                Some(head) => {
                    let identity = conflict_identity_from_edit(
                        ctx.workspace_id(),
                        head,
                        &about,
                        &snapshot.root,
                        edit,
                        *kind,
                        &IdentityBinding::default(),
                    );
                    let fingerprint =
                        feanorfs_common::compute_conflict_identity_fingerprint(&identity);
                    ctx.db
                        .upsert_conflict_fingerprinted(
                            &edit.path,
                            kind,
                            &dir_string,
                            ts,
                            &identity,
                            &fingerprint,
                        )
                        .await?;
                }
                None => {
                    ctx.db
                        .upsert_conflict(
                            &edit.path,
                            kind,
                            &dir_string,
                            ts,
                            crate::state::ConflictRecordStatus::Pending,
                        )
                        .await?;
                }
            }
        }
        entries.push(materialized_entry(&dir, edit, kind, already));
    }
    if !edits.is_empty() {
        let manifest: Vec<String> = edits.iter().map(|(edit, _)| edit.path.clone()).collect();
        tokio::fs::write(dir.join("manifest.json"), serde_json::to_string(&manifest)?).await?;
    }

    Ok(ConflictMaterializeResult {
        about_snapshot: about,
        conflict_dir: dir.to_string_lossy().into_owned(),
        entries,
    })
}

fn legacy_missing(path: &str) -> feanorfs_common::FileState {
    feanorfs_common::FileState {
        path: path.to_string(),
        hash: String::new(),
        size: 0,
        mtime: 0,
        deleted: true,
        mode: 0,
    }
}

/// Verifies the conflict still exists with identical legs in the current head
/// (a signal-only head with the same root is fine; a resolved conflict is not).
async fn ensure_current_conflict(
    ctx: &SyncCtx<'_>,
    about: &str,
    edit: &ConcurrentEdit,
) -> Result<()> {
    let head = ctx.api.get_head(ctx.workspace_id()).await?;
    let Some(head) = head else {
        return Err(crate::agent::continuous::retryable_volatility_failure(
            "workspace head disappeared during conflict materialization",
        ));
    };
    if head == about {
        return Ok(());
    }
    let engine = SnapshotEngine::new(ctx);
    let head_snapshot = engine.load_snapshot(&head).await?;
    let head_state = engine.objects.get_tree_state(&head_snapshot.root).await?;
    let current = head_state
        .conflicts
        .iter()
        .find(|candidate| candidate.path == edit.path);
    let Some(current) = current else {
        bail!(
            "conflict at '{}' was already resolved in the current head; refuse stale materialization",
            edit.path
        );
    };
    let legs_equal = |left: &Option<feanorfs_common::FileState>,
                      right: &Option<feanorfs_common::FileState>| {
        left.as_ref()
            .map(|leg| (&leg.hash, leg.size, leg.deleted, leg.mode))
            == right
                .as_ref()
                .map(|leg| (&leg.hash, leg.size, leg.deleted, leg.mode))
    };
    if !(legs_equal(&edit.base, &current.base)
        && legs_equal(&edit.ours, &current.ours)
        && legs_equal(&edit.theirs, &current.theirs))
    {
        bail!(
            "conflict at '{}' changed legs in the current head; refuse stale materialization",
            edit.path
        );
    }
    Ok(())
}

/// Writes `.original`/`.local`/`.cloud` artifacts for one conflict via the
/// sole canonical triple writer (strict policy). Absent cloud legs use the
/// existing `deleted` sentinel so `conflicts keep --cloud` can accept the
/// deletion; absent local legs use `deleted-locally`; absent base legs use
/// `missing`.
async fn write_materialized_triple(
    dir: &Path,
    edit: &ConcurrentEdit,
    ctx: &SyncCtx<'_>,
) -> Result<()> {
    write_conflict_triple_with_labels(
        dir,
        edit,
        ctx,
        None,
        "deleted-locally",
        "deleted",
        MaterializationPolicy::Strict,
    )
    .await
}

fn materialized_entry(
    dir: &Path,
    edit: &ConcurrentEdit,
    kind: &ConflictKind,
    already_materialized: bool,
) -> ConflictMaterializeEntry {
    let is_binary = |role: ArtifactRole| {
        crate::conflict_artifacts::read_binary_prefix(
            &resolve_artifact(dir, &edit.path, role),
            8 * 1024,
        )
        .is_some_and(|prefix| is_binary_content(&prefix))
    };
    ConflictMaterializeEntry {
        path: edit.path.clone(),
        kind: *kind,
        original_available: edit.base.is_some(),
        local_available: edit.ours.is_some(),
        cloud_available: edit.theirs.is_some(),
        is_binary: is_binary(ArtifactRole::Local) || is_binary(ArtifactRole::Cloud),
        already_materialized,
    }
}

/// How one conflict owner was designated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerDesignationMethod {
    /// A causally older accepted proposal's author was eligible and selected
    /// under the transitive causal closure.
    CausalEligible,
    /// No causal order applied (concurrent/equal bases over the eligible
    /// roster); the auditable `ffint1` ranking drew.
    IntegratorFallback,
}

/// Bounded evidence of one owner designation.
///
/// Every field the wire [`feanorfs_common::resolution_contract`] evidence
/// block needs is carried here at the top level so the resolution engine can
/// map it without recomputation: the method, the fallback nonce and roster
/// fingerprint, the sorted unique eligible roster, the ranked roster, the
/// attempt, and the engine-produced reasoning. The causal path additionally
/// records the selected intent message id and sequence; the fallback path
/// keeps the full auditable draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDesignationEvidence {
    /// How the owner was designated (causal ancestry vs `ffint1` fallback).
    pub method: OwnerDesignationMethod,
    /// OS-CSPRNG selection nonce consumed by the `ffint1` ranking (fallback
    /// draw only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// Blake3 of the canonical sorted eligible roster (fallback draw only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roster_fingerprint: Option<String>,
    /// Sorted unique eligible agent names considered for designation.
    #[serde(default)]
    pub eligible: Vec<String>,
    /// Sorted unique ranked agent names (fallback draw only).
    #[serde(default)]
    pub ranked: Vec<String>,
    /// One attempt bound to the exact conflict fingerprint.
    pub attempt: u32,
    /// Bounded plain-language reasoning for the audit trail.
    pub reasoning: String,
    /// Full auditable draw when the `ffint1` fallback was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draw: Option<IntegratorDraw>,
    /// Selected accepted proposal intent message id (causal path only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_intent_message_id: Option<String>,
    /// Selected accepted proposal sequence (causal path only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_sequence: Option<u64>,
}

/// One owner designation bound to an exact conflict fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDesignation {
    pub owner: String,
    pub method: OwnerDesignationMethod,
    /// 128-bit assignment id (32 lowercase hex chars).
    pub assignment_id: String,
    /// One attempt bound to the exact fingerprint.
    pub attempt: u32,
    pub task_id: Option<String>,
    /// Accepted intent/message ids bound to the conflict (sorted, unique).
    pub intent_message_ids: Vec<String>,
    pub evidence: OwnerDesignationEvidence,
}

/// Typed reason automatic owner designation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesignationRefusalKind {
    /// The work projection is incomplete; acceptance is not provable.
    ProjectionIncomplete,
    /// No accepted proposal exists at all; there is nothing to designate from.
    NoRoster,
    /// Accepted proposals exist but none is eligible: no accepted scope
    /// covers the path, the record's current capabilities are absent or
    /// invalid, the author yielded/blocked, or the path's task ownership is
    /// ambiguous.
    NoCapableRoster,
    /// A non-terminal resolution assignment already exists for the same
    /// conflict; a second automatic designation is refused.
    ActiveAssignmentExists,
}

/// Typed refusal of one owner designation.
#[derive(Debug)]
pub struct DesignationRefusal {
    pub kind: DesignationRefusalKind,
    pub detail: String,
}

impl std::fmt::Display for DesignationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for DesignationRefusal {}

fn designation_refusal(
    kind: DesignationRefusalKind,
    detail: impl std::fmt::Display,
) -> DesignationRefusal {
    DesignationRefusal {
        kind,
        detail: detail.to_string(),
    }
}

/// One record's recorded causal references: the intent's declared causal base
/// (the original declaration, never rewritten by later transitions) plus the
/// references applied transitions recorded — an explicit relative ordering
/// and superseded decision messages.
fn recorded_causal_refs(record: &crate::work::WorkProposalRecord) -> impl Iterator<Item = &str> {
    record
        .causal_base
        .iter()
        .map(String::as_str)
        .chain(record.superseded_decisions.iter().map(String::as_str))
        .chain(
            record
                .decision
                .as_ref()
                .and_then(|decision| decision.ordered_after.as_deref()),
        )
}

/// Transitive causal closure over one work projection.
///
/// Nodes are message ids: an intent's declared causal base, the references
/// applied transitions recorded (ordering/supersede references), and the
/// last-applied transition source. A proposal is causally older than another
/// when a bounded walk (depth ≤ [`DESIGNATION_MAX_CAUSAL_DEPTH`], visited
/// set) from the newer proposal's recorded references reaches the older
/// proposal's intent or source message. The walk may pass through proposals
/// in any state and any task, exactly like the reducer's causal-base
/// satisfaction, so chains of length ≥ 3 resolve to their oldest author
/// instead of drawing.
struct CausalAncestry<'a> {
    /// Every proposal indexed by each message id it declares (its intent,
    /// its last-applied transition, and its applied decision).
    by_message: HashMap<&'a str, Vec<&'a crate::work::WorkProposalRecord>>,
}

impl<'a> CausalAncestry<'a> {
    fn new(records: impl IntoIterator<Item = &'a crate::work::WorkProposalRecord>) -> Self {
        let mut by_message: HashMap<&str, Vec<&crate::work::WorkProposalRecord>> = HashMap::new();
        for record in records {
            let mut ids: Vec<&str> = vec![
                record.intent_message_id.as_str(),
                record.source_message_id.as_str(),
            ];
            if let Some(decision) = &record.decision {
                ids.push(decision.message_id.as_str());
            }
            for id in ids {
                by_message.entry(id).or_default().push(record);
            }
        }
        Self { by_message }
    }

    /// Whether `older` is causally older than `newer` under the transitive
    /// closure: a bounded walk from the newer record's recorded references
    /// reaches the older record's intent or source message.
    fn causally_references(
        &self,
        older: &crate::work::WorkProposalRecord,
        newer: &crate::work::WorkProposalRecord,
    ) -> bool {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut frontier: Vec<&str> = recorded_causal_refs(newer).collect();
        let mut depth = 0usize;
        while !frontier.is_empty() && depth < DESIGNATION_MAX_CAUSAL_DEPTH {
            depth += 1;
            let mut next: Vec<&str> = Vec::new();
            for id in frontier {
                if !visited.insert(id) {
                    continue;
                }
                if id == older.intent_message_id || id == older.source_message_id {
                    return true;
                }
                if let Some(records) = self.by_message.get(id) {
                    for record in records {
                        next.extend(recorded_causal_refs(record));
                    }
                }
            }
            frontier = next;
        }
        false
    }
}

/// Whether an accepted record's current capabilities are present and valid:
/// non-empty, strictly ascending (sorted, unique), canonical lowercase
/// identifiers. The authenticated projection preserves the applied intent's
/// exact capability vector through every transition.
fn record_capabilities_valid(record: &crate::work::WorkProposalRecord) -> bool {
    !record.capabilities.is_empty()
        && record
            .capabilities
            .windows(2)
            .all(|window| window[0] < window[1])
        && record
            .capabilities
            .iter()
            .all(|capability| feanorfs_common::is_valid_capability(capability))
}

/// Whether the author has explicitly yielded or reported a terminal blocker
/// on any proposal in the same task. An agent that relinquished accepted
/// work or hit a terminal automation blocker is not available for automatic
/// designation, even if a newer accepted generation still covers the path.
fn agent_yielded_or_blocked_in_task(
    tasks: &[crate::work::WorkTaskRecord],
    task_id: &str,
    agent: &str,
) -> bool {
    tasks.iter().any(|task| {
        task.task_id == task_id
            && task.proposals.iter().any(|proposal| {
                proposal.agent == agent
                    && matches!(
                        proposal.state,
                        feanorfs_common::WorkTaskState::Yielded
                            | feanorfs_common::WorkTaskState::Blocked
                    )
            })
    })
}

/// Refuses when a non-terminal resolution assignment already exists for the
/// same conflict. The conflict identity's path is the canonical per-conflict
/// key (the fingerprint's automatic fields — assignment id, attempt, owner —
/// differ across assignments, so the path is the stable identity key shared
/// by every designation of the same conflict). Reads the resolution store
/// read-only; an unreadable store fails closed because absence of an active
/// assignment cannot be proven. The engine's own terminal-state model
/// ([`crate::resolution::ResolutionAssignmentState::is_terminal`]) decides:
/// everything except the explicit terminal states blocks re-designation, so
/// a crashed in-flight apply (`PublicationUncertain`) and a submitted result
/// awaiting its human decision or publication are covered exactly like the
/// resolution engine's own non-terminal guard.
fn refuse_on_active_assignment(
    ctx: &SyncCtx<'_>,
    path: &str,
) -> std::result::Result<(), DesignationRefusal> {
    let state = crate::resolution::ResolutionStore::open(ctx.base)
        .and_then(|store| store.load())
        .map_err(|error| {
            designation_refusal(
                DesignationRefusalKind::ProjectionIncomplete,
                format!(
                    "resolution store unavailable; cannot verify no active assignment \
                     exists for '{path}': {error}"
                ),
            )
        })?;
    let non_terminal = state
        .jobs
        .iter()
        .any(|record| record.job.conflict.path == path && !record.assignment_state.is_terminal());
    if non_terminal {
        return Err(designation_refusal(
            DesignationRefusalKind::ActiveAssignmentExists,
            format!(
                "a non-terminal resolution assignment already exists for conflict \
                 '{path}'; a second automatic designation is refused"
            ),
        ));
    }
    Ok(())
}

/// Designates the owner for one exact conflict.
///
/// Queries the accepted intents (work reducer projection) for
/// conflict authors whose accepted scope covers the path and whose record is
/// currently capable and available: capabilities present and valid, no
/// explicit yield or terminal blocker in the task, unambiguous single-task
/// ownership, and no existing non-terminal resolution assignment for the
/// conflict. When transitive causal ancestry orders the eligible authors,
/// the older accepted state's author receives the first attempt; otherwise
/// (concurrent/equal bases) the auditable `ffint1` ranking draws over the
/// eligible roster with recorded evidence. Selection binds one fresh
/// assignment id and attempt 0 to the exact conflict at job preparation.
///
/// # Errors
/// Returns a typed [`DesignationRefusal`] when acceptance is not provable or
/// no capable roster exists.
pub async fn designate_conflict_owner(
    ctx: &SyncCtx<'_>,
    path: &str,
) -> std::result::Result<OwnerDesignation, DesignationRefusal> {
    ensure_safe_designation_path(path)
        .map_err(|error| designation_refusal(DesignationRefusalKind::NoRoster, error))?;
    let state = crate::work::WorkStore::open(ctx.base)
        .map_err(|error| {
            designation_refusal(
                DesignationRefusalKind::ProjectionIncomplete,
                format!("work reducer state unavailable: {error}"),
            )
        })?
        .load()
        .map_err(|error| {
            designation_refusal(
                DesignationRefusalKind::ProjectionIncomplete,
                format!("work reducer state unreadable: {error}"),
            )
        })?;
    if state.incomplete {
        return Err(designation_refusal(
            DesignationRefusalKind::ProjectionIncomplete,
            "work projection is incomplete (cursor reset or bound exhaustion); \
             acceptance is not provable, so no automatic owner is designated",
        ));
    }

    // Authenticated projection only: accepted proposals, task-bound.
    let mut accepted: Vec<(String, crate::work::WorkProposalRecord)> = Vec::new();
    for task in &state.tasks {
        for proposal in &task.proposals {
            if proposal.state == feanorfs_common::WorkTaskState::Accepted {
                accepted.push((task.task_id.clone(), proposal.clone()));
            }
        }
    }

    // (d) No existing non-terminal assignment for the same conflict: a second
    // automatic designation must never run concurrently with a live one.
    // Checked before any roster refusal so an actively-assigned conflict is
    // never re-designated, even when the work projection changed meanwhile.
    refuse_on_active_assignment(ctx, path)?;

    if accepted.is_empty() {
        return Err(designation_refusal(
            DesignationRefusalKind::NoRoster,
            format!(
                "no accepted proposal exists for '{}' and no accepted-author roster exists for a fallback draw",
                path
            ),
        ));
    }

    // (e) Eligible roster: accepted records whose scope covers the path and
    // whose author passes (a) capabilities and (b) explicit yield/blocker
    // availability; (c) task ownership is then judged among the eligible
    // records. Any failure while building it refuses.
    let covering: Vec<&(String, crate::work::WorkProposalRecord)> = accepted
        .iter()
        .filter(|(_, proposal)| {
            feanorfs_common::work_contract::scope_covers_path(&proposal.scope, path)
        })
        .collect();
    if covering.is_empty() {
        return Err(designation_refusal(
            DesignationRefusalKind::NoCapableRoster,
            format!(
                "no accepted proposal's scope covers '{path}'; no capable roster exists \
                 for a designation"
            ),
        ));
    }
    let mut eligible: Vec<&(String, crate::work::WorkProposalRecord)> = Vec::new();
    for entry in covering {
        // (a) Current capabilities present and valid.
        if !record_capabilities_valid(&entry.1) {
            continue;
        }
        // (b) Not explicitly yielded or blocked in the owning task.
        if agent_yielded_or_blocked_in_task(&state.tasks, &entry.0, &entry.1.agent) {
            continue;
        }
        eligible.push(entry);
    }
    if eligible.is_empty() {
        return Err(designation_refusal(
            DesignationRefusalKind::NoCapableRoster,
            format!(
                "accepted proposals cover '{path}' but none is eligible: capabilities \
                 are absent or invalid, or every author yielded or blocked; no \
                 capable roster exists"
            ),
        ));
    }
    // (c) Task ownership: the conflict's task is the single task whose
    // eligible records cover the path; two tasks with eligible records
    // claiming the same path make ownership ambiguous and fail closed.
    let mut roster_task: Option<&str> = None;
    for entry in &eligible {
        match roster_task {
            None => roster_task = Some(entry.0.as_str()),
            Some(task_id) if task_id == entry.0.as_str() => {}
            Some(_) => {
                return Err(designation_refusal(
                    DesignationRefusalKind::NoCapableRoster,
                    format!(
                        "conflict path '{path}' is covered by eligible accepted proposals \
                         from multiple tasks; automatic designation requires unambiguous \
                         single-task ownership"
                    ),
                ));
            }
        }
    }
    let roster_task = roster_task.expect("eligible is non-empty, so one task is bound");

    // Distinct eligible authors preserve deterministic (task, sequence) order.
    let mut eligible_authors: Vec<&(String, crate::work::WorkProposalRecord)> = Vec::new();
    for entry in &eligible {
        if !eligible_authors
            .iter()
            .any(|other| other.1.agent == entry.1.agent)
        {
            eligible_authors.push(entry);
        }
    }

    // Canonical sorted eligible roster, bound by the integrator roster cap.
    let mut roster: Vec<String> = eligible_authors
        .iter()
        .map(|entry| entry.1.agent.clone())
        .collect();
    roster.sort();
    roster.dedup();
    if roster.len() > feanorfs_common::INTEGRATOR_MAX_CANDIDATES {
        return Err(designation_refusal(
            DesignationRefusalKind::NoRoster,
            format!(
                "eligible roster exceeds the {INTEGRATOR_MAX_CANDIDATES} candidate bound",
                INTEGRATOR_MAX_CANDIDATES = feanorfs_common::INTEGRATOR_MAX_CANDIDATES
            ),
        ));
    }

    let assignment_id = generate_assignment_id().map_err(|error| {
        designation_refusal(
            DesignationRefusalKind::NoRoster,
            format!("assignment id unavailable: {error}"),
        )
    })?;
    let attempt: u32 = 0;

    // Transitive causal closure over the whole projection (any task, any
    // state, exactly like the reducer's causal-base satisfaction): chains of
    // length ≥ 3 resolve to their oldest author instead of drawing.
    let ancestry = CausalAncestry::new(state.tasks.iter().flat_map(|task| task.proposals.iter()));

    // Causal eligibility: exactly one eligible author is strictly older than
    // every other eligible author; it receives the first attempt.
    if let Some(oldest) = strictly_oldest_author(&eligible_authors, &ancestry) {
        let mut intent_message_ids: Vec<String> = eligible
            .iter()
            .map(|(_, proposal)| proposal.intent_message_id.clone())
            .collect();
        intent_message_ids.sort();
        intent_message_ids.dedup();
        let mut reasoning = format!(
            "causally older accepted state selected: author '{}' intent {} (sequence {}) is \
             an ancestor of every other eligible accepted proposal for '{}'",
            oldest.1.agent, oldest.1.intent_message_id, oldest.1.sequence, path
        );
        if reasoning.len() > INTEGRATOR_DIGEST_FIELD_BYTES {
            reasoning.truncate(INTEGRATOR_DIGEST_FIELD_BYTES);
        }
        return Ok(OwnerDesignation {
            owner: oldest.1.agent.clone(),
            method: OwnerDesignationMethod::CausalEligible,
            assignment_id,
            attempt,
            task_id: Some(oldest.0.clone()),
            intent_message_ids,
            evidence: OwnerDesignationEvidence {
                method: OwnerDesignationMethod::CausalEligible,
                nonce: None,
                roster_fingerprint: None,
                eligible: roster.clone(),
                ranked: Vec::new(),
                attempt,
                reasoning,
                draw: None,
                selected_intent_message_id: Some(oldest.1.intent_message_id.clone()),
                selected_sequence: Some(oldest.1.sequence),
            },
        });
    }

    // Fallback: no strictly-older eligible author under the transitive causal
    // closure (concurrent/equal bases). The auditable ffint1 ranking draws
    // over the eligible roster, and the persisted draw carries the full
    // evidence: nonce, roster fingerprint, ranked list, eligible roster.
    let roster_fingerprint_value = roster_fingerprint(&roster).map_err(|error| {
        designation_refusal(DesignationRefusalKind::NoRoster, error.to_string())
    })?;
    let selection_nonce = generate_selection_nonce().map_err(|error| {
        designation_refusal(
            DesignationRefusalKind::NoRoster,
            format!("selection nonce unavailable: {error}"),
        )
    })?;
    let about_snapshot = ctx
        .api
        .get_head(ctx.workspace_id())
        .await
        .map_err(|error| {
            designation_refusal(
                DesignationRefusalKind::NoRoster,
                format!("current head unavailable for the fallback draw: {error}"),
            )
        })?
        .ok_or_else(|| {
            designation_refusal(
                DesignationRefusalKind::NoRoster,
                "workspace head disappeared before the fallback draw",
            )
        })?;
    let ranked = rank_candidates(
        ctx.workspace_id(),
        &about_snapshot,
        &assignment_id,
        &selection_nonce,
        &roster_fingerprint_value,
        &roster,
    )
    .map_err(|error| {
        designation_refusal(
            DesignationRefusalKind::NoRoster,
            format!("fallback ranking failed: {error}"),
        )
    })?;
    let draw = IntegratorDraw {
        assignment_id: assignment_id.clone(),
        selection_nonce,
        about_snapshot,
        roster_fingerprint: roster_fingerprint_value,
        ranked: ranked.clone(),
        neutral_integrator: false,
        eligible: roster.clone(),
    };
    let owner = ranked.first().expect("ranked roster is non-empty").clone();
    let mut reasoning = format!(
        "no strictly-older eligible author under the transitive causal closure for '{}' \
         (concurrent or equal bases); auditable ffint1 fallback draw over the eligible \
         roster selected '{}'",
        path, owner
    );
    if reasoning.len() > INTEGRATOR_DIGEST_FIELD_BYTES {
        reasoning.truncate(INTEGRATOR_DIGEST_FIELD_BYTES);
    }
    Ok(OwnerDesignation {
        owner,
        method: OwnerDesignationMethod::IntegratorFallback,
        assignment_id,
        attempt,
        task_id: Some(roster_task.to_string()),
        intent_message_ids: Vec::new(),
        evidence: OwnerDesignationEvidence {
            method: OwnerDesignationMethod::IntegratorFallback,
            nonce: Some(draw.selection_nonce.clone()),
            roster_fingerprint: Some(draw.roster_fingerprint.clone()),
            eligible: roster.clone(),
            ranked: ranked.clone(),
            attempt,
            reasoning,
            draw: Some(draw),
            selected_intent_message_id: None,
            selected_sequence: None,
        },
    })
}

fn ensure_safe_designation_path(path: &str) -> Result<()> {
    ensure!(
        is_safe_rel_path(path),
        "designation path must be one canonical portable relative path"
    );
    Ok(())
}

/// Returns the unique eligible author strictly older than every other
/// eligible author under the transitive causal closure, if exactly one
/// exists; `None` for concurrent/equal bases.
fn strictly_oldest_author<'a>(
    eligible: &[&'a (String, crate::work::WorkProposalRecord)],
    ancestry: &CausalAncestry<'_>,
) -> Option<&'a (String, crate::work::WorkProposalRecord)> {
    if eligible.len() < 2 {
        return eligible.first().copied();
    }
    let mut oldest: Option<&(String, crate::work::WorkProposalRecord)> = None;
    for candidate in eligible {
        let older_than_all = eligible.iter().all(|other| {
            std::ptr::eq(*candidate, *other) || ancestry.causally_references(&candidate.1, &other.1)
        });
        if older_than_all {
            if oldest.is_some() {
                // Two incomparable older authors: concurrent bases.
                return None;
            }
            oldest = Some(*candidate);
        }
    }
    oldest
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::IntegratorDigest;

    fn attempt(attempt: u32, state: IntegratorAttemptState) -> IntegratorAttempt {
        IntegratorAttempt {
            attempt,
            selected: "agent-b".to_string(),
            state,
            offered_at_ms: 1,
            request_message_id: Some("a".repeat(64)),
            acceptance_message_id: None,
            terminal_message_id: None,
            reason: None,
        }
    }

    fn assignment(state: IntegratorAssignmentState) -> PersistedIntegratorAssignment {
        PersistedIntegratorAssignment {
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            selection_nonce: "f".repeat(64),
            about_snapshot: "a".repeat(64),
            roster_fingerprint: "b".repeat(64),
            ranked: vec!["agent-b".to_string(), "agent-a".to_string()],
            neutral_integrator: true,
            eligible: vec!["agent-a".to_string(), "agent-b".to_string()],
            task_summary: "Integrate parser implementation and tests".to_string(),
            required_capabilities: vec![],
            ack_timeout_ms: None,
            conflict_authors: vec![],
            excluded: vec![],
            state,
            attempts: vec![attempt(0, IntegratorAttemptState::Accepted)],
            created_at_ms: 1,
            updated_at_ms: 1,
            inbox_cursor: None,
            digest: None,
        }
    }

    fn message(
        profile: &IntegratorProfile,
        reply_to: Option<&str>,
    ) -> feanorfs_common::AgentMessage {
        feanorfs_common::AgentMessage {
            message_id: "c".repeat(64),
            from: "agent-b".to_string(),
            to: "human".to_string(),
            kind: match profile {
                IntegratorProfile::Accepted { .. } => AgentMessageKind::Status,
                IntegratorProfile::Result { .. } => AgentMessageKind::Result,
                IntegratorProfile::Blocked { .. } => AgentMessageKind::Blocked,
                IntegratorProfile::Assignment { .. } => AgentMessageKind::Request,
            },
            body: encode_integrator_profile(profile).unwrap(),
            about_snapshot: "a".repeat(64),
            reply_to: reply_to.map(str::to_string),
            created_at_ms: 2,
        }
    }

    fn digest(state: IntegratorOutcomeState) -> IntegratorDigest {
        IntegratorDigest {
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            integrator: "agent-b".to_string(),
            about_snapshot: "a".repeat(64),
            inspected_snapshot: "a".repeat(64),
            state,
            landed_paths: 12,
            resolved_conflicts: 3,
            remaining_conflicts: 0,
            verification: feanorfs_common::VerificationSummary {
                status: feanorfs_common::VerificationStatus::Passed,
                summary: "84 tests passed".to_string(),
                ..Default::default()
            },
            outcome: "Integrated parser implementation and tests.".to_string(),
            risks: vec![],
            decision_required: (state == IntegratorOutcomeState::RequiresHuman)
                .then(|| "Choose which conflict version to keep".to_string()),
        }
    }

    #[test]
    fn acceptance_transitions_offered_to_accepted() {
        let mut a = assignment(IntegratorAssignmentState::Offered);
        a.attempts[0].state = IntegratorAttemptState::Offered;
        let profile = IntegratorProfile::Accepted {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            about_snapshot: a.about_snapshot.clone(),
        };
        assert!(apply_profile(
            &mut a,
            &profile,
            &message(&profile, Some(&"a".repeat(64))),
            "human",
            2
        )
        .unwrap());
        assert_eq!(a.state, IntegratorAssignmentState::Accepted);
        assert_eq!(a.attempts[0].state, IntegratorAttemptState::Accepted);
        assert!(a.attempts[0].acceptance_message_id.is_some());
    }

    #[test]
    fn reply_envelope_and_digest_are_bound_to_the_current_attempt() {
        let a = assignment(IntegratorAssignmentState::Accepted);
        let profile = IntegratorProfile::Result {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            about_snapshot: a.about_snapshot.clone(),
            digest: Box::new(digest(IntegratorOutcomeState::Completed)),
        };
        let request_id = "a".repeat(64);
        let valid = message(&profile, Some(&request_id));
        assert!(profile_message_matches(&a, &profile, &valid, "human"));

        let mut wrong = valid.clone();
        wrong.from = "agent-a".into();
        assert!(!profile_message_matches(&a, &profile, &wrong, "human"));
        let mut wrong = valid.clone();
        wrong.to = "other-dispatcher".into();
        assert!(!profile_message_matches(&a, &profile, &wrong, "human"));
        let mut wrong = valid.clone();
        wrong.kind = AgentMessageKind::Blocked;
        assert!(!profile_message_matches(&a, &profile, &wrong, "human"));
        let mut wrong = valid.clone();
        wrong.about_snapshot = "b".repeat(64);
        assert!(!profile_message_matches(&a, &profile, &wrong, "human"));
        let mut wrong = valid;
        wrong.reply_to = Some("b".repeat(64));
        assert!(!profile_message_matches(&a, &profile, &wrong, "human"));

        let mut wrong_profile = profile.clone();
        let IntegratorProfile::Result { digest, .. } = &mut wrong_profile else {
            unreachable!()
        };
        digest.integrator = "agent-a".into();
        assert!(!profile_message_matches(
            &a,
            &wrong_profile,
            &message(&wrong_profile, Some(&request_id)),
            "human",
        ));
    }

    #[test]
    fn stale_and_duplicate_replies_are_harmless() {
        let mut a = assignment(IntegratorAssignmentState::Accepted);
        let result = IntegratorProfile::Result {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            about_snapshot: a.about_snapshot.clone(),
            digest: Box::new(digest(IntegratorOutcomeState::Completed)),
        };
        // Missing reply_to: rejected.
        assert!(!apply_profile(&mut a, &result, &message(&result, None), "human", 2).unwrap());
        // Wrong assignment id: rejected.
        let mut other = result.clone();
        let IntegratorProfile::Result {
            assignment_id,
            digest,
            ..
        } = &mut other
        else {
            unreachable!()
        };
        *assignment_id = "11111111111111111111111111111111".to_string();
        digest.assignment_id = assignment_id.clone();
        assert!(!apply_profile(
            &mut a,
            &other,
            &message(&other, Some(&"a".repeat(64))),
            "human",
            2
        )
        .unwrap());
        // Wrong attempt: rejected.
        let mut stale = result.clone();
        let IntegratorProfile::Result { attempt, .. } = &mut stale else {
            unreachable!()
        };
        *attempt = 7;
        assert!(!apply_profile(
            &mut a,
            &stale,
            &message(&stale, Some(&"a".repeat(64))),
            "human",
            2
        )
        .unwrap());
        // Superseded attempt: rejected.
        let mut superseded = assignment(IntegratorAssignmentState::Offered);
        superseded.attempts[0].state = IntegratorAttemptState::Superseded;
        assert!(!apply_profile(
            &mut superseded,
            &result,
            &message(&result, Some(&"a".repeat(64))),
            "human",
            2,
        )
        .unwrap());
    }

    #[test]
    fn result_completes_and_blocked_fails_closed() {
        let mut a = assignment(IntegratorAssignmentState::Accepted);
        let result = IntegratorProfile::Result {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            about_snapshot: a.about_snapshot.clone(),
            digest: Box::new(digest(IntegratorOutcomeState::Completed)),
        };
        assert!(apply_profile(
            &mut a,
            &result,
            &message(&result, Some(&"a".repeat(64))),
            "human",
            2
        )
        .unwrap());
        assert_eq!(a.state, IntegratorAssignmentState::Completed);
        assert!(a.digest.is_some());
        assert_eq!(
            a.attempts[0].terminal_message_id.as_deref(),
            Some("c".repeat(64).as_str())
        );

        let mut b = assignment(IntegratorAssignmentState::Accepted);
        let blocked = IntegratorProfile::Blocked {
            assignment_id: b.assignment_id.clone(),
            attempt: 0,
            about_snapshot: b.about_snapshot.clone(),
            reason: "Missing iOS toolchain".to_string(),
        };
        assert!(apply_profile(
            &mut b,
            &blocked,
            &message(&blocked, Some(&"a".repeat(64))),
            "human",
            2,
        )
        .unwrap());
        assert_eq!(b.state, IntegratorAssignmentState::Blocked);
        assert_eq!(
            b.attempts[0].reason.as_deref(),
            Some("Missing iOS toolchain")
        );

        let mut c = assignment(IntegratorAssignmentState::Accepted);
        let human = IntegratorProfile::Result {
            assignment_id: c.assignment_id.clone(),
            attempt: 0,
            about_snapshot: c.about_snapshot.clone(),
            digest: Box::new(digest(IntegratorOutcomeState::RequiresHuman)),
        };
        assert!(apply_profile(
            &mut c,
            &human,
            &message(&human, Some(&"a".repeat(64))),
            "human",
            2
        )
        .unwrap());
        assert_eq!(c.state, IntegratorAssignmentState::RequiresHuman);
    }

    #[test]
    fn late_acceptance_after_supersession_is_rejected() {
        let mut a = assignment(IntegratorAssignmentState::Offered);
        a.attempts = vec![
            attempt(0, IntegratorAttemptState::Superseded),
            attempt(1, IntegratorAttemptState::Offered),
        ];
        a.state = IntegratorAssignmentState::Offered;
        let profile = IntegratorProfile::Accepted {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            about_snapshot: a.about_snapshot.clone(),
        };
        assert!(!apply_profile(
            &mut a,
            &profile,
            &message(&profile, Some(&"a".repeat(64))),
            "human",
            2,
        )
        .unwrap());
        assert_eq!(a.state, IntegratorAssignmentState::Offered);
    }

    #[test]
    fn published_assignment_request_is_recoverable_by_exact_context() {
        let a = assignment(IntegratorAssignmentState::Offered);
        let attempt = &a.attempts[0];
        let profile = IntegratorProfile::Assignment {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            selected: attempt.selected.clone(),
            about_snapshot: a.about_snapshot.clone(),
            roster_fingerprint: a.roster_fingerprint.clone(),
            neutral_integrator: a.neutral_integrator,
            task: a.task_summary.clone(),
        };
        let mut request = message(&profile, None);
        request.from = "human".into();
        request.to = attempt.selected.clone();
        request.kind = AgentMessageKind::Request;
        assert!(is_assignment_request(&a, attempt, "human", &request));
        request.to = "agent-a".into();
        assert!(!is_assignment_request(&a, attempt, "human", &request));
    }

    #[test]
    fn store_roundtrips_and_bounds_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = IntegratorStore::open(dir.path()).unwrap();
        let mut assignment = assignment(IntegratorAssignmentState::Completed);
        store
            .update(|state| {
                archive(state, &assignment);
                Ok(())
            })
            .unwrap();
        // Fill history past the bound and assert truncation.
        for i in 0..(INTEGRATOR_MAX_HISTORY + 10) {
            assignment.assignment_id = format!("{i:032x}");
            assignment.updated_at_ms = 1_000 + i as i64;
            store
                .update(|state| {
                    archive(state, &assignment);
                    Ok(())
                })
                .unwrap();
        }
        let loaded = store.load().unwrap();
        assert_eq!(loaded.history.len(), INTEGRATOR_MAX_HISTORY);
        assert_eq!(loaded.schema_version, INTEGRATOR_STATE_SCHEMA_VERSION);
        // Newest updated first.
        assert_eq!(
            loaded.history[0].assignment_id,
            format!("{:032x}", INTEGRATOR_MAX_HISTORY + 9)
        );
    }

    #[test]
    fn legacy_schema1_active_state_migrates_to_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let store = IntegratorStore::open(dir.path()).unwrap();
        store
            .update(|state| {
                state.active = Some(assignment(IntegratorAssignmentState::Accepted));
                Ok(())
            })
            .unwrap();
        // Rewrite the durable file exactly as a pre-migration dispatcher would
        // have stored it: schema version 1 with the removed "active" wire
        // values on both the assignment and its attempt.
        let path = store.path();
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(1);
        fn set_active(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(object) => {
                    for (key, child) in object.iter_mut() {
                        if key == "state" && child.as_str() == Some("accepted") {
                            *child = serde_json::json!("active");
                        } else {
                            set_active(child);
                        }
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        set_active(item);
                    }
                }
                _ => {}
            }
        }
        set_active(&mut value);
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
        drop(store);

        let migrated = IntegratorStore::open(dir.path()).unwrap();
        let state = migrated.load().unwrap();
        assert_eq!(state.schema_version, INTEGRATOR_STATE_SCHEMA_VERSION);
        let active = state.active.expect("active assignment survives migration");
        assert_eq!(active.state, IntegratorAssignmentState::Accepted);
        assert_eq!(active.attempts[0].state, IntegratorAttemptState::Accepted);
        // The rewritten file is schema 2 and carries no "active" state value.
        let disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(migrated.path()).unwrap()).unwrap();
        assert_eq!(
            disk["schema_version"].as_u64(),
            Some(INTEGRATOR_STATE_SCHEMA_VERSION as u64)
        );
        fn any_active_state(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::Object(object) => object.iter().any(|(key, child)| {
                    (key == "state" && child.as_str() == Some("active")) || any_active_state(child)
                }),
                serde_json::Value::Array(items) => items.iter().any(any_active_state),
                _ => false,
            }
        }
        assert!(
            !any_active_state(&disk),
            "migrated state must not contain the removed wire value"
        );
    }

    #[test]
    fn store_fails_closed_on_unsupported_schema() {
        let dir = tempfile::tempdir().unwrap();
        let store = IntegratorStore::open(dir.path()).unwrap();
        std::fs::write(
            store.path(),
            r#"{"schema_version": 99, "active": null, "history": []}"#,
        )
        .unwrap();
        let err = store.load().unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported integrator dispatcher state schema"));
    }

    #[test]
    fn store_fails_closed_on_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = IntegratorStore::open(dir.path()).unwrap();
        std::fs::write(store.path(), "not json at all").unwrap();
        assert!(store.load().is_err());
    }

    #[allow(clippy::too_many_arguments)]
    fn work_proposal(
        agent: &str,
        path: &str,
        intent_id: char,
        causal_base: Option<String>,
        scope_path: Option<&str>,
        capabilities: &[&str],
    ) -> crate::work::WorkProposalRecord {
        let scope_path = scope_path.unwrap_or(path);
        crate::work::WorkProposalRecord {
            agent: agent.to_string(),
            sequence: 1,
            intent_message_id: std::iter::repeat_n(intent_id, 64).collect(),
            coordinator: Some("human".to_string()),
            causal_base,
            original_scope: feanorfs_common::WorkScope {
                paths: vec![scope_path.to_string()],
                concerns: vec![],
                dependencies: vec![],
            },
            scope: feanorfs_common::WorkScope {
                paths: vec![scope_path.to_string()],
                concerns: vec![],
                dependencies: vec![],
            },
            state: feanorfs_common::WorkTaskState::Accepted,
            decision: None,
            superseded_decisions: vec![],
            amendments: vec![],
            accepted_overlap: vec![],
            verification: None,
            inspected_snapshot: None,
            outcome: None,
            reason: None,
            source_message_id: std::iter::repeat_n(intent_id, 64).collect(),
            updated_at_ms: 1,
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            author_restore: None,
        }
    }

    fn write_work_state(
        base: &Path,
        task_id: &str,
        proposals: Vec<crate::work::WorkProposalRecord>,
        incomplete: bool,
    ) {
        let store = crate::work::WorkStore::open(base).unwrap();
        store
            .update(|state| {
                state.incomplete = incomplete;
                state.tasks = vec![crate::work::WorkTaskRecord {
                    task_id: task_id.to_string(),
                    proposals,
                    updated_at_ms: 1,
                }];
                Ok(())
            })
            .unwrap();
    }

    async fn designation_ctx() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        crate::ApiClient,
        crate::ClientDb,
    ) {
        let base = tempfile::tempdir().unwrap();
        let hub_data = tempfile::tempdir().unwrap();
        let hub = crate::LocalHub::open(hub_data.path().to_path_buf(), None)
            .await
            .unwrap();
        let api = crate::ApiClient::local(Arc::clone(&hub), None);
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::ClientDb::new(&state).await.unwrap();
        // Seed the hub object store so its lock files exist before any
        // manifest write.
        api.upload_object(
            "workspace",
            &feanorfs_common::hash_bytes(b"seed"),
            b"seed".to_vec(),
        )
        .await
        .unwrap();
        (base, hub_data, api, db)
    }

    async fn publish_head(ctx: &crate::SyncCtx<'_>) -> String {
        let engine = crate::SnapshotEngine::new(ctx);
        let id = engine
            .write(crate::snapshot::SnapshotInput {
                files: &std::collections::HashMap::new(),
                conflicts: &[],
                parents: vec![],
                author: "test",
                message: None,
            })
            .await
            .unwrap();
        match ctx
            .api
            .swap_head(ctx.workspace_id(), None, &id)
            .await
            .unwrap()
        {
            crate::SwapHeadResult::Swapped => id,
            _ => panic!("head must swap"),
        }
    }

    #[tokio::test]
    async fn designation_prefers_the_causally_older_eligible_author() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        let a_id = a.intent_message_id.clone();
        let b = work_proposal(
            "agent-b",
            "src/main.rs",
            'b',
            Some(a_id.clone()),
            None,
            &["rust"],
        );
        write_work_state(base.path(), "parser-impl", vec![a.clone(), b], false);

        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("designate");
        assert_eq!(designation.owner, "agent-a");
        assert_eq!(designation.method, OwnerDesignationMethod::CausalEligible);
        assert_eq!(designation.task_id.as_deref(), Some("parser-impl"));
        assert_eq!(designation.attempt, 0);
        assert_eq!(designation.intent_message_ids.len(), 2);
        assert!(designation.evidence.draw.is_none());
        assert_eq!(
            designation.evidence.selected_intent_message_id.as_deref(),
            Some(a_id.as_str())
        );
    }

    use std::sync::Arc;

    #[tokio::test]
    async fn designation_concurrent_bases_fall_back_to_ffint1_ranking() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        let b = work_proposal("agent-b", "src/main.rs", 'b', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a, b], false);

        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("designate");
        assert_eq!(
            designation.method,
            OwnerDesignationMethod::IntegratorFallback
        );
        assert_eq!(designation.task_id.as_deref(), Some("parser-impl"));
        assert!(designation.evidence.draw.is_some());
        let draw = designation.evidence.draw.as_ref().unwrap();
        assert_eq!(draw.ranked.len(), 2);
        assert_eq!(designation.owner, draw.ranked[0]);
        assert!(draw.ranked.contains(&"agent-a".to_string()));
        assert!(draw.ranked.contains(&"agent-b".to_string()));
        assert_eq!(designation.attempt, 0);
        assert!(designation.assignment_id.len() == 32);
        // The fallback evidence must carry the complete auditable facts for
        // the wire block without recomputation: nonce, roster fingerprint,
        // ranked list, eligible roster, method, attempt.
        let evidence = &designation.evidence;
        assert_eq!(evidence.method, OwnerDesignationMethod::IntegratorFallback);
        assert_eq!(evidence.attempt, 0);
        let nonce = evidence.nonce.as_ref().expect("fallback carries its nonce");
        assert_eq!(nonce, &draw.selection_nonce);
        assert!(feanorfs_common::is_valid_hash(nonce));
        let fingerprint = evidence
            .roster_fingerprint
            .as_ref()
            .expect("fallback carries its roster fingerprint");
        assert_eq!(fingerprint, &draw.roster_fingerprint);
        assert!(feanorfs_common::is_valid_hash(fingerprint));
        assert_eq!(evidence.ranked, draw.ranked);
        assert_eq!(evidence.ranked.len(), 2);
        assert_eq!(evidence.eligible, draw.eligible);
        let mut sorted = evidence.eligible.clone();
        sorted.sort();
        assert_eq!(evidence.eligible, sorted);
        assert_eq!(
            evidence.eligible,
            vec!["agent-a".to_string(), "agent-b".to_string()]
        );
        assert!(evidence.selected_intent_message_id.is_none());
        assert!(
            evidence
                .reasoning
                .contains("no strictly-older eligible author"),
            "fallback reasoning must state that no causal order applied: {}",
            evidence.reasoning
        );
    }

    #[tokio::test]
    async fn designation_refuses_when_no_scope_covers_the_path() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        // Accepted proposals exist, but none covers the conflict path: the
        // eligible roster is empty, so the fallback must refuse instead of
        // drawing over authors who never claimed the path.
        let a = work_proposal(
            "agent-a",
            "src/other.rs",
            'a',
            None,
            Some("src/other.rs"),
            &["rust"],
        );
        write_work_state(base.path(), "parser-impl", vec![a], false);

        let refusal = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect_err("no capable roster must refuse");
        assert_eq!(refusal.kind, DesignationRefusalKind::NoCapableRoster);
    }

    #[tokio::test]
    async fn designation_refuses_without_any_accepted_roster() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        write_work_state(base.path(), "parser-impl", vec![], false);
        let refusal = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect_err("no roster must refuse");
        assert_eq!(refusal.kind, DesignationRefusalKind::NoRoster);
    }

    #[tokio::test]
    async fn designation_refuses_when_projection_is_incomplete() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a], true);
        let refusal = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect_err("incomplete projection must refuse");
        assert_eq!(refusal.kind, DesignationRefusalKind::ProjectionIncomplete);
    }

    #[tokio::test]
    async fn designation_single_eligible_author_is_selected_directly() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a], false);
        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("designate");
        assert_eq!(designation.owner, "agent-a");
        assert_eq!(designation.method, OwnerDesignationMethod::CausalEligible);
    }

    #[tokio::test]
    async fn designation_transitive_chain_selects_the_oldest_author_without_draw() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        // a <- b <- c: c's intent derives from b's intent, b's from a's. The
        // transitive closure must select a without an ffint1 draw.
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        let a_id = a.intent_message_id.clone();
        let b = work_proposal(
            "agent-b",
            "src/main.rs",
            'b',
            Some(a_id.clone()),
            None,
            &["rust"],
        );
        let b_id = b.intent_message_id.clone();
        let c = work_proposal("agent-c", "src/main.rs", 'c', Some(b_id), None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a.clone(), b, c], false);

        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("designate");
        assert_eq!(designation.owner, "agent-a");
        assert_eq!(designation.method, OwnerDesignationMethod::CausalEligible);
        assert_eq!(designation.task_id.as_deref(), Some("parser-impl"));
        assert!(designation.evidence.draw.is_none());
        assert_eq!(
            designation.evidence.selected_intent_message_id.as_deref(),
            Some(a_id.as_str())
        );
        assert_eq!(
            designation.evidence.eligible,
            vec![
                "agent-a".to_string(),
                "agent-b".to_string(),
                "agent-c".to_string(),
            ]
        );
        assert!(designation.evidence.ranked.is_empty());
        assert!(designation.evidence.nonce.is_none());
        assert!(designation.evidence.roster_fingerprint.is_none());
        assert_eq!(
            designation.evidence.method,
            OwnerDesignationMethod::CausalEligible
        );
        assert_eq!(designation.evidence.attempt, 0);
        assert_eq!(designation.intent_message_ids.len(), 3);
    }

    #[tokio::test]
    async fn designation_excludes_yielded_eligible_agent() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        // agent-a accepted and covering the path, but explicitly yielded in
        // the same task; agent-b accepted and covering the path.
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        let mut yielded = work_proposal("agent-a", "src/main.rs", 'y', None, None, &["rust"]);
        yielded.state = feanorfs_common::WorkTaskState::Yielded;
        let b = work_proposal("agent-b", "src/main.rs", 'b', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a, yielded, b], false);

        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("designate");
        assert_eq!(designation.owner, "agent-b");
        assert_eq!(designation.method, OwnerDesignationMethod::CausalEligible);
        assert_eq!(designation.evidence.eligible, vec!["agent-b".to_string()]);
        assert!(
            !designation
                .evidence
                .eligible
                .contains(&"agent-a".to_string()),
            "a yielded author must not be eligible"
        );
    }

    #[tokio::test]
    async fn designation_excludes_blocked_eligible_agent() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        // agent-a accepted and covering the path, but with a terminal blocker
        // in the same task; agent-b accepted and covering the path.
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        let mut blocked = work_proposal("agent-a", "src/main.rs", 'k', None, None, &["rust"]);
        blocked.state = feanorfs_common::WorkTaskState::Blocked;
        let b = work_proposal("agent-b", "src/main.rs", 'b', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a, blocked, b], false);

        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("designate");
        assert_eq!(designation.owner, "agent-b");
        assert_eq!(designation.evidence.eligible, vec!["agent-b".to_string()]);
    }

    #[tokio::test]
    async fn designation_excludes_capability_less_record() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        // agent-a covers the path but its record carries no capabilities;
        // agent-b covers the path with a valid capability set.
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &[]);
        let b = work_proposal("agent-b", "src/main.rs", 'b', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a, b], false);

        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("designate");
        assert_eq!(designation.owner, "agent-b");
        assert_eq!(designation.evidence.eligible, vec!["agent-b".to_string()]);
    }

    #[tokio::test]
    async fn designation_refuses_when_every_covering_record_is_capability_less() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &[]);
        let b = work_proposal("agent-b", "src/main.rs", 'b', None, None, &[]);
        write_work_state(base.path(), "parser-impl", vec![a, b], false);

        let refusal = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect_err("no capable roster must refuse");
        assert_eq!(refusal.kind, DesignationRefusalKind::NoCapableRoster);
    }

    /// Seeds one fully valid resolution assignment for `path` with the given
    /// lifecycle state so the active-assignment refusal can be exercised
    /// without a full job preparation. The resolution store validates every
    /// job record on load, so the seeded job is complete and canonical.
    fn seed_resolution_job(
        base: &Path,
        path: &str,
        assignment_state: crate::resolution::ResolutionAssignmentState,
    ) {
        let store = crate::resolution::ResolutionStore::open(base).unwrap();
        store
            .update(|state| {
                let conflict = feanorfs_common::ConflictIdentity {
                    schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
                    workspace_id: "workspace".to_string(),
                    current_snapshot: "a".repeat(64),
                    about_snapshot: "a".repeat(64),
                    tree_root: "a".repeat(64),
                    path: path.to_string(),
                    base: feanorfs_common::ConflictLegDescriptor {
                        present: true,
                        deleted: false,
                        hash: "b".repeat(64),
                        size: 4,
                        mode: 0,
                    },
                    ours: feanorfs_common::ConflictLegDescriptor {
                        present: true,
                        deleted: false,
                        hash: "c".repeat(64),
                        size: 4,
                        mode: 0,
                    },
                    theirs: feanorfs_common::ConflictLegDescriptor {
                        present: true,
                        deleted: false,
                        hash: "d".repeat(64),
                        size: 4,
                        mode: 0,
                    },
                    kind: feanorfs_common::ConflictKind::EditEdit,
                    task_id: Some("parser-impl".to_string()),
                    intent_message_ids: vec!["a".repeat(64)],
                    assignment_id: Some("11111111111111111111111111111111".to_string()),
                    attempt: Some(0),
                    designated_owner: Some("agent-b".to_string()),
                    verification_policy: Some(
                        feanorfs_common::RESOLUTION_VERIFICATION_POLICY_ID.to_string(),
                    ),
                };
                let job_id = "22222222222222222222222222222222".to_string();
                state.jobs.push(crate::resolution::PersistedResolutionJob {
                    schema_version: 1,
                    job: feanorfs_common::ResolutionJob {
                        schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
                        job_id: job_id.clone(),
                        task_id: "parser-impl".to_string(),
                        assignment_id: "11111111111111111111111111111111".to_string(),
                        attempt: 0,
                        workspace_id: "workspace".to_string(),
                        owner: "agent-b".to_string(),
                        conflict_fingerprint: feanorfs_common::compute_conflict_identity_fingerprint(
                            &conflict,
                        ),
                        current_snapshot: "a".repeat(64),
                        about_snapshot: "a".repeat(64),
                        tree_root: "a".repeat(64),
                        accepted_intents: vec!["a".repeat(64)],
                        causal_refs: vec!["a".repeat(64)],
                        artifacts: vec![feanorfs_common::ArtifactDescriptor {
                            role: feanorfs_common::ArtifactRoleName::Original,
                            path: format!("orchestrator/resolution/jobs/{job_id}/base.bin"),
                        }],
                        candidate_destination: feanorfs_common::CandidateDestination {
                            path: format!(
                                "orchestrator/resolution/jobs/{job_id}/candidate-0.bin"
                            ),
                            create_new: true,
                        },
                        allowed_output_paths: vec![path.to_string()],
                        verification: feanorfs_common::VerificationPolicyRef {
                            policy_id: feanorfs_common::RESOLUTION_VERIFICATION_POLICY_ID
                                .to_string(),
                            command_config_ref: feanorfs_common::RESOLUTION_VERIFICATION_CONFIG_REF
                                .to_string(),
                            timeout_ms: feanorfs_common::RESOLUTION_DEFAULT_VERIFICATION_TIMEOUT_MS,
                            freshness_required: true,
                        },
                        prevention: feanorfs_common::PreventionReason::Exhausted {
                            detail: "seeded for designation test".to_string(),
                        },
                        last_resort_reason: "seeded for designation test".to_string(),
                        designation: feanorfs_common::resolution_contract::OwnerDesignationEvidence {
                            method: feanorfs_common::resolution_contract::OwnerDesignationMethod::IntegratorFallback,
                            nonce: Some("f".repeat(64)),
                            roster_fingerprint: Some("e".repeat(64)),
                            eligible: vec!["agent-b".to_string()],
                            ranked: vec!["agent-b".to_string()],
                            reasoning: "seeded".to_string(),
                            attempt: 0,
                        },
                        conflict,
                    },
                    assignment_state,
                    created_at_ms: 1,
                    verified_at_ms: None,
                    result: None,
                    question_generation: 0,
                });
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn designation_refuses_when_active_assignment_exists_for_the_conflict() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a], false);
        seed_resolution_job(
            base.path(),
            "src/main.rs",
            crate::resolution::ResolutionAssignmentState::Active,
        );

        let refusal = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect_err("an active assignment for the conflict must refuse");
        assert_eq!(refusal.kind, DesignationRefusalKind::ActiveAssignmentExists);
        assert!(refusal.detail.contains("src/main.rs"));
    }

    #[tokio::test]
    async fn designation_ignores_terminal_assignments_for_the_conflict() {
        let (base, _hub_data, api, db) = designation_ctx().await;
        let ctx = crate::SyncCtx::with_format_version(
            &api,
            &db,
            base.path(),
            "workspace",
            Some("shared-key"),
            feanorfs_common::LegacyPolicy::Reject,
            3,
        );
        publish_head(&ctx).await;
        let a = work_proposal("agent-a", "src/main.rs", 'a', None, None, &["rust"]);
        write_work_state(base.path(), "parser-impl", vec![a], false);
        seed_resolution_job(
            base.path(),
            "src/main.rs",
            crate::resolution::ResolutionAssignmentState::Revoked,
        );

        let designation = designate_conflict_owner(&ctx, "src/main.rs")
            .await
            .expect("a terminal assignment must not block designation");
        assert_eq!(designation.owner, "agent-a");
    }

    #[test]
    fn assignment_profiles_are_ignored_by_the_dispatcher() {
        let mut a = assignment(IntegratorAssignmentState::Offered);
        let profile = IntegratorProfile::Assignment {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            selected: "agent-b".to_string(),
            about_snapshot: a.about_snapshot.clone(),
            roster_fingerprint: "b".repeat(64),
            neutral_integrator: true,
            task: "Integrate parser implementation and tests".to_string(),
        };
        assert!(!apply_profile(&mut a, &profile, &message(&profile, None), "human", 2).unwrap());
        assert_eq!(a.state, IntegratorAssignmentState::Offered);
    }
}
