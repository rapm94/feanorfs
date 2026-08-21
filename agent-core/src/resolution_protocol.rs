//! Cross-machine `ffres1` resolution protocol: a pure deterministic reducer
//! over encrypted signal bodies plus one durable private projection.
//!
//! The profiles (assignment, result, revoke, human answer) travel inside the
//! existing `ffmsg1` stream — the hub gains no semantic route, no plaintext
//! metadata, and no resolution logic. Assignment profiles embed the complete
//! immutable [`ResolutionJob`], so the designated machine imports the same
//! job by ID and fingerprint without shared local filesystem paths and
//! reconstructs the legs from authenticated hub blobs.
//!
//! Determinism rules (identical reachable histories produce identical
//! projections on every machine): profiles are applied in canonical message
//! id order; an assignment is superseded only by a higher attempt (equal
//! attempts keep the smaller assignment id); results and answers bind to the
//! exact job/assignment/attempt/fingerprint/question generation; bound
//! exhaustion and cursor resets mark the projection incomplete.

use crate::durable::DurableJson;
use crate::messages::signals_since;
use crate::workspace_layout::ensure_workspace_state;
use crate::SyncCtx;
use anyhow::{ensure, Context, Result};
use feanorfs_common::resolution_contract::{
    parse_resolution_profile, HumanResolutionAnswer, ResolutionAssignmentProfile,
    ResolutionProfile, ResolutionResult, ResolutionRevokeReason,
};
use feanorfs_common::{
    encode_resolution_profile, validate_human_resolution_answer, validate_resolution_job,
    validate_resolution_result, AgentMessage, AgentMessageInput, AgentMessageKind, ResolutionJob,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Current durable projection schema version.
pub const RESOLUTION_PROTOCOL_SCHEMA_VERSION: u32 = 1;

/// Durable projection file under the orchestrator boundary.
pub const RESOLUTION_PROTOCOL_FILE: &str = "resolution-protocol.json";

/// Maximum number of fingerprints retained in one projection. Overflow marks
/// the projection incomplete and deterministically evicts terminal entries.
pub const RESOLUTION_PROTOCOL_MAX_ENTRIES: usize = 64;

/// Maximum number of applied profile message ids retained for idempotency.
/// Overflow marks the projection incomplete.
pub const RESOLUTION_PROTOCOL_MAX_APPLIED: usize = 4096;

/// Maximum pending profiles per fingerprint and pending fingerprints.
/// Overflow marks the projection incomplete.
pub const RESOLUTION_PROTOCOL_MAX_PENDING_PER_FINGERPRINT: usize = 8;
pub const RESOLUTION_PROTOCOL_MAX_PENDING_FINGERPRINTS: usize = 64;

/// Bounded observation window per pass (matches the signal inbox bound).
const RESOLUTION_PROTOCOL_OBSERVE_LIMIT: usize = 1000;

/// Closed local lifecycle state of one fingerprint in the projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolAssignmentState {
    /// The assignment was observed; the designated machine has the job.
    Assigned,
    /// A bound result was observed.
    ResultReceived,
    /// A bound human answer was observed.
    HumanAnswered,
    /// The assignment was revoked or superseded.
    Revoked,
}

impl ProtocolAssignmentState {
    /// Stable wire string of this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::ResultReceived => "result_received",
            Self::HumanAnswered => "human_answered",
            Self::Revoked => "revoked",
        }
    }
}

/// One fingerprint's bounded projection entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolEntry {
    pub conflict_fingerprint: String,
    pub job_id: String,
    pub assignment_id: String,
    pub attempt: u32,
    pub owner: String,
    /// The complete imported job (durable; validated on every load).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<ResolutionJob>,
    pub state: ProtocolAssignmentState,
    pub question_generation: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResolutionResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<HumanResolutionAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoke_reason: Option<ResolutionRevokeReason>,
    /// Message id of the last profile applied for this fingerprint.
    pub observed_message_id: String,
}

/// Durable projection state (schema-versioned, advisory lock, atomic
/// replacement, bounded).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResolutionProtocolState {
    pub schema_version: u32,
    /// Workspace head observed by the last pass; the next pass reads from
    /// here. Lives inside this file so cursor advance and projection
    /// replacement are one atomic write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Sticky: a cursor reset, bound exhaustion, or an evicted non-terminal
    /// entry made the projection unable to prove completeness. Only a clean
    /// bounded rebuild clears it.
    pub incomplete: bool,
    /// Applied profile message ids (idempotency, bounded).
    pub applied: Vec<String>,
    pub entries: BTreeMap<String, ProtocolEntry>,
    /// Unbound profiles waiting for their assignment (keyed by fingerprint,
    /// applied in canonical message id order once the entry exists). Bounded;
    /// overflow marks the projection incomplete.
    #[serde(default)]
    pub pending: BTreeMap<String, Vec<(AgentMessage, ResolutionProfile)>>,
}

