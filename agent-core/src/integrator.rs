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
    is_binary_content, resolve_artifact, sentinel, ArtifactRole, SUFFIX_CLOUD, SUFFIX_LOCAL,
    SUFFIX_ORIGINAL,
};
use crate::ctx::SyncCtx;
use crate::durable::DurableJson;
use crate::lock::DispatcherLock;
use crate::messages::{inbox, send_message};
use crate::paths::validate_name;
use crate::snapshot::SnapshotEngine;
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{
    classify_conflict_kind, encode_integrator_profile, filter_eligible, generate_assignment_id,
    generate_selection_nonce, is_safe_rel_path, is_valid_hash, is_valid_hex_id,
    parse_integrator_profile, rank_candidates, roster_fingerprint, AgentInboxQuery,
    AgentMessageInput, AgentMessageKind, ConcurrentEdit, ConflictKind, ConflictMaterializeEntry,
    ConflictMaterializeResult, IntegratorAssignInput, IntegratorAssignResult,
    IntegratorAssignmentState, IntegratorAttempt, IntegratorAttemptState, IntegratorAttemptStatus,
    IntegratorDigest, IntegratorObserveResult, IntegratorOutcomeState, IntegratorProfile,
    IntegratorStatusResult, INTEGRATOR_MAX_HISTORY,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const INTEGRATOR_STATE_SCHEMA_VERSION: u32 = 1;
const INTEGRATOR_STATE_FILE: &str = "integrator-state.json";
const INTEGRATOR_MAX_ATTEMPTS: u32 = 64;
const INTEGRATOR_OBSERVE_LIMIT: usize = feanorfs_common::AGENT_INBOX_MAX_LIMIT;

/// Fully persisted assignment record (schema version 1).
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

impl IntegratorStore {
    /// Opens (creating when absent) the orchestrator state store for a
    /// workspace. Corrupt or unsupported-schema state fails closed.
    pub fn open(base: &Path) -> Result<Self> {
        let dir = crate::workspace_layout::ensure_workspace_state(base)?.join("orchestrator");
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
    let mut pending = vec![head];
    let mut seen = HashSet::new();
    let engine = SnapshotEngine::new(ctx);
    while let Some(current) = pending.pop() {
        if seen.len() >= 10_000 {
            bail!("about_snapshot is not reachable within the scan bound");
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        if current == about_snapshot {
            return Ok(about_snapshot.to_string());
        }
        pending.extend(engine.load_snapshot(&current).await?.parents);
    }
    bail!("about_snapshot is not reachable from the workspace head")
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
    ensure!(
        ctx.format_version() >= 3,
        "integrator assignment requires format v3; run `feanorfs migrate` first"
    );
    ensure_reachable_snapshot(ctx, &input.about_snapshot).await?;

    let assignment_id = generate_assignment_id()?;
    let selection_nonce = generate_selection_nonce()?;
    let fingerprint = roster_fingerprint(&eligibility.eligible)?;
    let ranked = rank_candidates(
        ctx.workspace_id(),
        &input.about_snapshot,
        &assignment_id,
        &selection_nonce,
        &fingerprint,
        &eligibility.eligible,
    )?;

    let _lock = DispatcherLock::acquire(ctx.base)?;
    let store = IntegratorStore::open(ctx.base)?;
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
            eligible: eligibility.eligible.clone(),
            task_summary: input.task_summary.clone(),
            required_capabilities: input.required_capabilities.clone(),
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
        attempt.request_message_id = Some(message_id);
    }
    let head = ctx.api.get_head(ctx.workspace_id()).await?;
    assignment.inbox_cursor = head;
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

/// Explicitly revokes the active assignment (INT-5). An accepted/active
/// integrator is replaced by the next recorded candidate when one remains;
/// revoking an offered attempt cancels the assignment. The reason is recorded
/// durably for the audit trail.
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
                | IntegratorAssignmentState::Active
                | IntegratorAssignmentState::RequiresHuman
        ),
        "assignment is already terminal ({:?})",
        assignment.state
    );
    let revoked_open = assignment.state == IntegratorAssignmentState::Accepted
        || assignment.state == IntegratorAssignmentState::Active;
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
        let message_id = send_assignment_request(
            ctx,
            &dispatcher,
            &assignment,
            attempt.attempt,
            &attempt.selected,
            assignment.neutral_integrator,
        )
        .await?;
        if let Some(current) = assignment.attempts.last_mut() {
            current.request_message_id = Some(message_id);
        }
        assignment.inbox_cursor = ctx.api.get_head(ctx.workspace_id()).await?;
        next_cursor = assignment.inbox_cursor.clone();
        action = "offered".to_string();
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
            recipient: dispatcher,
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

    for message in &read.messages {
        let Some(profile) = parse_integrator_profile(&message.body) else {
            continue;
        };
        if apply_profile(&mut assignment, &profile, message, now_ms())? {
            messages_processed += 1;
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
        if let Some(timeout) = options.ack_timeout_ms {
            let offered_at = assignment
                .attempts
                .last()
                .map(|attempt| attempt.offered_at_ms)
                .unwrap_or(0);
            let elapsed = now_ms().saturating_sub(offered_at) as u64;
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

/// Applies one `ffint1` reply to the assignment state machine. Returns true
/// when the assignment changed. Stale, duplicate, and superseded replies are
/// harmless no-ops; terminal replies must reference the original request.
fn apply_profile(
    assignment: &mut PersistedIntegratorAssignment,
    profile: &IntegratorProfile,
    message: &feanorfs_common::AgentMessage,
    now: i64,
) -> Result<bool> {
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
        IntegratorProfile::Accepted { about_snapshot, .. } => {
            if assignment.state != IntegratorAssignmentState::Offered {
                return Ok(false);
            }
            attempt.state = IntegratorAttemptState::Accepted;
            attempt.acceptance_message_id = Some(message.message_id.clone());
            assignment.state = IntegratorAssignmentState::Accepted;
            let _ = about_snapshot;
        }
        IntegratorProfile::Result {
            digest,
            about_snapshot,
            ..
        } => {
            if !matches!(
                assignment.state,
                IntegratorAssignmentState::Accepted | IntegratorAssignmentState::Active
            ) {
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
            assignment.digest = Some(digest.clone());
            assignment.state = match digest.state {
                IntegratorOutcomeState::Completed => IntegratorAssignmentState::Completed,
                IntegratorOutcomeState::Blocked => IntegratorAssignmentState::Blocked,
                IntegratorOutcomeState::RequiresHuman => IntegratorAssignmentState::RequiresHuman,
                IntegratorOutcomeState::Cancelled => IntegratorAssignmentState::Cancelled,
            };
            let _ = about_snapshot;
        }
        IntegratorProfile::Blocked {
            reason,
            about_snapshot,
            ..
        } => {
            if !matches!(
                assignment.state,
                IntegratorAssignmentState::Offered
                    | IntegratorAssignmentState::Accepted
                    | IntegratorAssignmentState::Active
            ) {
                return Ok(false);
            }
            attempt.state = IntegratorAttemptState::Blocked;
            attempt.reason = Some(reason.clone());
            attempt.terminal_message_id = Some(message.message_id.clone());
            assignment.state = IntegratorAssignmentState::Blocked;
            let _ = about_snapshot;
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
    ensure!(
        ctx.format_version() >= 3,
        "conflict materialization requires format v3; run `feanorfs migrate` first"
    );
    let about = ensure_reachable_snapshot(ctx, about_snapshot).await?;
    feanorfs_common::validate_path_list(paths)?;
    let engine = SnapshotEngine::new(ctx);
    let snapshot = engine.load_snapshot(&about).await?;
    let state = engine.objects.get_tree_state(&snapshot.root).await?;

    let requested: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut edits: Vec<(ConcurrentEdit, ConflictKind)> = Vec::new();
    for edit in &state.conflicts {
        if !is_safe_rel_path(&edit.path) {
            continue;
        }
        if !requested.is_empty() && !requested.contains(edit.path.as_str()) {
            continue;
        }
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

    let mut entries = Vec::new();
    for (edit, kind) in &edits {
        let already = ctx.db.get_conflict_record(&edit.path).await?.is_some();
        if !already {
            write_materialized_triple(&dir, edit, ctx).await?;
            ctx.db
                .upsert_conflict(&edit.path, kind, &dir.to_string_lossy(), ts, "pending")
                .await?;
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
        bail!("workspace head disappeared during conflict materialization");
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
        left.as_ref().map(|l| l.hash.as_str()) == right.as_ref().map(|r| r.hash.as_str())
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

/// Writes `.original`/`.local`/`.cloud` artifacts for one conflict. Absent
/// cloud legs use the existing `deleted` sentinel so `conflicts keep --cloud`
/// can accept the deletion; absent local legs use `deleted-locally`; absent
/// base legs use `missing`.
async fn write_materialized_triple(
    dir: &Path,
    edit: &ConcurrentEdit,
    ctx: &SyncCtx<'_>,
) -> Result<()> {
    let base_dest = dir.join(format!("{}{SUFFIX_ORIGINAL}", edit.path));
    let ours_dest = dir.join(format!("{}{SUFFIX_LOCAL}", edit.path));
    let cloud_dest = dir.join(format!("{}{SUFFIX_CLOUD}", edit.path));
    write_leg(&base_dest, edit.base.as_ref(), ctx, &edit.path, "missing").await?;
    write_leg(
        &ours_dest,
        edit.ours.as_ref(),
        ctx,
        &edit.path,
        "deleted-locally",
    )
    .await?;
    write_leg(
        &cloud_dest,
        edit.theirs.as_ref(),
        ctx,
        &edit.path,
        "deleted",
    )
    .await?;
    Ok(())
}

async fn write_leg(
    dest: &Path,
    state: Option<&feanorfs_common::FileState>,
    ctx: &SyncCtx<'_>,
    path: &str,
    absent_label: &str,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match state {
        Some(state) if !state.deleted && !state.hash.is_empty() => {
            let plain = crate::large_file::read_bytes(ctx, path, &state.hash, state.size).await?;
            tokio::fs::write(dest, &plain).await?;
        }
        _ => {
            tokio::fs::write(dest, sentinel(absent_label)).await?;
        }
    }
    Ok(())
}

fn materialized_entry(
    dir: &Path,
    edit: &ConcurrentEdit,
    kind: &ConflictKind,
    already_materialized: bool,
) -> ConflictMaterializeEntry {
    let is_binary = |role: ArtifactRole| {
        resolve_artifact(dir, &edit.path, role)
            .exists()
            .then(|| std::fs::read(resolve_artifact(dir, &edit.path, role)).ok())
            .flatten()
            .is_some_and(|bytes| is_binary_content(&bytes))
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
            kind: AgentMessageKind::Result,
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
            },
            outcome: "Integrated parser implementation and tests.".to_string(),
            risks: vec![],
            decision_required: None,
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
        assert!(apply_profile(&mut a, &profile, &message(&profile, None), 2).unwrap());
        assert_eq!(a.state, IntegratorAssignmentState::Accepted);
        assert_eq!(a.attempts[0].state, IntegratorAttemptState::Accepted);
        assert!(a.attempts[0].acceptance_message_id.is_some());
    }

    #[test]
    fn stale_and_duplicate_replies_are_harmless() {
        let mut a = assignment(IntegratorAssignmentState::Accepted);
        let result = IntegratorProfile::Result {
            assignment_id: a.assignment_id.clone(),
            attempt: 0,
            about_snapshot: a.about_snapshot.clone(),
            digest: digest(IntegratorOutcomeState::Completed),
        };
        // Missing reply_to: rejected.
        assert!(!apply_profile(&mut a, &result, &message(&result, None), 2).unwrap());
        // Wrong assignment id: rejected.
        let mut other = result.clone();
        let IntegratorProfile::Result { assignment_id, .. } = &mut other else {
            unreachable!()
        };
        *assignment_id = "11111111111111111111111111111111".to_string();
        assert!(
            !apply_profile(&mut a, &other, &message(&other, Some(&"a".repeat(64))), 2).unwrap()
        );
        // Wrong attempt: rejected.
        let mut stale = result.clone();
        let IntegratorProfile::Result { attempt, .. } = &mut stale else {
            unreachable!()
        };
        *attempt = 7;
        assert!(
            !apply_profile(&mut a, &stale, &message(&stale, Some(&"a".repeat(64))), 2).unwrap()
        );
        // Superseded attempt: rejected.
        let mut superseded = assignment(IntegratorAssignmentState::Offered);
        superseded.attempts[0].state = IntegratorAttemptState::Superseded;
        assert!(!apply_profile(
            &mut superseded,
            &result,
            &message(&result, Some(&"a".repeat(64))),
            2
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
            digest: digest(IntegratorOutcomeState::Completed),
        };
        assert!(
            apply_profile(&mut a, &result, &message(&result, Some(&"a".repeat(64))), 2).unwrap()
        );
        assert_eq!(a.state, IntegratorAssignmentState::Completed);
        assert!(a.digest.is_some());
        assert_eq!(
            a.attempts[0].terminal_message_id.as_deref(),
            Some("c".repeat(64).as_str())
        );

        let mut b = assignment(IntegratorAssignmentState::Active);
        let blocked = IntegratorProfile::Blocked {
            assignment_id: b.assignment_id.clone(),
            attempt: 0,
            about_snapshot: b.about_snapshot.clone(),
            reason: "Missing iOS toolchain".to_string(),
        };
        assert!(apply_profile(&mut b, &blocked, &message(&blocked, None), 2).unwrap());
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
            digest: digest(IntegratorOutcomeState::RequiresHuman),
        };
        assert!(apply_profile(&mut c, &human, &message(&human, Some(&"a".repeat(64))), 2).unwrap());
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
        assert!(!apply_profile(&mut a, &profile, &message(&profile, None), 2).unwrap());
        assert_eq!(a.state, IntegratorAssignmentState::Offered);
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
        assert!(!apply_profile(&mut a, &profile, &message(&profile, None), 2).unwrap());
        assert_eq!(a.state, IntegratorAssignmentState::Offered);
    }
}