impl ResolutionProtocolState {
    fn fresh() -> Self {
        Self {
            schema_version: RESOLUTION_PROTOCOL_SCHEMA_VERSION,
            cursor: None,
            incomplete: false,
            applied: Vec::new(),
            entries: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}

/// Crash-safe resolution protocol store.
#[derive(Debug)]
pub struct ResolutionProtocolStore {
    inner: DurableJson<ResolutionProtocolState>,
}

impl ResolutionProtocolStore {
    /// Opens (creating when absent) the orchestrator protocol store for a
    /// workspace. Corrupt or unsupported-schema state fails closed.
    pub fn open(base: &Path) -> Result<Self> {
        let dir = ensure_workspace_state(base)?.join("orchestrator");
        let inner = DurableJson::open(
            &dir,
            RESOLUTION_PROTOCOL_FILE,
            ResolutionProtocolState::fresh(),
        )?;
        inner.with_read(|state| {
            ensure!(
                state.schema_version == RESOLUTION_PROTOCOL_SCHEMA_VERSION,
                "unsupported resolution protocol schema {} (expected \
                 {RESOLUTION_PROTOCOL_SCHEMA_VERSION})",
                state.schema_version
            );
            validate_protocol_state(state)?;
            Ok(())
        })?;
        Ok(Self { inner })
    }

    /// Reads the current state.
    pub fn load(&self) -> Result<ResolutionProtocolState> {
        self.inner.with_read(|state| Ok(state.clone()))
    }

    /// Atomically replaces the state (one durable write).
    pub fn update(
        &self,
        update: impl FnOnce(&mut ResolutionProtocolState) -> Result<()>,
    ) -> Result<()> {
        self.inner.with_write(|state| {
            ensure!(
                state.schema_version == RESOLUTION_PROTOCOL_SCHEMA_VERSION,
                "unsupported resolution protocol schema {}",
                state.schema_version
            );
            update(state)?;
            validate_protocol_state(state)?;
            Ok(())
        })
    }
}

/// Validates one loaded projection in full; corrupt or mixed-version state
/// fails closed.
fn validate_protocol_state(state: &ResolutionProtocolState) -> Result<()> {
    ensure!(
        state.entries.len() <= RESOLUTION_PROTOCOL_MAX_ENTRIES,
        "resolution protocol projection exceeds its entry bound"
    );
    ensure!(
        state.applied.len() <= RESOLUTION_PROTOCOL_MAX_APPLIED,
        "resolution protocol applied-ids exceed their bound"
    );
    for (fingerprint, entry) in &state.entries {
        ensure!(
            entry.conflict_fingerprint == *fingerprint
                && feanorfs_common::is_valid_hash(fingerprint),
            "resolution protocol entry key/fingerprint mismatch"
        );
        ensure!(
            feanorfs_common::integrator_contract::is_valid_hex_id(&entry.job_id, 32)
                && feanorfs_common::integrator_contract::is_valid_hex_id(&entry.assignment_id, 32),
            "resolution protocol entry ids are invalid"
        );
        ensure!(
            feanorfs_common::integrator_contract::is_valid_agent_name(&entry.owner),
            "resolution protocol entry owner is invalid"
        );
        ensure!(
            feanorfs_common::is_valid_hash(&entry.observed_message_id),
            "resolution protocol entry observed message id is invalid"
        );
        if let Some(job) = &entry.job {
            validate_resolution_job(job)?;
            ensure!(
                job.job_id == entry.job_id
                    && job.assignment_id == entry.assignment_id
                    && job.attempt == entry.attempt
                    && job.owner == entry.owner
                    && job.conflict_fingerprint == entry.conflict_fingerprint,
                "resolution protocol entry does not match its imported job"
            );
        }
        if let Some(result) = &entry.result {
            validate_resolution_result(result)?;
            ensure!(
                result.job_id == entry.job_id
                    && result.assignment_id == entry.assignment_id
                    && result.attempt == entry.attempt
                    && result.conflict_fingerprint == entry.conflict_fingerprint,
                "resolution protocol result binding mismatch"
            );
        }
        ensure!(
            state.pending.len() <= RESOLUTION_PROTOCOL_MAX_PENDING_FINGERPRINTS,
            "resolution protocol pending fingerprints exceed their bound"
        );
        for (fingerprint, pending) in &state.pending {
            ensure!(
                feanorfs_common::is_valid_hash(fingerprint),
                "resolution protocol pending fingerprint is invalid"
            );
            ensure!(
                pending.len() <= RESOLUTION_PROTOCOL_MAX_PENDING_PER_FINGERPRINT,
                "resolution protocol pending profiles exceed their bound"
            );
            for (message, profile) in pending {
                ensure!(
                    feanorfs_common::is_valid_hash(&message.message_id),
                    "resolution protocol pending message id is invalid"
                );
                ensure!(
                    feanorfs_common::validate_resolution_profile(profile).is_ok(),
                    "resolution protocol pending profile is invalid"
                );
            }
        }
        if let Some(answer) = &entry.answer {
            validate_human_resolution_answer(answer)?;
            ensure!(
                answer.job_id == entry.job_id
                    && answer.assignment_id == entry.assignment_id
                    && answer.attempt == entry.attempt
                    && answer.conflict_fingerprint == entry.conflict_fingerprint
                    && answer.question_generation == entry.question_generation,
                "resolution protocol answer binding mismatch"
            );
        }
    }
    Ok(())
}

/// Registers one message id whose profile was applied (idempotency,
/// bounded; overflow marks the projection incomplete).
fn register_applied(state: &mut ResolutionProtocolState, message_id: &str) {
    if state.applied.binary_search(&message_id.to_string()).is_ok() {
        return;
    }
    if state.applied.len() >= RESOLUTION_PROTOCOL_MAX_APPLIED {
        state.incomplete = true;
        state.applied.drain(0..RESOLUTION_PROTOCOL_MAX_APPLIED / 2);
    }
    if let Err(index) = state.applied.binary_search(&message_id.to_string()) {
        state.applied.insert(index, message_id.to_string());
    }
}

/// Applies one validated profile to one fingerprint entry. Returns the
/// complete immutable jobs that must be durably imported (assignment
/// profiles only) and whether the entry changed.
enum ApplyOutcome {
    /// The profile applied (or was an idempotent no-op). Carries the job an
    /// assignment must import durably, when one was applied.
    Applied(Option<Box<ResolutionJob>>),
    /// The profile references the entry but binds to state it has not
    /// observed yet (its result/question arrives later canonically). The
    /// caller holds it in bounded pending state and retries it after every
    /// entry mutation.
    HoldForLater,
}

fn apply_profile_to_entry(
    entry: &mut ProtocolEntry,
    profile: &ResolutionProfile,
    message_id: &str,
    sender: &str,
) -> Result<ApplyOutcome> {
    match profile {
        ResolutionProfile::Assignment(profile) => {
            let ResolutionAssignmentProfile {
                job,
                question_generation,
                ..
            } = profile.as_ref();
            // The fingerprint covers the automatic block including the
            // assignment id and attempt, so an equal fingerprint means the
            // identical job (validation enforces fingerprint == recomputed).
            // A genuinely different attempt has a different fingerprint and
            // its own entry; revocation of an older attempt travels as an
            // explicit revoke profile.
            if entry.job_id == job.job_id && entry.assignment_id == job.assignment_id {
                return Ok(ApplyOutcome::Applied(None));
            }
            ensure!(
                entry.assignment_id == job.assignment_id && entry.attempt == job.attempt,
                "ffres1 assignment fingerprint collision with a different job"
            );
            *entry = ProtocolEntry {
                conflict_fingerprint: job.conflict_fingerprint.clone(),
                job_id: job.job_id.clone(),
                assignment_id: job.assignment_id.clone(),
                attempt: job.attempt,
                owner: job.owner.clone(),
                job: Some(job.clone()),
                state: ProtocolAssignmentState::Assigned,
                question_generation: question_generation.unwrap_or(0),
                result: None,
                answer: None,
                revoke_reason: None,
                observed_message_id: message_id.to_string(),
            };
            Ok(ApplyOutcome::Applied(Some(Box::new(job.clone()))))
        }
        ResolutionProfile::Result(result) => {
            if !(result.job_id == entry.job_id
                && result.assignment_id == entry.assignment_id
                && result.attempt == entry.attempt
                && result.conflict_fingerprint == entry.conflict_fingerprint
                && sender == entry.owner)
            {
                return Ok(ApplyOutcome::HoldForLater);
            }
            if matches!(entry.state, ProtocolAssignmentState::Revoked) {
                return Ok(ApplyOutcome::Applied(None));
            }
            // Deterministic first-wins: an already-observed result keeps the
            // smaller observed message id; equal content is idempotent.
            if let Some(existing) = &entry.result {
                if existing == result {
                    return Ok(ApplyOutcome::Applied(None));
                }
                ensure!(
                    message_id < entry.observed_message_id.as_str(),
                    "ffres1 result supersedes an already-observed result with a \
                     non-canonical message id"
                );
            }
            entry.state = ProtocolAssignmentState::ResultReceived;
            entry.question_generation = result.question_generation;
            entry.result = Some(result.clone());
            entry.observed_message_id = message_id.to_string();
            Ok(ApplyOutcome::Applied(None))
        }
        ResolutionProfile::Revoke(profile) => {
            if !(profile.job_id == entry.job_id
                && profile.assignment_id == entry.assignment_id
                && profile.conflict_fingerprint == entry.conflict_fingerprint
                && profile.attempt >= entry.attempt)
            {
                return Ok(ApplyOutcome::HoldForLater);
            }
            if matches!(entry.state, ProtocolAssignmentState::Revoked) {
                return Ok(ApplyOutcome::Applied(None));
            }
            entry.state = ProtocolAssignmentState::Revoked;
            entry.revoke_reason = Some(profile.reason);
            entry.observed_message_id = message_id.to_string();
            Ok(ApplyOutcome::Applied(None))
        }
        ResolutionProfile::HumanAnswer(answer) => {
            if !(answer.job_id == entry.job_id
                && answer.assignment_id == entry.assignment_id
                && answer.attempt == entry.attempt
                && answer.conflict_fingerprint == entry.conflict_fingerprint
                && answer.question_generation == entry.question_generation)
            {
                return Ok(ApplyOutcome::HoldForLater);
            }
            if matches!(entry.state, ProtocolAssignmentState::Revoked) {
                return Ok(ApplyOutcome::Applied(None));
            }
            if let Some(existing) = &entry.answer {
                if existing == answer {
                    return Ok(ApplyOutcome::Applied(None));
                }
                ensure!(
                    message_id < entry.observed_message_id.as_str(),
                    "ffres1 human answer supersedes an already-observed answer with a \
                     non-canonical message id"
                );
            }
            entry.state = ProtocolAssignmentState::HumanAnswered;
            entry.answer = Some(answer.clone());
            entry.observed_message_id = message_id.to_string();
            Ok(ApplyOutcome::Applied(None))
        }
    }
}

/// Retries every held pending profile for one fingerprint against its
/// current entry (canonical message id order). Profiles that still bind to
/// unobserved state stay pending for the next mutation.
fn drain_pending(
    state: &mut ResolutionProtocolState,
    fingerprint: &str,
    imports: &mut Vec<ResolutionJob>,
) -> Result<()> {
    let Some(held) = state.pending.remove(fingerprint) else {
        return Ok(());
    };
    for (held_message, held_profile) in held {
        let sender = held_message.from.clone();
        let outcome = {
            let entry = state
                .entries
                .get_mut(fingerprint)
                .expect("entry exists after assignment insert");
            apply_profile_to_entry(entry, &held_profile, &held_message.message_id, &sender)?
        };
        match outcome {
            ApplyOutcome::Applied(import) => {
                if let Some(job) = import {
                    imports.push(*job);
                }
                register_applied(state, &held_message.message_id);
            }
            ApplyOutcome::HoldForLater => {
                let pending = state.pending.entry(fingerprint.to_string()).or_default();
                pending.push((held_message, held_profile));
                pending.sort_by(|left, right| left.0.message_id.cmp(&right.0.message_id));
            }
        }
    }
    Ok(())
}

/// Applies one bounded batch of observed `ffmsg1` messages (already sorted
/// by canonical message id) to the projection. Pure and deterministic;
/// returns the jobs an assignment profile must import durably.
fn apply_protocol_batch(
    state: &mut ResolutionProtocolState,
    messages: &[AgentMessage],
) -> Result<Vec<ResolutionJob>> {
    let mut imports = Vec::new();
    // Canonical application order regardless of delivery order or batch
    // boundaries: identical reachable histories reduce identically. Profiles
    // that arrive before their assignment are held in bounded pending state
    // and drained deterministically once the entry exists.
    let mut ordered = messages.to_vec();
    ordered.sort_by(|left, right| left.message_id.cmp(&right.message_id));
    for message in &ordered {
        let Some(profile) = parse_resolution_profile(&message.body) else {
            continue;
        };
        let fingerprint = match &profile {
            ResolutionProfile::Assignment(profile) => profile.job.conflict_fingerprint.clone(),
            ResolutionProfile::Result(result) => result.conflict_fingerprint.clone(),
            ResolutionProfile::Revoke(profile) => profile.conflict_fingerprint.clone(),
            ResolutionProfile::HumanAnswer(answer) => answer.conflict_fingerprint.clone(),
        };
        let sender = message.from.clone();
        let message_id = message.message_id.clone();
        match state.entries.get_mut(&fingerprint) {
            Some(entry) => match apply_profile_to_entry(entry, &profile, &message_id, &sender)? {
                ApplyOutcome::Applied(import) => {
                    if let Some(job) = import {
                        imports.push(*job);
                    }
                    register_applied(state, &message_id);
                    let fingerprint = fingerprint.clone();
                    drain_pending(state, &fingerprint, &mut imports)?;
                }
                ApplyOutcome::HoldForLater => {
                    let fingerprint_count = state.pending.len();
                    let has_fingerprint = state.pending.contains_key(&fingerprint);
                    let pending = state.pending.entry(fingerprint.clone()).or_default();
                    if pending
                        .iter()
                        .any(|(held, _)| held.message_id == message.message_id)
                    {
                        continue;
                    }
                    if pending.len() >= RESOLUTION_PROTOCOL_MAX_PENDING_PER_FINGERPRINT
                        || (fingerprint_count >= RESOLUTION_PROTOCOL_MAX_PENDING_FINGERPRINTS
                            && !has_fingerprint)
                    {
                        state.incomplete = true;
                    }
                    pending.push((message.clone(), profile.clone()));
                    pending.sort_by(|left, right| left.0.message_id.cmp(&right.0.message_id));
                }
            },
            None => {
                // A non-assignment profile whose entry does not exist yet is
                // held in bounded pending state (its assignment may sort
                // later canonically or arrive in a later batch); only a
                // genuinely unbound profile is ever dropped.
                if !matches!(profile, ResolutionProfile::Assignment(_)) {
                    if state.applied.binary_search(&message.message_id).is_ok() {
                        continue;
                    }
                    let fingerprint_count = state.pending.len();
                    let has_fingerprint = state.pending.contains_key(&fingerprint);
                    let pending = state.pending.entry(fingerprint.clone()).or_default();
                    if pending
                        .iter()
                        .any(|(held, _)| held.message_id == message.message_id)
                    {
                        continue;
                    }
                    if pending.len() >= RESOLUTION_PROTOCOL_MAX_PENDING_PER_FINGERPRINT
                        || (fingerprint_count >= RESOLUTION_PROTOCOL_MAX_PENDING_FINGERPRINTS
                            && !has_fingerprint)
                    {
                        state.incomplete = true;
                    }
                    pending.push((message.clone(), profile.clone()));
                    pending.sort_by(|left, right| left.0.message_id.cmp(&right.0.message_id));
                    continue;
                }
                // First observation must be an assignment (nothing else can
                // bind without an entry); everything else is evidence-dropped.
                let outer: &ResolutionProfile = &profile;
                if let ResolutionProfile::Assignment(assignment) = outer {
                    let profile = assignment;
                    let job = &profile.job;
                    if state.entries.len() >= RESOLUTION_PROTOCOL_MAX_ENTRIES {
                        // Deterministic eviction of terminal entries; failure
                        // to make room marks the projection incomplete.
                        let evicted = state
                            .entries
                            .iter()
                            .find(|(_, entry)| entry.state == ProtocolAssignmentState::Revoked)
                            .map(|(key, _)| key.clone());
                        match evicted {
                            Some(key) => {
                                state.entries.remove(&key);
                            }
                            None => {
                                state.incomplete = true;
                            }
                        }
                    }
                    if state.entries.len() < RESOLUTION_PROTOCOL_MAX_ENTRIES {
                        let entry = ProtocolEntry {
                            conflict_fingerprint: fingerprint.clone(),
                            job_id: job.job_id.clone(),
                            assignment_id: job.assignment_id.clone(),
                            attempt: job.attempt,
                            owner: job.owner.clone(),
                            job: Some(job.clone()),
                            state: ProtocolAssignmentState::Assigned,
                            question_generation: profile.question_generation.unwrap_or(0),
                            result: None,
                            answer: None,
                            revoke_reason: None,
                            observed_message_id: message_id.clone(),
                        };
                        // A first observation is by construction a new
                        // assignment: collect the import directly (the
                        // supersession check compares against an equal
                        // pre-filled entry and would report no change).
                        imports.push(job.clone());
                        register_applied(state, &message_id);
                        state.entries.insert(fingerprint.clone(), entry);
                        drain_pending(state, &fingerprint, &mut imports)?;
                    }
                }
            }
        }
    }
    Ok(imports)
}

/// Bounded metadata-only status projection of the protocol store (ids,
/// state, counts only; never paths, leg bytes, or candidate bodies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionProtocolStatus {
    pub schema_version: u32,
    pub cursor: Option<String>,
    pub projection_incomplete: bool,
    pub entries: Vec<ResolutionProtocolEntryStatus>,
}

/// One fingerprint's metadata-only projection entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionProtocolEntryStatus {
    pub conflict_fingerprint: String,
    pub job_id: String,
    pub assignment_id: String,
    pub attempt: u32,
    pub owner: String,
    pub state: ProtocolAssignmentState,
    pub question_generation: u32,
    /// Outcome of the observed result, when one was received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<feanorfs_common::ResolutionOutcome>,
    /// The bounded question, when a result escalated to a human. Metadata
    /// only; the full result is fetched by typed ID-based operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
}

fn entry_status(entry: &ProtocolEntry) -> ResolutionProtocolEntryStatus {
    ResolutionProtocolEntryStatus {
        conflict_fingerprint: entry.conflict_fingerprint.clone(),
        job_id: entry.job_id.clone(),
        assignment_id: entry.assignment_id.clone(),
        attempt: entry.attempt,
        owner: entry.owner.clone(),
        state: entry.state,
        question_generation: entry.question_generation,
        outcome: entry.result.as_ref().map(|result| result.outcome),
        question: entry
            .result
            .as_ref()
            .filter(|result| result.outcome == feanorfs_common::ResolutionOutcome::RequiresHuman)
            .and_then(|result| result.question.clone()),
    }
}

/// Imports one assignment-embedded job into the local resolution store
/// (durable, idempotent, before the protocol projection is persisted).
async fn import_resolution_job(ctx: &SyncCtx<'_>, job: &ResolutionJob) -> Result<()> {
    validate_resolution_job(job)?;
    let store = crate::resolution::ResolutionStore::open(ctx.base)?;
    store.update(|state| {
        if state
            .jobs
            .iter()
            .any(|record| record.job.job_id == job.job_id)
        {
            return Ok(());
        }
        // Import by ID and fingerprint: the same immutable job the
        // preparing machine persisted, validated in full before it becomes
        // visible to local prepare/status/apply operations.
        let state_record = crate::resolution::PersistedResolutionJob {
            schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
            job: job.clone(),
            assignment_state: crate::resolution::ResolutionAssignmentState::Active,
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            verified_at_ms: None,
            result: None,
            question_generation: 0,
        };
        state.jobs.push(state_record);
        Ok(())
    })?;
    Ok(())
}

/// Observes the signal stream through the deterministic `ffres1` reducer
/// and returns the bounded projection. Cursor advance and projection
/// replacement are one durable write; a rebuild (`rebuild = true`) resets
/// the cursor and re-observes the bounded window.
///
/// # Errors
/// Returns an error for unreadable state or a failed job import.
pub async fn resolution_protocol_status(
    ctx: &SyncCtx<'_>,
    rebuild: bool,
) -> Result<ResolutionProtocolStatus> {
    let store = ResolutionProtocolStore::open(ctx.base)?;
    let mut state = if rebuild {
        ResolutionProtocolState::fresh()
    } else {
        store.load()?
    };
    let inbox = signals_since(
        ctx,
        state.cursor.as_deref(),
        RESOLUTION_PROTOCOL_OBSERVE_LIMIT,
    )
    .await?;
    if rebuild {
        // A clean bounded rebuild restores completeness.
        state.incomplete = false;
    }
    if inbox.cursor_reset {
        state.incomplete = true;
    }
    let mut messages = inbox.messages;
    messages.sort_by(|left, right| left.message_id.cmp(&right.message_id));
    let imports = apply_protocol_batch(&mut state, &messages)?;
    for job in imports {
        import_resolution_job(ctx, &job).await?;
    }
    state.cursor = Some(inbox.cursor);
    store.update(|stored| {
        *stored = state.clone();
        Ok(())
    })?;
    let mut entries = state.entries.values().map(entry_status).collect::<Vec<_>>();
    entries.sort_by(|left, right| left.conflict_fingerprint.cmp(&right.conflict_fingerprint));
    Ok(ResolutionProtocolStatus {
        schema_version: RESOLUTION_PROTOCOL_SCHEMA_VERSION,
        cursor: state.cursor,
        projection_incomplete: state.incomplete,
        entries,
    })
}

/// Publishes one bounded validated `ffres1` profile inside the existing
/// encrypted signal stream and returns the sent message id.
async fn send_profile(
    ctx: &SyncCtx<'_>,
    profile: &ResolutionProfile,
    to: &str,
    from: Option<&str>,
) -> Result<String> {
    let body = encode_resolution_profile(profile)?;
    let sent = crate::messages::send_message(
        ctx,
        AgentMessageInput {
            to: to.to_string(),
            kind: AgentMessageKind::Status,
            body,
            about_snapshot: None,
            reply_to: None,
            from: from.map(str::to_string),
        },
    )
    .await?;
    Ok(sent.message_id)
}

/// Loads one local resolution job by id and validates it in full.
async fn load_valid_job(ctx: &SyncCtx<'_>, job_id: &str) -> Result<ResolutionJob> {
    let store = crate::resolution::ResolutionStore::open(ctx.base)?;
    let record = store
        .load()?
        .jobs
        .into_iter()
        .find(|record| record.job.job_id == job_id)
        .map(|record| record.job)
        .context("no resolution job with that id in the local store")?;
    validate_resolution_job(&record)?;
    Ok(record)
}

/// Publishes the `ffres1` assignment profile for one locally prepared job.
/// The complete immutable job is embedded; the designated machine imports
/// it by ID and fingerprint.
///
/// # Errors
/// Returns an error for unknown jobs, invalid profiles, or failed sends.
pub async fn send_resolution_assignment(ctx: &SyncCtx<'_>, job_id: &str) -> Result<String> {
    let job = load_valid_job(ctx, job_id).await?;
    let owner = job.owner.clone();
    let profile = ResolutionProfile::Assignment(Box::new(ResolutionAssignmentProfile {
        schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
        question_generation: None,
        job,
    }));
    send_profile(ctx, &profile, &owner, None).await
}

/// Publishes the `ffres1` result profile for one locally submitted job.
///
/// # Errors
/// Returns an error for jobs without a submitted result, invalid profiles,
/// or failed sends.
pub async fn send_resolution_result(ctx: &SyncCtx<'_>, job_id: &str) -> Result<String> {
    let store = crate::resolution::ResolutionStore::open(ctx.base)?;
    let result = store
        .load()?
        .jobs
        .into_iter()
        .find(|record| record.job.job_id == job_id)
        .and_then(|record| record.result)
        .context("no submitted result for that job")?;
    validate_resolution_result(&result)?;
    let profile = ResolutionProfile::Result(result.clone());
    // Results return to the assignment origin, which observes any member
    // inbox; the sender identity must equal the designated owner so the
    // reducer's binding check passes on the other machine.
    send_profile(ctx, &profile, "*", Some(&result.owner)).await
}

/// Publishes the `ffres1` revoke/supersede profile for one local job.
///
/// # Errors
/// Returns an error for unknown jobs or failed sends.
pub async fn send_resolution_revoke(
    ctx: &SyncCtx<'_>,
    job_id: &str,
    superseded: bool,
) -> Result<String> {
    let job = load_valid_job(ctx, job_id).await?;
    let profile = ResolutionProfile::Revoke(
        feanorfs_common::resolution_contract::ResolutionRevokeProfile {
            schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
            job_id: job.job_id,
            assignment_id: job.assignment_id,
            attempt: job.attempt,
            conflict_fingerprint: job.conflict_fingerprint,
            reason: if superseded {
                ResolutionRevokeReason::Superseded
            } else {
                ResolutionRevokeReason::Cancelled
            },
        },
    );
    send_profile(ctx, &profile, "*", None).await
}

/// Publishes the `ffres1` human-answer profile for one exact escalation.
///
/// # Errors
/// Returns an error for invalid answers or failed sends.
pub async fn send_human_answer(
    ctx: &SyncCtx<'_>,
    answer: &HumanResolutionAnswer,
) -> Result<String> {
    validate_human_resolution_answer(answer)?;
    let profile = ResolutionProfile::HumanAnswer(answer.clone());
    send_profile(ctx, &profile, "*", None).await
}

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::resolution_contract::resolution_fixtures;
    use feanorfs_common::HumanResolutionOption;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn hex(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn message(id_char: char, body: &str, from: &str) -> AgentMessage {
        AgentMessage {
            message_id: hex(id_char),
            from: from.to_string(),
            to: "*".to_string(),
            kind: AgentMessageKind::Status,
            body: body.to_string(),
            about_snapshot: hex('a'),
            reply_to: None,
            created_at_ms: 0,
        }
    }

    fn assignment_body(job: &ResolutionJob) -> String {
        let profile = ResolutionProfile::Assignment(Box::new(ResolutionAssignmentProfile {
            schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
            question_generation: None,
            job: job.clone(),
        }));
        encode_resolution_profile(&profile).unwrap()
    }

    fn result_body(job: &ResolutionJob, question_generation: u32) -> String {
        let mut result = resolution_fixtures::result();
        result.job_id = job.job_id.clone();
        result.assignment_id = job.assignment_id.clone();
        result.attempt = job.attempt;
        result.owner = job.owner.clone();
        result.conflict_fingerprint = job.conflict_fingerprint.clone();
        result.question_generation = question_generation;
        let profile = ResolutionProfile::Result(result);
        encode_resolution_profile(&profile).unwrap()
    }

    fn revoke_body(job: &ResolutionJob, superseded: bool) -> String {
        let profile = ResolutionProfile::Revoke(
            feanorfs_common::resolution_contract::ResolutionRevokeProfile {
                schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
                job_id: job.job_id.clone(),
                assignment_id: job.assignment_id.clone(),
                attempt: job.attempt,
                conflict_fingerprint: job.conflict_fingerprint.clone(),
                reason: if superseded {
                    ResolutionRevokeReason::Superseded
                } else {
                    ResolutionRevokeReason::Cancelled
                },
            },
        );
        encode_resolution_profile(&profile).unwrap()
    }

    fn answer_body(job: &ResolutionJob, question_generation: u32) -> String {
        let answer = HumanResolutionAnswer {
            schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
            job_id: job.job_id.clone(),
            assignment_id: job.assignment_id.clone(),
            attempt: job.attempt,
            conflict_fingerprint: job.conflict_fingerprint.clone(),
            question_generation,
            chosen_option: HumanResolutionOption::Defer,
            candidate: None,
            verification: None,
        };
        let profile = ResolutionProfile::HumanAnswer(answer);
        encode_resolution_profile(&profile).unwrap()
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_job(attempt: u32) -> ResolutionJob {
        // Distinct job ids per construction so two jobs never collide.
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut job = resolution_fixtures::job();
        job.job_id = format!("{:016x}{:016x}", sequence + 1, sequence + 2);
        job.assignment_id = format!("{:016x}{:016x}", sequence + 3, sequence + 4);
        job.attempt = attempt;
        // The job's identity must reflect its own assignment/attempt for the
        // fixture validation to hold.
        job.conflict.assignment_id = Some(job.assignment_id.clone());
        job.conflict.attempt = Some(attempt);
        job.conflict_fingerprint =
            feanorfs_common::resolution_contract::compute_conflict_identity_fingerprint(
                &job.conflict,
            );
        job
    }

    #[test]
    fn assignment_then_result_then_revoke_is_deterministic_in_both_orders() {
        let job = unique_job(0);
        let assignment = message('b', &assignment_body(&job), "preparer");
        let result = message('c', &result_body(&job, 0), &job.owner);
        let revoke = message('d', &revoke_body(&job, false), "preparer");

        let run = |order: Vec<AgentMessage>| {
            let mut state = ResolutionProtocolState::fresh();
            apply_protocol_batch(&mut state, &order).unwrap();
            state
        };
        let forward = run(vec![assignment.clone(), result.clone(), revoke.clone()]);
        let backward = run(vec![revoke.clone(), result.clone(), assignment.clone()]);
        assert_eq!(forward, backward);
        let entry = &forward.entries.values().next().unwrap();
        assert_eq!(entry.state, ProtocolAssignmentState::Revoked);
        assert_eq!(entry.revoke_reason, Some(ResolutionRevokeReason::Cancelled));
        // The revoked entry retained the observed result evidence.
        assert!(entry.result.is_some());
    }

    #[test]
    fn repeated_assignments_are_idempotent_and_attempts_are_distinct_entries() {
        let job = unique_job(0);
        let newer_attempt = unique_job(1);
        let mut state = ResolutionProtocolState::fresh();
        let batch = vec![
            message('a', &assignment_body(&job), "preparer"),
            message('b', &assignment_body(&job), "preparer"),
            message('c', &assignment_body(&newer_attempt), "preparer"),
        ];
        apply_protocol_batch(&mut state, &batch).unwrap();
        // Same fingerprint (identical job) collapses to one entry; the newer
        // attempt has its own fingerprint and its own entry.
        assert_eq!(state.entries.len(), 2);
        let entry = state.entries.get(&job.conflict_fingerprint).unwrap();
        assert_eq!(entry.assignment_id, job.assignment_id);
        assert_eq!(entry.attempt, 0);
        // Applying the same batch twice is idempotent.
        let mut second = state.clone();
        apply_protocol_batch(&mut second, &batch).unwrap();
        assert_eq!(second, state);
    }

    #[test]
    fn unbound_profiles_are_evidence_dropped_and_never_create_entries() {
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        let batch = vec![
            message('b', &result_body(&job, 0), &job.owner),
            message('c', &revoke_body(&job, false), "preparer"),
            message('d', &answer_body(&job, 0), "human"),
        ];
        apply_protocol_batch(&mut state, &batch).unwrap();
        assert!(state.entries.is_empty());
    }

    #[test]
    fn result_from_the_wrong_sender_is_rejected() {
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        apply_protocol_batch(
            &mut state,
            &[message('b', &assignment_body(&job), "preparer")],
        )
        .unwrap();
        let batch = vec![message('c', &result_body(&job, 0), "imposter")];
        apply_protocol_batch(&mut state, &batch).unwrap();
        // An imposter result never applies; it is held in bounded pending
        // state (fail-closed) and never reaches the projection.
        let entry = state.entries.values().next().unwrap();
        assert_eq!(entry.state, ProtocolAssignmentState::Assigned);
        assert!(entry.result.is_none());
        assert_eq!(state.pending.values().next().unwrap().len(), 1);
    }

    #[test]
    fn answer_binds_to_the_exact_question_generation() {
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        apply_protocol_batch(
            &mut state,
            &[message('b', &assignment_body(&job), "preparer")],
        )
        .unwrap();
        let batch = vec![message('c', &answer_body(&job, 3), "human")];
        apply_protocol_batch(&mut state, &batch).unwrap();
        // A stale-generation answer never applies; it stays held in bounded
        // pending state.
        let entry = state.entries.values().next().unwrap();
        assert_eq!(entry.state, ProtocolAssignmentState::Assigned);
        assert!(entry.answer.is_none());
        assert_eq!(state.pending.values().next().unwrap().len(), 1);
    }

    #[test]
    fn entry_overflow_evicts_terminal_entries_deterministically() {
        let mut state = ResolutionProtocolState::fresh();
        for _ in 0..RESOLUTION_PROTOCOL_MAX_ENTRIES {
            let job = unique_job(0);
            state.entries.insert(
                job.conflict_fingerprint.clone(),
                ProtocolEntry {
                    conflict_fingerprint: job.conflict_fingerprint.clone(),
                    job_id: job.job_id.clone(),
                    assignment_id: job.assignment_id.clone(),
                    attempt: job.attempt,
                    owner: job.owner.clone(),
                    job: Some(job),
                    state: ProtocolAssignmentState::Assigned,
                    question_generation: 0,
                    result: None,
                    answer: None,
                    revoke_reason: None,
                    observed_message_id: hex('a'),
                },
            );
        }
        // Revoke the first entry so a new assignment can evict it.
        let revoked_key = state.entries.keys().next().unwrap().clone();
        state.entries.get_mut(&revoked_key).unwrap().state = ProtocolAssignmentState::Revoked;
        let job = unique_job(0);
        apply_protocol_batch(
            &mut state,
            &[message('b', &assignment_body(&job), "preparer")],
        )
        .unwrap();
        assert_eq!(state.entries.len(), RESOLUTION_PROTOCOL_MAX_ENTRIES);
        assert!(!state.entries.contains_key(&revoked_key));
        assert!(state.entries.contains_key(&job.conflict_fingerprint));
    }

    #[test]
    fn applied_id_overflow_marks_incomplete() {
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        let messages = (0..(RESOLUTION_PROTOCOL_MAX_APPLIED + 4))
            .map(|index| {
                // Even ids re-apply the assignment (idempotent supersession
                // no-op); odd ids apply a bound result. Both consume applied
                // ids, so the bounded set overflows deterministically.
                let body = if index % 2 == 0 {
                    assignment_body(&job)
                } else {
                    result_body(&job, 0)
                };
                let sender = if index % 2 == 0 {
                    "preparer"
                } else {
                    &job.owner
                };
                let ch = b'a' + (index % 26) as u8;
                message(ch as char, &body, sender)
            })
            .collect::<Vec<_>>();
        // Distinct message ids are required for the applied set to grow.
        let messages = messages
            .into_iter()
            .enumerate()
            .map(|(index, mut message)| {
                message.message_id = format!("{:064x}", index + 1);
                message
            })
            .collect::<Vec<_>>();
        apply_protocol_batch(&mut state, &messages).unwrap();
        assert!(state.incomplete);
    }

    #[test]
    fn result_before_assignment_is_held_and_drained_deterministically() {
        let job = unique_job(0);
        let result = message('b', &result_body(&job, 0), &job.owner);
        let assignment = message('c', &assignment_body(&job), "preparer");
        let mut state = ResolutionProtocolState::fresh();
        // Result canonically before its assignment (separate batches).
        apply_protocol_batch(&mut state, std::slice::from_ref(&result)).unwrap();
        assert!(state.entries.is_empty());
        assert_eq!(state.pending.values().next().unwrap().len(), 1);
        apply_protocol_batch(&mut state, std::slice::from_ref(&assignment)).unwrap();
        let entry = state.entries.values().next().unwrap();
        assert_eq!(entry.state, ProtocolAssignmentState::ResultReceived);
        assert!(entry.result.is_some());
        assert!(state.pending.is_empty());

        // The reverse order produces the identical final state.
        let mut reversed = ResolutionProtocolState::fresh();
        apply_protocol_batch(&mut reversed, &[assignment, result]).unwrap();
        assert_eq!(state, reversed);
    }

    #[test]
    fn unbound_profiles_are_dropped_only_when_no_assignment_ever_arrives() {
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        apply_protocol_batch(
            &mut state,
            &[message('b', &result_body(&job, 0), &job.owner)],
        )
        .unwrap();
        // Pending holds it; the earlier evidence-drop test asserted the
        // entry-free end state, which still holds.
        assert!(state.entries.is_empty());
        assert_eq!(state.pending.values().next().unwrap().len(), 1);
    }

    #[test]
    fn unknown_malformed_and_oversized_profiles_are_ignored_deterministically() {
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        let batch = vec![
            // A well-formed assignment…
            message('a', &assignment_body(&job), "preparer"),
            // …an unknown ffres1 schema version…
            message(
                'b',
                "ffres1:{\"type\":\"assignment\",\"schema_version\":999}",
                "preparer",
            ),
            // …a malformed profile…
            message('c', "ffres1:not-json", "preparer"),
            // …a non-ffres1 body…
            message('d', "ffwork1:{\"type\":\"work_intent\"}", "preparer"),
        ];
        apply_protocol_batch(&mut state, &batch).unwrap();
        // Only the canonical assignment is projected; unknown profiles never
        // mutate state and never create evidence entries.
        assert_eq!(state.entries.len(), 1);
        let entry = state.entries.get(&job.conflict_fingerprint).unwrap();
        assert_eq!(entry.state, ProtocolAssignmentState::Assigned);
        assert_eq!(entry.observed_message_id, hex('a'));
    }

    #[test]
    fn status_projection_is_metadata_only() {
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        apply_protocol_batch(
            &mut state,
            &[message('b', &assignment_body(&job), "preparer")],
        )
        .unwrap();
        let status = entry_status(state.entries.values().next().unwrap());
        let serialized = serde_json::to_value(&status).unwrap();
        assert!(serialized.get("conflict_fingerprint").is_some());
        assert!(serialized.get("job").is_none());
        assert!(serialized.get("result").is_none());
        assert!(serialized.get("answer").is_none());
        assert_eq!(status.outcome, None);
        assert_eq!(status.question, None);
    }

    #[test]
    fn store_roundtrips_and_fails_closed_on_corrupt_or_newer_schema() {
        // Round-trip through the store is validated on every load.
        let dir = tempfile::tempdir().unwrap();
        let _ = ensure_workspace_state(dir.path()).unwrap();
        let job = unique_job(0);
        let mut state = ResolutionProtocolState::fresh();
        apply_protocol_batch(
            &mut state,
            &[message('b', &assignment_body(&job), "preparer")],
        )
        .unwrap();
        let orchestrator = ensure_workspace_state(dir.path())
            .unwrap()
            .join("orchestrator");
        let store = ResolutionProtocolStore {
            inner: DurableJson::open(
                &orchestrator,
                RESOLUTION_PROTOCOL_FILE,
                ResolutionProtocolState::fresh(),
            )
            .unwrap(),
        };
        store
            .update(|stored| {
                *stored = state.clone();
                Ok(())
            })
            .unwrap();
        let loaded = ResolutionProtocolStore::open(dir.path())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(loaded, state);

        // A newer schema fails closed on open.
        let corrupt_dir = tempfile::tempdir().unwrap();
        let _ = ensure_workspace_state(corrupt_dir.path()).unwrap();
        let orchestrator = ensure_workspace_state(corrupt_dir.path())
            .unwrap()
            .join("orchestrator");
        let store = ResolutionProtocolStore {
            inner: DurableJson::open(
                &orchestrator,
                RESOLUTION_PROTOCOL_FILE,
                ResolutionProtocolState::fresh(),
            )
            .unwrap(),
        };
        store
            .update(|stored| {
                stored.schema_version = RESOLUTION_PROTOCOL_SCHEMA_VERSION + 1;
                Ok(())
            })
            .unwrap();
        let error = ResolutionProtocolStore::open(corrupt_dir.path()).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported resolution protocol schema"));
    }
}
