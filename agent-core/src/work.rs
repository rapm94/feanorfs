//! Deterministic work-intent reducer (`ffwork1`).
//!
//! Consumes typed `AgentMessage` records from the existing encrypted signal
//! traversal (`messages::signals_since`) — never a second message store —
//! and projects a deterministic, rebuildable, private work state under the
//! protected workspace `orchestrator/` boundary.
//!
//! Authority model:
//! - Author transitions key by `(task_id, agent, sequence)`; a transition
//!   cannot decrease sequence, change immutable identity, skip required
//!   states, or act for another author.
//! - Coordinator decisions key by exact proposal message id plus the
//!   authorized coordinator identity (named by the proposal, supplied by the
//!   operating context, or `human`).
//! - Causal dominance wins. Concurrent same-author updates use the canonical
//!   message id only as a deterministic tie-breaker; the losing branch is
//!   retained as bounded protocol evidence.
//! - Duplicate delivery is idempotent. Clock fields are display/liveness
//!   hints only and never drive decisions.
//! - Cursor-reset rebuild walks a bounded reachable closure and marks the
//!   projection incomplete rather than inferring acceptance it cannot prove.

use crate::durable::DurableJson;
use crate::messages::{send_message, signals_since};
use crate::SyncCtx;
use anyhow::{ensure, Result};
use feanorfs_common::{
    encode_work_profile, evaluate_scope_overlap, parse_work_profile, AgentMessage,
    AgentMessageInput, AgentMessageKind, AgentSendResult, WorkAmendInput, WorkAmendmentStatus,
    WorkBlockInput, WorkCompleteInput, WorkDecideInput, WorkDecisionKind, WorkDecisionStatus,
    WorkOverlap, WorkOverlapAcceptance, WorkProfile, WorkProposalStatus, WorkProposeInput,
    WorkRejectReason, WorkScope, WorkSendResult, WorkSettleInput, WorkStatusInput,
    WorkStatusResult, WorkTaskState, WorkTaskStatus, WorkVerification, WorkYieldInput,
    WORK_MAX_ACTIVE_TASKS, WORK_MAX_AMENDMENTS, WORK_MAX_CAPABILITIES, WORK_MAX_EVIDENCE,
    WORK_MAX_PENDING, WORK_MAX_PROPOSALS_PER_TASK, WORK_MAX_SEEN, WORK_MAX_TERMINAL_TASKS,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

const WORK_STATE_SCHEMA_VERSION: u32 = 1;
const WORK_STATE_FILE: &str = "work-state.json";
const WORK_OBSERVE_LIMIT: usize = feanorfs_common::AGENT_INBOX_MAX_LIMIT;
/// Maximum number of applied transition message ids retained as authenticated
/// ancestry. Overflow marks the projection incomplete (acceptance is no
/// longer fully provable from the bounded set).
const WORK_MAX_APPLIED: usize = 4096;

/// Fully persisted work reducer state file (schema version 1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkStateFile {
    pub schema_version: u32,
    /// Workspace head cursor of the last processed signal read.
    pub cursor: Option<String>,
    /// True when the last rebuild/read could not cover all history; while
    /// true, acceptance is not fully provable.
    pub incomplete: bool,
    pub tasks: Vec<WorkTaskRecord>,
    /// Bounded protocol evidence: losing branches, invalid transitions,
    /// superseded decisions.
    pub evidence: Vec<WorkEvidenceRecord>,
    /// Bounded set of seen message ids (sorted) retained for dedup /
    /// idempotency bookkeeping; never used for causal satisfaction.
    pub seen: Vec<String>,
    /// Authenticated applied ancestry: exact message ids whose transitions
    /// were applied by the reducer, sorted. Causal-base satisfaction and
    /// admission use this set, never the bounded observation cache. Bound
    /// exhaustion marks the projection incomplete.
    #[serde(default)]
    pub applied: Vec<String>,
    /// Bounded not-yet-appliable transitions re-examined on later observes.
    pub pending: Vec<WorkPendingRecord>,
    /// Count of transitions dropped by bound exhaustion.
    pub dropped_count: u64,
    /// Display/liveness hint only.
    pub updated_at_ms: i64,
}

impl WorkStateFile {
    fn fresh() -> Self {
        Self {
            schema_version: WORK_STATE_SCHEMA_VERSION,
            cursor: None,
            incomplete: false,
            tasks: Vec::new(),
            evidence: Vec::new(),
            seen: Vec::new(),
            applied: Vec::new(),
            pending: Vec::new(),
            dropped_count: 0,
            updated_at_ms: 0,
        }
    }
}

/// One task's bounded proposal chains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkTaskRecord {
    pub task_id: String,
    pub proposals: Vec<WorkProposalRecord>,
    pub updated_at_ms: i64,
}

/// One applied proposal chain for one `(task_id, agent)` author key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkProposalRecord {
    pub agent: String,
    pub sequence: u64,
    /// Exact intent message id; decisions reference this as the proposal id.
    pub intent_message_id: String,
    pub coordinator: Option<String>,
    pub causal_base: Option<String>,
    /// Original scope declared by the intent.
    pub original_scope: WorkScope,
    /// Current accepted scope (post-decision-narrow and post-amendment).
    pub scope: WorkScope,
    /// Advertised capabilities from the applied intent, preserved through
    /// every transition and rebuild so admission and designation can use the
    /// current authenticated capability set.
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub state: WorkTaskState,
    pub decision: Option<WorkDecisionRecord>,
    /// Decision message ids superseded for this proposal (bounded).
    pub superseded_decisions: Vec<String>,
    pub amendments: Vec<WorkAmendmentRecord>,
    pub accepted_overlap: Vec<WorkOverlapAcceptance>,
    pub verification: Option<WorkVerification>,
    pub inspected_snapshot: Option<String>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    /// Exact message id of the last applied transition.
    pub source_message_id: String,
    /// Deterministic unwind point for a same-sequence concurrent author
    /// transition: the smaller canonical message id displaces the applied
    /// (larger) one by restoring this snapshot, then re-applying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_restore: Option<WorkAuthorRestore>,
    /// Display/liveness hint only.
    pub updated_at_ms: i64,
}

/// Pre-transition snapshot captured when an author transition applies; lets a
/// smaller-id concurrent fork unwind the applied (larger) transition
/// deterministically regardless of delivery order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkAuthorRestore {
    /// The applied transition whose effects this snapshot undoes.
    pub source_message_id: String,
    pub sequence: u64,
    pub state: WorkTaskState,
    pub scope: WorkScope,
    pub reason: Option<String>,
    pub verification: Option<WorkVerification>,
    pub inspected_snapshot: Option<String>,
    pub outcome: Option<String>,
    pub amendments_len: usize,
}

/// One applied coordinator decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDecisionRecord {
    pub message_id: String,
    pub coordinator: String,
    pub kind: WorkDecisionKind,
    pub ordered_after: Option<String>,
    /// Pre-decision proposal snapshot captured when this decision applied;
    /// lets a smaller-id concurrent decision unwind it deterministically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<WorkDecisionRestore>,
}

/// Pre-decision snapshot captured when a decision applies; restores the
/// proposal so a smaller-id concurrent decision can replace the applied
/// (larger) one deterministically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDecisionRestore {
    pub scope: WorkScope,
    pub accepted_overlap: Vec<WorkOverlapAcceptance>,
    pub reason: Option<String>,
}

/// One applied amendment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkAmendmentRecord {
    pub message_id: String,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Exact applied coordinator decision message id approving a scope
    /// expansion beyond the original declared scope, when the amendment
    /// required approval. Included in status `causal_refs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_decision_id: Option<String>,
}

/// Bounded protocol evidence record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkEvidenceRecord {
    pub message_id: String,
    pub task_id: String,
    pub disposition: String,
    pub state: WorkTaskState,
}

/// Bounded not-yet-appliable transition retained for re-examination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkPendingRecord {
    pub message_id: String,
    pub from: String,
    pub profile: WorkProfile,
    /// Display/liveness hint only.
    pub created_at_ms: i64,
}

/// Crash-safe reducer state store (advisory lock + atomic replacement).
pub struct WorkStore {
    inner: DurableJson<WorkStateFile>,
}

impl WorkStore {
    /// Opens (creating when absent) the orchestrator work store for a
    /// workspace. Corrupt or unsupported-schema state fails closed.
    pub fn open(base: &Path) -> Result<Self> {
        let dir = crate::workspace_layout::ensure_workspace_state(base)?.join("orchestrator");
        let inner = DurableJson::open(&dir, WORK_STATE_FILE, WorkStateFile::fresh())?;
        inner.with_read(|state| {
            ensure!(
                state.schema_version == WORK_STATE_SCHEMA_VERSION,
                "unsupported work reducer state schema {} (expected {WORK_STATE_SCHEMA_VERSION}); \
                 do not infer work state from signal history alone",
                state.schema_version
            );
            Ok(())
        })?;
        Ok(Self { inner })
    }

    pub fn load(&self) -> Result<WorkStateFile> {
        self.inner.with_read(|state| {
            ensure!(
                state.schema_version == WORK_STATE_SCHEMA_VERSION,
                "unsupported work reducer state schema {} (expected {WORK_STATE_SCHEMA_VERSION}); \
                 do not infer work state from signal history alone",
                state.schema_version
            );
            Ok(state.clone())
        })
    }

    pub fn update(
        &self,
        f: impl FnOnce(&mut WorkStateFile) -> Result<()>,
    ) -> Result<WorkStateFile> {
        self.inner.with_write(|state| {
            ensure!(
                state.schema_version == WORK_STATE_SCHEMA_VERSION,
                "unsupported work reducer state schema {}",
                state.schema_version
            );
            f(state)?;
            state.schema_version = WORK_STATE_SCHEMA_VERSION;
            Ok(())
        })?;
        self.load()
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        self.inner.state_path.as_path()
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One transition candidate inside an apply pass.
struct WorkCandidate {
    message_id: String,
    from: String,
    profile: WorkProfile,
    created_at_ms: i64,
}

enum ApplyOutcome {
    Applied,
    Pending,
    Evidence(WorkRejectReason),
    /// Dropped by bound exhaustion (counted, no evidence record).
    Dropped,
}

/// Applies one observe batch to the reducer state.
///
/// When `rebuild` (cursor reset), the projection is re-derived from the
/// supplied bounded window only; the caller marks `incomplete`. Processing
/// runs to a fixpoint over candidates sorted by canonical message id, so
/// delivery permutations consistent with ancestry converge deterministically.
/// Returns the number of messages settled (applied, evidenced, or dropped).
fn apply_batch(state: &mut WorkStateFile, messages: &[AgentMessage], rebuild: bool) -> usize {
    if rebuild {
        *state = WorkStateFile::fresh();
    }
    for message in messages {
        insert_seen(&mut state.seen, &message.message_id);
    }

    let mut candidates: Vec<WorkCandidate> = std::mem::take(&mut state.pending)
        .into_iter()
        .map(|pending| WorkCandidate {
            message_id: pending.message_id,
            from: pending.from,
            profile: pending.profile,
            created_at_ms: pending.created_at_ms,
        })
        .collect();
    for message in messages {
        if let Some(profile) = parse_work_profile(&message.body) {
            candidates.push(WorkCandidate {
                message_id: message.message_id.clone(),
                from: message.from.clone(),
                profile,
                created_at_ms: message.created_at_ms,
            });
        }
    }
    candidates.sort_by(|a, b| a.message_id.cmp(&b.message_id));

    let mut settled = 0usize;
    loop {
        let mut progressed = false;
        let mut remaining: Vec<WorkCandidate> = Vec::new();
        for candidate in candidates {
            match apply_transition(state, &candidate) {
                ApplyOutcome::Applied => {
                    progressed = true;
                    settled += 1;
                    insert_applied(
                        &mut state.applied,
                        &candidate.message_id,
                        &mut state.incomplete,
                    );
                }
                ApplyOutcome::Pending => remaining.push(candidate),
                ApplyOutcome::Evidence(reason) => {
                    push_evidence(state, &candidate, reason);
                    settled += 1;
                }
                ApplyOutcome::Dropped => {
                    // Any dropped transition is bound exhaustion on
                    // non-terminal data: acceptance is not fully provable.
                    state.dropped_count = state.dropped_count.saturating_add(1);
                    state.incomplete = true;
                    settled += 1;
                }
            }
        }
        candidates = remaining;
        if !progressed || candidates.is_empty() {
            break;
        }
    }
    for candidate in candidates {
        push_pending(state, candidate);
    }
    maintain_bounds(state);
    settled
}

fn insert_applied(applied: &mut Vec<String>, message_id: &str, incomplete: &mut bool) {
    if applied
        .binary_search_by(|id| id.as_str().cmp(message_id))
        .is_ok()
    {
        return;
    }
    if applied.len() >= WORK_MAX_APPLIED {
        // Deterministic bound: drop the smallest id and mark the projection
        // incomplete (ancestry no longer provable in full).
        applied.remove(0);
        *incomplete = true;
    }
    applied.push(message_id.to_string());
    applied.sort();
}

fn insert_seen(seen: &mut Vec<String>, message_id: &str) {
    if seen
        .binary_search_by(|id| id.as_str().cmp(message_id))
        .is_ok()
    {
        return;
    }
    seen.push(message_id.to_string());
    seen.sort();
    while seen.len() > WORK_MAX_SEEN {
        seen.pop();
    }
}

fn push_evidence(state: &mut WorkStateFile, candidate: &WorkCandidate, reason: WorkRejectReason) {
    state.evidence.push(WorkEvidenceRecord {
        message_id: candidate.message_id.clone(),
        task_id: candidate.profile.task_id().to_string(),
        disposition: reason.as_str().to_string(),
        state: WorkTaskState::Proposed,
    });
}

fn push_pending(state: &mut WorkStateFile, candidate: WorkCandidate) {
    state.pending.push(WorkPendingRecord {
        message_id: candidate.message_id,
        from: candidate.from,
        profile: candidate.profile,
        created_at_ms: candidate.created_at_ms,
    });
    while state.pending.len() > WORK_MAX_PENDING {
        if let Some(max_index) = state
            .pending
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.message_id.cmp(&b.message_id))
            .map(|(index, _)| index)
        {
            state.pending.remove(max_index);
            state.dropped_count = state.dropped_count.saturating_add(1);
            // Dropping a pending transition is bound exhaustion on
            // non-terminal data: acceptance is not fully provable.
            state.incomplete = true;
        }
    }
}

fn maintain_bounds(state: &mut WorkStateFile) {
    // Per-task proposal chains (keep the highest (sequence, message id)).
    for task in &mut state.tasks {
        while task.proposals.len() > WORK_MAX_PROPOSALS_PER_TASK {
            if let Some(min_index) = task
                .proposals
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    a.sequence
                        .cmp(&b.sequence)
                        .then_with(|| a.intent_message_id.cmp(&b.intent_message_id))
                })
                .map(|(index, _)| index)
            {
                let evicted = &task.proposals[min_index];
                if !matches!(
                    evicted.state,
                    WorkTaskState::Completed | WorkTaskState::Blocked
                ) {
                    // Evicting a non-terminal proposal loses provable
                    // acceptance: mark the projection incomplete.
                    state.incomplete = true;
                }
                task.proposals.remove(min_index);
            }
        }
        // Per-proposal amendment history.
        for proposal in &mut task.proposals {
            while proposal.amendments.len() > WORK_MAX_AMENDMENTS {
                if let Some(max_index) = proposal
                    .amendments
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.message_id.cmp(&b.message_id))
                    .map(|(index, _)| index)
                {
                    if !matches!(
                        proposal.state,
                        WorkTaskState::Completed | WorkTaskState::Blocked
                    ) {
                        // Trimming amendment history of a non-terminal
                        // proposal loses provable acceptance.
                        state.incomplete = true;
                    }
                    proposal.amendments.remove(max_index);
                }
            }
        }
    }
    // Bounded terminal history: keep the highest-activity terminal tasks.
    // Evicting terminal records is routine trimming, not incompleteness.
    let terminal: Vec<usize> = state
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| {
            matches!(
                task_state(task),
                WorkTaskState::Completed | WorkTaskState::Blocked
            )
        })
        .map(|(index, _)| index)
        .collect();
    if terminal.len() > WORK_MAX_TERMINAL_TASKS {
        let mut evict = terminal.clone();
        evict.sort_by(|&a, &b| {
            let key = |index: usize| {
                let task = &state.tasks[index];
                let highest = task.proposals.iter().map(|p| p.sequence).max().unwrap_or(0);
                (highest, task.task_id.clone())
            };
            key(a).cmp(&key(b))
        });
        for index in evict
            .iter()
            .take(terminal.len() - WORK_MAX_TERMINAL_TASKS)
            .rev()
        {
            state.tasks.remove(*index);
        }
    }
    // Bounded evidence: keep the canonical (smallest) message ids. Evidence
    // trimming is routine and never marks the projection incomplete.
    while state.evidence.len() > WORK_MAX_EVIDENCE {
        if let Some(max_index) = state
            .evidence
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.message_id.cmp(&b.message_id))
            .map(|(index, _)| index)
        {
            state.evidence.remove(max_index);
        }
    }
}

/// Derives the task-level state: highest-priority proposal state, with
/// terminal states dominating; ties break by sequence then intent message id.
fn task_state(task: &WorkTaskRecord) -> WorkTaskState {
    task.proposals
        .iter()
        .max_by(|a, b| {
            state_priority(a.state)
                .cmp(&state_priority(b.state))
                .then_with(|| a.sequence.cmp(&b.sequence))
                .then_with(|| b.intent_message_id.cmp(&a.intent_message_id))
        })
        .map(|proposal| proposal.state)
        .unwrap_or(WorkTaskState::Proposed)
}

fn state_priority(state: WorkTaskState) -> u8 {
    match state {
        WorkTaskState::Completed => 6,
        WorkTaskState::Blocked => 5,
        WorkTaskState::Settled => 4,
        WorkTaskState::Accepted => 3,
        WorkTaskState::Yielded => 2,
        WorkTaskState::Proposed => 1,
        WorkTaskState::Rejected => 0,
    }
}

fn find_proposal_mut(
    tasks: &mut [WorkTaskRecord],
    task_id: &str,
    intent_message_id: &str,
) -> Option<(usize, usize)> {
    for (task_index, task) in tasks.iter_mut().enumerate() {
        if task.task_id != task_id {
            continue;
        }
        for (proposal_index, proposal) in task.proposals.iter_mut().enumerate() {
            if proposal.intent_message_id == intent_message_id {
                return Some((task_index, proposal_index));
            }
        }
    }
    None
}

fn any_proposal_with_intent(tasks: &[WorkTaskRecord], intent_message_id: &str) -> bool {
    tasks.iter().any(|task| {
        task.proposals
            .iter()
            .any(|p| p.intent_message_id == intent_message_id)
    })
}

fn dependency_creates_cycle(tasks: &[WorkTaskRecord], task_id: &str, deps: &[String]) -> bool {
    for dep in deps {
        if dep == task_id {
            continue;
        }
        let mut stack = vec![dep.clone()];
        let mut visited = HashSet::new();
        while let Some(current) = stack.pop() {
            if current == task_id {
                return true;
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            for task in tasks {
                if task.task_id != current {
                    continue;
                }
                for proposal in &task.proposals {
                    for next in &proposal.scope.dependencies {
                        stack.push(next.clone());
                    }
                }
            }
        }
    }
    false
}

fn authorized_coordinator(record: &WorkProposalRecord) -> String {
    record
        .coordinator
        .clone()
        .unwrap_or_else(|| record.agent.clone())
}

fn narrow_within_scope(scope: &WorkScope, paths: &[String], concerns: &[String]) -> bool {
    let path_covered = |entry: &str| {
        scope.paths.iter().any(|candidate| {
            if candidate == entry {
                return true;
            }
            let candidate_root = candidate.strip_suffix("/**").unwrap_or(candidate);
            if is_under_or_equal(entry, candidate_root) {
                return true;
            }
            entry
                .strip_suffix("/**")
                .is_some_and(|root| is_under_or_equal(candidate, root))
        })
    };
    paths.iter().all(|path| path_covered(path))
        && concerns
            .iter()
            .all(|concern| scope.concerns.iter().any(|c| c == concern))
}

fn is_under_or_equal(child: &str, dir: &str) -> bool {
    child == dir
        || child
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn overlap_claim_matches(claim: &WorkOverlapAcceptance, derived: &WorkOverlap) -> bool {
    claim.kind == derived.kind
        && claim.path_a == derived.path_a
        && claim.path_b == derived.path_b
        && claim.concern == derived.concern
}

fn apply_transition(state: &mut WorkStateFile, candidate: &WorkCandidate) -> ApplyOutcome {
    match &candidate.profile {
        WorkProfile::WorkIntent(profile) => apply_intent(state, candidate, profile),
        WorkProfile::WorkDecision(profile) => apply_decision(state, candidate, profile),
        WorkProfile::WorkAmendment(_) => {
            apply_author_transition(state, candidate, &candidate.profile)
        }
        WorkProfile::WorkYield(_) => apply_author_transition(state, candidate, &candidate.profile),
        WorkProfile::WorkSettled(_) => {
            apply_author_transition(state, candidate, &candidate.profile)
        }
        WorkProfile::WorkCompleted(_) => {
            apply_author_transition(state, candidate, &candidate.profile)
        }
        WorkProfile::WorkBlocked(_) => {
            apply_author_transition(state, candidate, &candidate.profile)
        }
        WorkProfile::WorkSuperseded(profile) => apply_superseded(state, candidate, profile),
    }
}

fn apply_intent(
    state: &mut WorkStateFile,
    candidate: &WorkCandidate,
    profile: &feanorfs_common::WorkIntentProfile,
) -> ApplyOutcome {
    // Duplicate / same-author fork rules against existing chains. Distinct
    // intents with different sequences are independent records: supersession
    // is resolved by admission (`superseded_by_newer`), never by drop order.
    let mut displaced: Vec<(usize, usize)> = Vec::new();
    let mut lost_fork = false;
    for (task_index, task) in state.tasks.iter_mut().enumerate() {
        if task.task_id != profile.task_id {
            continue;
        }
        for (proposal_index, proposal) in task.proposals.iter_mut().enumerate() {
            if proposal.agent != profile.agent {
                continue;
            }
            if proposal.intent_message_id == candidate.message_id {
                return ApplyOutcome::Applied; // idempotent duplicate
            }
            if proposal.sequence == profile.sequence {
                // Concurrent same-author fork: the smaller canonical message
                // id wins regardless of batch order; the loser is retained
                // as protocol evidence.
                if proposal.intent_message_id < candidate.message_id {
                    lost_fork = true;
                } else {
                    displaced.push((task_index, proposal_index));
                }
            }
        }
    }
    if lost_fork {
        return ApplyOutcome::Evidence(WorkRejectReason::SequenceDecreased);
    }
    for &(task_index, proposal_index) in displaced.iter().rev() {
        let record = state.tasks[task_index].proposals.remove(proposal_index);
        state.evidence.push(WorkEvidenceRecord {
            message_id: record.intent_message_id,
            task_id: state.tasks[task_index].task_id.clone(),
            disposition: WorkRejectReason::SequenceDecreased.as_str().to_string(),
            state: WorkTaskState::Proposed,
        });
    }

    // Causal dominance: the causal base must be an applied transition
    // (authenticated ancestry) or a proposal record in the closure. The
    // bounded observation cache (`seen`) never satisfies causal bases: a
    // dropped or evidenced base leaves dependents pending/evidenced.
    if let Some(base) = &profile.causal_base {
        let base_applied = state
            .applied
            .binary_search_by(|id| id.as_str().cmp(base))
            .is_ok()
            || any_proposal_with_intent(&state.tasks, base)
            || state.tasks.iter().any(|task| {
                task.proposals
                    .iter()
                    .any(|p| p.superseded_decisions.contains(base))
            });
        if !base_applied {
            return ApplyOutcome::Pending;
        }
    }

    // Dependencies: self-dependencies and cycles reject explicitly. Mutual
    // dependency cycles (applied proposal in a dependency task depends back
    // on this task) resolve deterministically: the intent with the smaller
    // canonical message id applies, the larger is evidenced — including when
    // the larger arrived first (its record is downgraded to the evidenced
    // state).
    if profile
        .dependencies
        .iter()
        .any(|dependency| dependency == &profile.task_id)
    {
        return ApplyOutcome::Evidence(WorkRejectReason::SelfDependency);
    }
    let mut mutual_larger: Vec<(usize, usize)> = Vec::new();
    let mut mutual_smaller = false;
    for dependency in &profile.dependencies {
        for (task_index, task) in state.tasks.iter().enumerate() {
            if task.task_id != *dependency {
                continue;
            }
            for (proposal_index, proposal) in task.proposals.iter().enumerate() {
                if proposal
                    .scope
                    .dependencies
                    .iter()
                    .any(|d| d == &profile.task_id)
                {
                    if proposal.intent_message_id < candidate.message_id {
                        mutual_smaller = true;
                    } else {
                        mutual_larger.push((task_index, proposal_index));
                    }
                }
            }
        }
    }
    if mutual_smaller {
        return ApplyOutcome::Evidence(WorkRejectReason::DependencyCycle);
    }
    for &(task_index, proposal_index) in mutual_larger.iter().rev() {
        let record = state.tasks[task_index].proposals.remove(proposal_index);
        state.evidence.push(WorkEvidenceRecord {
            message_id: record.intent_message_id,
            task_id: state.tasks[task_index].task_id.clone(),
            disposition: WorkRejectReason::DependencyCycle.as_str().to_string(),
            state: WorkTaskState::Proposed,
        });
    }
    if dependency_creates_cycle(&state.tasks, &profile.task_id, &profile.dependencies) {
        return ApplyOutcome::Evidence(WorkRejectReason::DependencyCycle);
    }

    // Active bound: refuse new tasks once the projection is full.
    let task_exists = state
        .tasks
        .iter()
        .any(|task| task.task_id == profile.task_id);
    if !task_exists && state.tasks.len() >= WORK_MAX_ACTIVE_TASKS + WORK_MAX_TERMINAL_TASKS {
        return ApplyOutcome::Dropped;
    }

    let scope = WorkScope {
        paths: profile.paths.clone(),
        concerns: profile.concerns.clone(),
        dependencies: profile.dependencies.clone(),
    };
    // Capabilities arrive validated (parse_work_profile enforces
    // ensure_sorted_unique + WORK_MAX_CAPABILITIES); store them verbatim.
    debug_assert!(
        profile.capabilities.len() <= WORK_MAX_CAPABILITIES
            && profile.capabilities.windows(2).all(|w| w[0] < w[1]),
        "intent capabilities must be sorted, unique, and bounded"
    );
    let record = WorkProposalRecord {
        agent: profile.agent.clone(),
        sequence: profile.sequence,
        intent_message_id: candidate.message_id.clone(),
        coordinator: profile.coordinator.clone(),
        causal_base: profile.causal_base.clone(),
        original_scope: scope.clone(),
        scope,
        capabilities: profile.capabilities.clone(),
        state: WorkTaskState::Proposed,
        decision: None,
        superseded_decisions: Vec::new(),
        amendments: Vec::new(),
        accepted_overlap: Vec::new(),
        verification: None,
        inspected_snapshot: None,
        outcome: None,
        reason: None,
        source_message_id: candidate.message_id.clone(),
        author_restore: None,
        updated_at_ms: candidate.created_at_ms,
    };
    if let Some((task_index, _)) = state
        .tasks
        .iter_mut()
        .enumerate()
        .find(|(_, task)| task.task_id == profile.task_id)
    {
        state.tasks[task_index].proposals.push(record);
        state.tasks[task_index].updated_at_ms = candidate.created_at_ms;
    } else {
        state.tasks.push(WorkTaskRecord {
            task_id: profile.task_id.clone(),
            proposals: vec![record],
            updated_at_ms: candidate.created_at_ms,
        });
    }
    ApplyOutcome::Applied
}

fn apply_decision(
    state: &mut WorkStateFile,
    candidate: &WorkCandidate,
    profile: &feanorfs_common::WorkDecisionProfile,
) -> ApplyOutcome {
    // The proposal must be applied in the closure (no acceptance inference
    // for decisions whose proposal is absent).
    let Some((task_index, proposal_index)) =
        find_proposal_by_intent_index(&mut state.tasks, &profile.proposal_message_id)
    else {
        return ApplyOutcome::Pending;
    };

    let authorized = {
        let record = &state.tasks[task_index].proposals[proposal_index];
        authorized_coordinator(record)
    };
    if candidate.from != authorized {
        return ApplyOutcome::Evidence(WorkRejectReason::UnauthorizedCoordinator);
    }

    let record_state = state.tasks[task_index].proposals[proposal_index].state;
    if record_state != WorkTaskState::Proposed {
        // Duplicate delivery of the applied decision is idempotent.
        let applied = state.tasks[task_index].proposals[proposal_index]
            .decision
            .clone();
        if applied
            .as_ref()
            .is_some_and(|applied| applied.message_id == candidate.message_id)
        {
            return ApplyOutcome::Applied;
        }
        // Concurrent decisions are never first-arrival-wins: the smaller
        // canonical message id wins deterministically. When the smaller
        // decision arrives after the larger was applied, unwind the applied
        // decision to its pre-decision snapshot, evict it to evidence, and
        // apply the smaller one through the normal path below.
        if let Some(applied) = applied {
            if candidate.message_id < applied.message_id
                && state.tasks[task_index].proposals[proposal_index].source_message_id
                    == applied.message_id
            {
                let restore = applied.restore.unwrap_or(WorkDecisionRestore {
                    scope: state.tasks[task_index].proposals[proposal_index]
                        .original_scope
                        .clone(),
                    accepted_overlap: Vec::new(),
                    reason: None,
                });
                state.evidence.push(WorkEvidenceRecord {
                    message_id: applied.message_id.clone(),
                    task_id: state.tasks[task_index].task_id.clone(),
                    disposition: WorkRejectReason::DecisionAlreadyApplied
                        .as_str()
                        .to_string(),
                    state: WorkTaskState::Proposed,
                });
                let record = &mut state.tasks[task_index].proposals[proposal_index];
                record.decision = None;
                record.state = WorkTaskState::Proposed;
                record.scope = restore.scope;
                record.accepted_overlap = restore.accepted_overlap;
                record.reason = restore.reason;
            } else {
                return ApplyOutcome::Evidence(WorkRejectReason::DecisionAlreadyApplied);
            }
        } else {
            return ApplyOutcome::Evidence(WorkRejectReason::DecisionAlreadyApplied);
        }
    }

    // Kind-specific validation (narrow containment, overlap derivability,
    // ordering reference).
    let narrow_scope: Option<(Vec<String>, Vec<String>)> = match &profile.kind {
        WorkDecisionKind::Narrow(inner) => {
            let record_scope = state.tasks[task_index].proposals[proposal_index]
                .scope
                .clone();
            if !narrow_within_scope(&record_scope, &inner.paths, &inner.concerns) {
                return ApplyOutcome::Evidence(WorkRejectReason::NarrowOutsideScope);
            }
            Some((inner.paths.clone(), inner.concerns.clone()))
        }
        _ => None,
    };
    match &profile.kind {
        WorkDecisionKind::AcceptOverlap(inner) => {
            // Every claimed overlap entry must be derivable from the pure
            // overlap evaluation against another applied proposal; entries
            // referencing a not-yet-applied scope stay pending.
            let proposal = &state.tasks[task_index].proposals[proposal_index];
            let mut all_claims_satisfiable = true;
            let mut any_scope_contains_entry = false;
            for claim in &inner.overlap {
                let mut found = false;
                for (other_task_index, other_task) in state.tasks.iter().enumerate() {
                    if other_task_index == task_index {
                        continue;
                    }
                    for other in &other_task.proposals {
                        let derived = evaluate_scope_overlap(
                            &proposal.scope.paths,
                            &proposal.scope.concerns,
                            &other.scope.paths,
                            &other.scope.concerns,
                        );
                        if derived
                            .iter()
                            .any(|entry| overlap_claim_matches(claim, entry))
                        {
                            found = true;
                            break;
                        }
                        if claim
                            .path_b
                            .as_deref()
                            .is_some_and(|entry| other.scope.paths.iter().any(|p| p == entry))
                        {
                            any_scope_contains_entry = true;
                        }
                    }
                    if found {
                        break;
                    }
                }
                if !found {
                    all_claims_satisfiable = false;
                    break;
                }
            }
            if !all_claims_satisfiable {
                if any_scope_contains_entry {
                    return ApplyOutcome::Evidence(WorkRejectReason::InvalidOverlapClaim);
                }
                return ApplyOutcome::Pending;
            }
        }
        WorkDecisionKind::Order(inner) => {
            if let Some(after) = &inner.after {
                if !any_proposal_with_intent(&state.tasks, after) {
                    return ApplyOutcome::Pending;
                }
            }
        }
        _ => {}
    }

    let ordered_after = match &profile.kind {
        WorkDecisionKind::Order(inner) => inner.after.clone(),
        _ => None,
    };
    let (target_state, scope_update, overlap_update, reason_update) = match &profile.kind {
        WorkDecisionKind::Accept(_) => (WorkTaskState::Accepted, None, None, None),
        WorkDecisionKind::Reject(inner) => (
            WorkTaskState::Rejected,
            None,
            None,
            Some(inner.reason.clone()),
        ),
        WorkDecisionKind::Narrow(_) => {
            let (paths, concerns) = narrow_scope.clone().expect("validated above");
            (WorkTaskState::Accepted, Some((paths, concerns)), None, None)
        }
        WorkDecisionKind::Order(_) => (WorkTaskState::Accepted, None, None, None),
        WorkDecisionKind::AcceptOverlap(inner) => (
            WorkTaskState::Accepted,
            None,
            Some(inner.overlap.clone()),
            None,
        ),
    };

    let record = &mut state.tasks[task_index].proposals[proposal_index];
    let restore = WorkDecisionRestore {
        scope: record.scope.clone(),
        accepted_overlap: record.accepted_overlap.clone(),
        reason: record.reason.clone(),
    };
    record.decision = Some(WorkDecisionRecord {
        message_id: candidate.message_id.clone(),
        coordinator: authorized,
        kind: profile.kind.clone(),
        ordered_after,
        restore: Some(restore),
    });
    record.state = target_state;
    if let Some((paths, concerns)) = scope_update {
        record.scope.paths = paths;
        record.scope.concerns = concerns;
    }
    if let Some(overlap) = overlap_update {
        record.accepted_overlap = overlap;
    }
    if let Some(reason) = reason_update {
        record.reason = Some(reason);
    }
    record.source_message_id = candidate.message_id.clone();
    record.updated_at_ms = candidate.created_at_ms;
    ApplyOutcome::Applied
}

fn apply_author_transition(
    state: &mut WorkStateFile,
    candidate: &WorkCandidate,
    profile: &WorkProfile,
) -> ApplyOutcome {
    let (task_id, intent_message_id, sequence) = match profile {
        WorkProfile::WorkAmendment(p) => (&p.task_id, &p.intent_message_id, p.sequence),
        WorkProfile::WorkYield(p) => (&p.task_id, &p.intent_message_id, p.sequence),
        WorkProfile::WorkSettled(p) => (&p.task_id, &p.intent_message_id, p.sequence),
        WorkProfile::WorkCompleted(p) => (&p.task_id, &p.intent_message_id, p.sequence),
        WorkProfile::WorkBlocked(p) => (&p.task_id, &p.intent_message_id, p.sequence),
        _ => return ApplyOutcome::Evidence(WorkRejectReason::MissingIntent),
    };
    let Some((task_index, proposal_index)) =
        find_proposal_mut(&mut state.tasks, task_id, intent_message_id)
    else {
        // The intent may arrive in a later observe; hold rather than reject.
        return ApplyOutcome::Pending;
    };

    {
        let record = &state.tasks[task_index].proposals[proposal_index];
        if candidate.from != record.agent {
            return ApplyOutcome::Evidence(WorkRejectReason::NotAuthor);
        }
        if sequence < record.sequence {
            return ApplyOutcome::Evidence(WorkRejectReason::SequenceDecreased);
        }
        if sequence == record.sequence {
            if record.source_message_id == candidate.message_id {
                return ApplyOutcome::Applied; // idempotent duplicate
            }
            // Concurrent same-author fork at the same sequence: the smaller
            // canonical message id applies deterministically, the larger is
            // evidenced. When the larger arrived first, unwind it via its
            // restore snapshot so the smaller applies below; the displaced
            // (larger) transition is evicted to protocol evidence.
            let displaced: Option<(WorkAuthorRestore, String)> =
                if candidate.message_id < record.source_message_id {
                    record.author_restore.as_ref().and_then(|restore| {
                        (restore.source_message_id == record.source_message_id)
                            .then(|| (restore.clone(), record.source_message_id.clone()))
                    })
                } else {
                    None
                };
            if let Some((restore, applied_source)) = displaced {
                state.evidence.push(WorkEvidenceRecord {
                    message_id: applied_source,
                    task_id: task_id.to_string(),
                    disposition: WorkRejectReason::SequenceDecreased.as_str().to_string(),
                    state: WorkTaskState::Proposed,
                });
                let record = &mut state.tasks[task_index].proposals[proposal_index];
                record.sequence = restore.sequence;
                record.state = restore.state;
                record.scope = restore.scope;
                record.reason = restore.reason;
                record.verification = restore.verification;
                record.inspected_snapshot = restore.inspected_snapshot;
                record.outcome = restore.outcome;
                record.amendments.truncate(restore.amendments_len);
                record.author_restore = None;
            } else {
                return ApplyOutcome::Evidence(WorkRejectReason::SequenceDecreased);
            }
        }
    }

    let record_state = state.tasks[task_index].proposals[proposal_index].state;
    if let Some(reason) = feanorfs_common::transition_rejection(Some(record_state), profile) {
        if premature_for_state(record_state, profile) {
            return ApplyOutcome::Pending;
        }
        return ApplyOutcome::Evidence(reason);
    }

    let record = &mut state.tasks[task_index].proposals[proposal_index];
    // Amendment expansion gate: an amended scope not contained in the
    // original declared scope requires the currently applied coordinator
    // decision's message id as approval; otherwise the amendment is
    // evidenced and the record is left unchanged.
    if let WorkProfile::WorkAmendment(p) = profile {
        let amended_paths: Vec<String> = p
            .paths
            .clone()
            .unwrap_or_else(|| record.scope.paths.clone());
        let amended_concerns: Vec<String> = p
            .concerns
            .clone()
            .unwrap_or_else(|| record.scope.concerns.clone());
        let amended_dependencies: Vec<String> = p
            .dependencies
            .clone()
            .unwrap_or_else(|| record.scope.dependencies.clone());
        let within_original = amended_paths.iter().all(|path| {
            feanorfs_common::work_contract::scope_covers_path(&record.original_scope, path)
        }) && amended_concerns
            .iter()
            .all(|concern| record.original_scope.concerns.contains(concern))
            && amended_dependencies
                .iter()
                .all(|dependency| record.original_scope.dependencies.contains(dependency));
        if !within_original {
            let approved = p.approval_decision_id.as_deref()
                == record
                    .decision
                    .as_ref()
                    .map(|decision| decision.message_id.as_str());
            if !approved {
                return ApplyOutcome::Evidence(
                    WorkRejectReason::AmendmentExpandsScopeWithoutApproval,
                );
            }
        }
    }
    let restore = WorkAuthorRestore {
        source_message_id: candidate.message_id.clone(),
        sequence: record.sequence,
        state: record.state,
        scope: record.scope.clone(),
        reason: record.reason.clone(),
        verification: record.verification.clone(),
        inspected_snapshot: record.inspected_snapshot.clone(),
        outcome: record.outcome.clone(),
        amendments_len: record.amendments.len(),
    };
    record.sequence = sequence;
    record.source_message_id = candidate.message_id.clone();
    record.author_restore = Some(restore);
    record.updated_at_ms = candidate.created_at_ms;
    match profile {
        WorkProfile::WorkAmendment(p) => {
            if let Some(paths) = &p.paths {
                record.scope.paths = paths.clone();
            }
            if let Some(concerns) = &p.concerns {
                record.scope.concerns = concerns.clone();
            }
            if let Some(dependencies) = &p.dependencies {
                record.scope.dependencies = dependencies.clone();
            }
            record.amendments.push(WorkAmendmentRecord {
                message_id: candidate.message_id.clone(),
                sequence: p.sequence,
                reason: p.reason.clone(),
                approval_decision_id: p.approval_decision_id.clone(),
            });
        }
        WorkProfile::WorkYield(p) => {
            record.state = WorkTaskState::Yielded;
            record.reason = p.reason.clone();
        }
        WorkProfile::WorkSettled(p) => {
            record.state = WorkTaskState::Settled;
            record.verification = Some(p.verification.clone());
            record.inspected_snapshot = Some(p.inspected_snapshot.clone());
        }
        WorkProfile::WorkCompleted(p) => {
            record.state = WorkTaskState::Completed;
            record.outcome = Some(p.outcome.clone());
        }
        WorkProfile::WorkBlocked(p) => {
            record.state = WorkTaskState::Blocked;
            record.reason = Some(p.reason.clone());
        }
        _ => unreachable!("author transitions are closed above"),
    }
    ApplyOutcome::Applied
}

/// Whether `current` can still reach the state the transition requires; when
/// true the transition is premature (pending) rather than definitively
/// invalid, keeping incremental observes convergent with full closures.
fn premature_for_state(current: WorkTaskState, profile: &WorkProfile) -> bool {
    match profile {
        WorkProfile::WorkAmendment(_) | WorkProfile::WorkYield(_) => {
            current == WorkTaskState::Proposed
        }
        WorkProfile::WorkSettled(_) => current == WorkTaskState::Proposed,
        WorkProfile::WorkCompleted(_) => matches!(
            current,
            WorkTaskState::Proposed | WorkTaskState::Accepted | WorkTaskState::Yielded
        ),
        WorkProfile::WorkBlocked(_) => current == WorkTaskState::Proposed,
        _ => false,
    }
}

fn apply_superseded(
    state: &mut WorkStateFile,
    candidate: &WorkCandidate,
    profile: &feanorfs_common::WorkSupersededProfile,
) -> ApplyOutcome {
    let Some((task_index, proposal_index)) = find_proposal_mut(
        &mut state.tasks,
        &profile.task_id,
        &profile.proposal_message_id,
    ) else {
        return ApplyOutcome::Pending;
    };

    let authorized = {
        let record = &state.tasks[task_index].proposals[proposal_index];
        authorized_coordinator(record)
    };
    if candidate.from != authorized {
        return ApplyOutcome::Evidence(WorkRejectReason::UnauthorizedCoordinator);
    }

    let record_state = state.tasks[task_index].proposals[proposal_index].state;
    if !matches!(
        record_state,
        WorkTaskState::Accepted | WorkTaskState::Rejected
    ) {
        return ApplyOutcome::Evidence(WorkRejectReason::SupersededDecisionUnknown);
    }

    let record = &mut state.tasks[task_index].proposals[proposal_index];
    if let Some(applied) = &record.decision {
        if applied.message_id != profile.superseded_decision_message_id {
            return ApplyOutcome::Evidence(WorkRejectReason::SupersededDecisionUnknown);
        }
    } else if record
        .superseded_decisions
        .contains(&profile.superseded_decision_message_id)
    {
        return ApplyOutcome::Applied; // idempotent duplicate supersede
    } else {
        // The referenced decision has not been applied (yet); hold.
        return ApplyOutcome::Pending;
    }

    record
        .superseded_decisions
        .push(profile.superseded_decision_message_id.clone());
    record.decision = None;
    record.state = WorkTaskState::Proposed;
    record.source_message_id = candidate.message_id.clone();
    record.reason = profile.reason.clone();
    record.updated_at_ms = candidate.created_at_ms;
    ApplyOutcome::Applied
}

fn find_proposal_by_intent_index(
    tasks: &mut [WorkTaskRecord],
    intent_message_id: &str,
) -> Option<(usize, usize)> {
    for (task_index, task) in tasks.iter_mut().enumerate() {
        for (proposal_index, proposal) in task.proposals.iter_mut().enumerate() {
            if proposal.intent_message_id == intent_message_id {
                return Some((task_index, proposal_index));
            }
        }
    }
    None
}

fn status_result(
    state: &WorkStateFile,
    messages_processed: usize,
    cursor_reset: bool,
) -> WorkStatusResult {
    let mut tasks: Vec<WorkTaskStatus> = state
        .tasks
        .iter()
        .map(|task| {
            let mut proposals: Vec<WorkProposalStatus> =
                task.proposals
                    .iter()
                    .map(|proposal| {
                        let mut causal_refs: Vec<String> = Vec::new();
                        if let Some(base) = &proposal.causal_base {
                            causal_refs.push(base.clone());
                        }
                        if let Some(ordered_after) =
                            proposal.decision.as_ref().and_then(|d| match &d.kind {
                                WorkDecisionKind::Order(inner) => inner.after.clone(),
                                _ => None,
                            })
                        {
                            causal_refs.push(ordered_after);
                        }
                        causal_refs.extend(proposal.superseded_decisions.clone());
                        // Approval refs of applied amendments are part of the
                        // record's referenced transitions: expose them so
                        // admission can verify expansion approvals.
                        causal_refs.extend(
                            proposal
                                .amendments
                                .iter()
                                .filter_map(|amendment| amendment.approval_decision_id.clone()),
                        );
                        WorkProposalStatus {
                            agent: proposal.agent.clone(),
                            state: proposal.state,
                            sequence: proposal.sequence,
                            intent_message_id: proposal.intent_message_id.clone(),
                            coordinator: proposal.coordinator.clone(),
                            accepted_scope: proposal.scope.clone(),
                            capabilities: proposal.capabilities.clone(),
                            decision: proposal.decision.as_ref().map(|decision| {
                                WorkDecisionStatus {
                                    message_id: decision.message_id.clone(),
                                    coordinator: decision.coordinator.clone(),
                                    kind: decision.kind.clone(),
                                    ordered_after: decision.ordered_after.clone(),
                                }
                            }),
                            accepted_overlap: proposal.accepted_overlap.clone(),
                            amendments: proposal
                                .amendments
                                .iter()
                                .map(|amendment| WorkAmendmentStatus {
                                    message_id: amendment.message_id.clone(),
                                    sequence: amendment.sequence,
                                    reason: amendment.reason.clone(),
                                })
                                .collect(),
                            causal_refs,
                            inspected_snapshot: proposal.inspected_snapshot.clone(),
                            verification: proposal.verification.clone(),
                            outcome: proposal.outcome.clone(),
                            reason: proposal.reason.clone(),
                            source_message_id: proposal.source_message_id.clone(),
                            updated_at_ms: proposal.updated_at_ms,
                        }
                    })
                    .collect();
            proposals.sort_by(|a, b| {
                a.sequence
                    .cmp(&b.sequence)
                    .then_with(|| a.intent_message_id.cmp(&b.intent_message_id))
            });
            WorkTaskStatus {
                task_id: task.task_id.clone(),
                state: task_state(task),
                proposals,
            }
        })
        .collect();
    tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));

    WorkStatusResult {
        cursor: state.cursor.clone().unwrap_or_default(),
        cursor_reset,
        projection_incomplete: state.incomplete,
        messages_processed,
        tasks,
        evidence_count: state.evidence.len(),
        dropped_count: state.dropped_count,
        updated_at_ms: state.updated_at_ms,
        applied_message_ids: state.applied.clone(),
    }
}

fn send_result_from(
    profile: &WorkProfile,
    send: AgentSendResult,
    agent: &str,
    applied_message_ids: Vec<String>,
) -> WorkSendResult {
    let (task_id, scope, causal_refs, overlap, state) = match profile {
        WorkProfile::WorkIntent(p) => {
            let scope = WorkScope {
                paths: p.paths.clone(),
                concerns: p.concerns.clone(),
                dependencies: p.dependencies.clone(),
            };
            (
                p.task_id.clone(),
                scope,
                p.causal_base.clone().into_iter().collect(),
                Vec::new(),
                WorkTaskState::Proposed,
            )
        }
        WorkProfile::WorkDecision(p) => {
            let (state, overlap, extra_refs) = match &p.kind {
                WorkDecisionKind::Accept(_) => (WorkTaskState::Accepted, Vec::new(), Vec::new()),
                WorkDecisionKind::Reject(_) => (WorkTaskState::Rejected, Vec::new(), Vec::new()),
                WorkDecisionKind::Narrow(_) => (WorkTaskState::Accepted, Vec::new(), Vec::new()),
                WorkDecisionKind::Order(inner) => (
                    WorkTaskState::Accepted,
                    Vec::new(),
                    inner.after.clone().into_iter().collect(),
                ),
                WorkDecisionKind::AcceptOverlap(inner) => {
                    (WorkTaskState::Accepted, inner.overlap.clone(), Vec::new())
                }
            };
            let mut refs = vec![p.proposal_message_id.clone()];
            refs.extend(extra_refs);
            (String::new(), WorkScope::default(), refs, overlap, state)
        }
        WorkProfile::WorkAmendment(p) => {
            let scope = WorkScope {
                paths: p.paths.clone().unwrap_or_default(),
                concerns: p.concerns.clone().unwrap_or_default(),
                dependencies: p.dependencies.clone().unwrap_or_default(),
            };
            (
                p.task_id.clone(),
                scope,
                vec![p.intent_message_id.clone()],
                Vec::new(),
                WorkTaskState::Accepted,
            )
        }
        WorkProfile::WorkYield(p) => (
            p.task_id.clone(),
            WorkScope::default(),
            vec![p.intent_message_id.clone()],
            Vec::new(),
            WorkTaskState::Yielded,
        ),
        WorkProfile::WorkSettled(p) => (
            p.task_id.clone(),
            WorkScope::default(),
            vec![p.intent_message_id.clone()],
            Vec::new(),
            WorkTaskState::Settled,
        ),
        WorkProfile::WorkCompleted(p) => (
            p.task_id.clone(),
            WorkScope::default(),
            vec![p.intent_message_id.clone()],
            Vec::new(),
            WorkTaskState::Completed,
        ),
        WorkProfile::WorkBlocked(p) => (
            p.task_id.clone(),
            WorkScope::default(),
            vec![p.intent_message_id.clone()],
            Vec::new(),
            WorkTaskState::Blocked,
        ),
        WorkProfile::WorkSuperseded(p) => (
            p.task_id.clone(),
            WorkScope::default(),
            vec![
                p.proposal_message_id.clone(),
                p.superseded_decision_message_id.clone(),
            ],
            Vec::new(),
            WorkTaskState::Proposed,
        ),
    };
    WorkSendResult {
        message_id: send.message_id,
        about_snapshot: send.about_snapshot,
        task_id,
        agent: agent.to_string(),
        profile: profile.type_name().to_string(),
        state,
        scope,
        causal_refs,
        overlap,
        projection_incomplete: false,
        applied_message_ids,
    }
}

async fn publish_profile(
    ctx: &SyncCtx<'_>,
    profile: &WorkProfile,
    kind: AgentMessageKind,
    to: Option<&str>,
    about_snapshot: Option<String>,
    from: Option<&str>,
) -> Result<WorkSendResult> {
    let body = encode_work_profile(profile)?;
    let sender = from
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "human".to_string());
    let send = send_message(
        ctx,
        AgentMessageInput {
            to: to.unwrap_or("*").to_string(),
            kind,
            body,
            about_snapshot,
            reply_to: None,
            from: Some(sender.clone()),
        },
    )
    .await?;
    // Authenticated applied ancestry at send time: admission and causal-base
    // checks read this set, never the bounded observation cache.
    let applied_message_ids = WorkStore::open(ctx.base)
        .ok()
        .and_then(|store| store.load().ok())
        .map(|state| state.applied.clone())
        .unwrap_or_default();
    Ok(send_result_from(
        profile,
        send,
        &sender,
        applied_message_ids,
    ))
}

fn resolve_sender(explicit: Option<&str>, fallback: &str) -> String {
    explicit
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Proposes one work intent. Sends an ordinary `ffmsg1` `request` signal
/// carrying the `ffwork1` intent profile; the local projection only changes
/// when the reducer observes the signal.
///
/// # Errors
/// Returns an error for invalid input, unbounded profiles, or failed signal
/// publication.
pub async fn work_propose(ctx: &SyncCtx<'_>, input: WorkProposeInput) -> Result<WorkSendResult> {
    let sender = resolve_sender(input.agent.as_deref(), "human");
    let profile = WorkProfile::WorkIntent(feanorfs_common::WorkIntentProfile {
        task_id: input.task_id.clone(),
        agent: sender.clone(),
        sequence: input.sequence,
        causal_base: input.causal_base,
        coordinator: input.coordinator.clone(),
        paths: input.paths,
        concerns: input.concerns,
        dependencies: input.dependencies,
        capabilities: input.capabilities,
    });
    let to = input
        .to
        .or_else(|| input.coordinator.clone())
        .unwrap_or_else(|| "*".to_string());
    publish_profile(
        ctx,
        &profile,
        AgentMessageKind::Request,
        Some(&to),
        input.about_snapshot,
        Some(&sender),
    )
    .await
}

/// Sends one coordinator decision for an exact proposal.
///
/// # Errors
/// Returns an error for invalid input or failed signal publication.
pub async fn work_decide(ctx: &SyncCtx<'_>, input: WorkDecideInput) -> Result<WorkSendResult> {
    let sender = resolve_sender(input.from.as_deref(), "human");
    let profile = WorkProfile::WorkDecision(feanorfs_common::WorkDecisionProfile {
        proposal_message_id: input.proposal_message_id,
        kind: input.kind,
    });
    publish_profile(
        ctx,
        &profile,
        AgentMessageKind::Status,
        input.to.as_deref(),
        input.about_snapshot,
        Some(&sender),
    )
    .await
}

/// Sends one scope amendment against an accepted intent.
///
/// # Errors
/// Returns an error for invalid input or failed signal publication.
pub async fn work_amend(ctx: &SyncCtx<'_>, input: WorkAmendInput) -> Result<WorkSendResult> {
    let sender = resolve_sender(input.from.as_deref(), "human");
    let profile = WorkProfile::WorkAmendment(feanorfs_common::WorkAmendmentProfile {
        task_id: input.task_id,
        intent_message_id: input.intent_message_id,
        sequence: input.sequence,
        paths: input.paths,
        concerns: input.concerns,
        dependencies: input.dependencies,
        // The sender-side input carries no approval reference: expansion
        // amendments are authorized by the runner building the profile
        // directly with the applied decision id. The reducer enforces the
        // approval gate regardless.
        approval_decision_id: None,
        reason: input.reason,
    });
    publish_profile(
        ctx,
        &profile,
        AgentMessageKind::Request,
        input.to.as_deref(),
        input.about_snapshot,
        Some(&sender),
    )
    .await
}

/// Sends one explicit yield relinquishing accepted overlap.
///
/// # Errors
/// Returns an error for invalid input or failed signal publication.
pub async fn work_yield(ctx: &SyncCtx<'_>, input: WorkYieldInput) -> Result<WorkSendResult> {
    let sender = resolve_sender(input.from.as_deref(), "human");
    let profile = WorkProfile::WorkYield(feanorfs_common::WorkYieldProfile {
        task_id: input.task_id,
        intent_message_id: input.intent_message_id,
        sequence: input.sequence,
        reason: input.reason,
    });
    publish_profile(
        ctx,
        &profile,
        AgentMessageKind::Status,
        input.to.as_deref(),
        input.about_snapshot,
        Some(&sender),
    )
    .await
}

/// Sends one settled profile with verification evidence.
///
/// # Errors
/// Returns an error for invalid input or failed signal publication.
pub async fn work_settle(ctx: &SyncCtx<'_>, input: WorkSettleInput) -> Result<WorkSendResult> {
    let sender = resolve_sender(input.from.as_deref(), "human");
    let profile = WorkProfile::WorkSettled(feanorfs_common::WorkSettledProfile {
        task_id: input.task_id,
        intent_message_id: input.intent_message_id,
        sequence: input.sequence,
        inspected_snapshot: input.inspected_snapshot,
        verification: input.verification,
    });
    publish_profile(
        ctx,
        &profile,
        AgentMessageKind::Status,
        input.to.as_deref(),
        input.about_snapshot,
        Some(&sender),
    )
    .await
}

/// Sends one terminal completion.
///
/// # Errors
/// Returns an error for invalid input or failed signal publication.
pub async fn work_complete(ctx: &SyncCtx<'_>, input: WorkCompleteInput) -> Result<WorkSendResult> {
    let sender = resolve_sender(input.from.as_deref(), "human");
    let profile = WorkProfile::WorkCompleted(feanorfs_common::WorkCompletedProfile {
        task_id: input.task_id,
        intent_message_id: input.intent_message_id,
        sequence: input.sequence,
        outcome: input.outcome,
    });
    publish_profile(
        ctx,
        &profile,
        AgentMessageKind::Result,
        input.to.as_deref(),
        input.about_snapshot,
        Some(&sender),
    )
    .await
}

/// Sends one terminal blocker.
///
/// # Errors
/// Returns an error for invalid input or failed signal publication.
pub async fn work_block(ctx: &SyncCtx<'_>, input: WorkBlockInput) -> Result<WorkSendResult> {
    let sender = resolve_sender(input.from.as_deref(), "human");
    let profile = WorkProfile::WorkBlocked(feanorfs_common::WorkBlockedProfile {
        task_id: input.task_id,
        intent_message_id: input.intent_message_id,
        sequence: input.sequence,
        reason: input.reason,
    });
    publish_profile(
        ctx,
        &profile,
        AgentMessageKind::Blocked,
        input.to.as_deref(),
        input.about_snapshot,
        Some(&sender),
    )
    .await
}

/// Observes new signals since the persisted cursor through the reducer and
/// reports the bounded projection. A deliberate `rebuild` resets the
/// projection to a fresh state (cursor None), re-observes the bounded window
/// from the beginning, and clears `incomplete` when the scan covered the
/// closure without bound exhaustion — the only production path that clears
/// it. Store-driven cursor resets during normal status stay sticky-
/// incomplete (no acceptance inference).
///
/// # Errors
/// Returns an error for corrupt state or unreadable history.
pub async fn work_status(ctx: &SyncCtx<'_>, input: WorkStatusInput) -> Result<WorkStatusResult> {
    let store = WorkStore::open(ctx.base)?;
    let mut state = store.load()?;
    let rebuild_requested = input.rebuild;
    if rebuild_requested {
        // Fresh projection from an empty cursor: the full bounded window is
        // re-observed from the beginning.
        state = WorkStateFile::fresh();
    }
    let signals = signals_since(ctx, state.cursor.as_deref(), WORK_OBSERVE_LIMIT).await?;
    let rebuild = rebuild_requested || signals.cursor_reset;
    let messages_processed = apply_batch(&mut state, &signals.messages, rebuild);
    state.cursor = Some(signals.cursor);
    // A deliberate rebuild that covered the closure without bound exhaustion
    // leaves `incomplete` false (apply_batch re-marks it on drops/evictions;
    // a truncated window re-marks it via cursor_reset). Normal status keeps
    // the sticky-incomplete flag across store-driven resets.
    state.incomplete = state.incomplete || signals.cursor_reset;
    state.updated_at_ms = now_ms();
    store.update(|persisted| {
        *persisted = state.clone();
        Ok(())
    })?;
    Ok(status_result(
        &state,
        messages_processed,
        signals.cursor_reset,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{
        WorkDecisionAccept, WorkDecisionNarrow, WorkDecisionReject, WorkVerification,
        WorkVerificationStatus,
    };

    fn hex64(byte: u8) -> String {
        std::iter::repeat_n(byte as char, 64).collect()
    }

    fn message(message_id: char, from: &str, profile: &WorkProfile) -> AgentMessage {
        AgentMessage {
            message_id: hex64(message_id as u8),
            from: from.to_string(),
            to: "human".to_string(),
            kind: AgentMessageKind::Status,
            body: encode_work_profile(profile).unwrap(),
            about_snapshot: hex64(b'b'),
            reply_to: None,
            created_at_ms: 1_785_852_000_000,
        }
    }

    fn intent(task_id: &str, agent: &str, sequence: u64, coordinator: Option<&str>) -> WorkProfile {
        WorkProfile::WorkIntent(feanorfs_common::WorkIntentProfile {
            task_id: task_id.to_string(),
            agent: agent.to_string(),
            sequence,
            causal_base: None,
            coordinator: coordinator.map(str::to_string),
            paths: vec![format!("src/{task_id}.rs"), format!("tests/{task_id}.rs")],
            concerns: vec![format!("{task_id} behavior")],
            dependencies: vec![],
            capabilities: vec!["rust".to_string()],
        })
    }

    fn accept(proposal_id: char) -> WorkProfile {
        WorkProfile::WorkDecision(feanorfs_common::WorkDecisionProfile {
            proposal_message_id: hex64(proposal_id as u8),
            kind: WorkDecisionKind::Accept(WorkDecisionAccept { reason: None }),
        })
    }

    fn reject(proposal_id: char) -> WorkProfile {
        WorkProfile::WorkDecision(feanorfs_common::WorkDecisionProfile {
            proposal_message_id: hex64(proposal_id as u8),
            kind: WorkDecisionKind::Reject(WorkDecisionReject {
                reason: "scope too broad".to_string(),
            }),
        })
    }

    fn narrow(proposal_id: char, path: &str) -> WorkProfile {
        WorkProfile::WorkDecision(feanorfs_common::WorkDecisionProfile {
            proposal_message_id: hex64(proposal_id as u8),
            kind: WorkDecisionKind::Narrow(WorkDecisionNarrow {
                paths: vec![path.to_string()],
                concerns: vec![],
                reason: None,
            }),
        })
    }

    fn settled(task_id: &str, intent_id: char, sequence: u64) -> WorkProfile {
        WorkProfile::WorkSettled(feanorfs_common::WorkSettledProfile {
            task_id: task_id.to_string(),
            intent_message_id: hex64(intent_id as u8),
            sequence,
            inspected_snapshot: hex64(b'd'),
            verification: WorkVerification {
                status: WorkVerificationStatus::Passed,
                summary: "84 tests passed".to_string(),
            },
        })
    }

    fn completed(task_id: &str, intent_id: char, sequence: u64) -> WorkProfile {
        WorkProfile::WorkCompleted(feanorfs_common::WorkCompletedProfile {
            task_id: task_id.to_string(),
            intent_message_id: hex64(intent_id as u8),
            sequence,
            outcome: "Done.".to_string(),
        })
    }

    fn apply_all(profiles: &[(&str, &str, WorkProfile)]) -> WorkStateFile {
        let mut state = WorkStateFile::fresh();
        let messages: Vec<AgentMessage> = profiles
            .iter()
            .map(|(id, from, profile)| message(id.chars().next().unwrap(), from, profile))
            .collect();
        apply_batch(&mut state, &messages, false);
        state
    }

    fn find_proposal<'a>(state: &'a WorkStateFile, task_id: &str) -> &'a WorkProposalRecord {
        state
            .tasks
            .iter()
            .find(|task| task.task_id == task_id)
            .unwrap_or_else(|| panic!("task {task_id} missing"))
            .proposals
            .iter()
            .max_by(|a, b| a.sequence.cmp(&b.sequence))
            .expect("proposal present")
    }

    #[test]
    fn full_lifecycle_propose_accept_settle_complete() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", settled("parser-impl", 'a', 3)),
            ("f", "linux-dev", completed("parser-impl", 'a', 4)),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Completed);
        assert_eq!(proposal.sequence, 4);
        assert_eq!(proposal.decision.as_ref().unwrap().message_id, hex64(b'c'));
        assert_eq!(task_state(&state.tasks[0]), WorkTaskState::Completed);
        assert!(state.evidence.is_empty());
    }

    #[test]
    fn delivery_permutations_converge() {
        let intent_a = (
            "a",
            "linux-dev",
            intent("parser-impl", "linux-dev", 1, Some("human")),
        );
        let accept_c = ("c", "human", accept('a'));
        let settle_e = ("e", "linux-dev", settled("parser-impl", 'a', 3));
        let complete_f = ("f", "linux-dev", completed("parser-impl", 'a', 4));
        let permutations: Vec<Vec<(&str, &str, WorkProfile)>> = vec![
            vec![
                intent_a.clone(),
                accept_c.clone(),
                settle_e.clone(),
                complete_f.clone(),
            ],
            vec![
                accept_c.clone(),
                complete_f.clone(),
                intent_a.clone(),
                settle_e.clone(),
            ],
            vec![
                settle_e.clone(),
                intent_a.clone(),
                complete_f.clone(),
                accept_c.clone(),
            ],
            vec![
                complete_f.clone(),
                settle_e.clone(),
                accept_c.clone(),
                intent_a.clone(),
            ],
        ];
        for (index, permutation) in permutations.into_iter().enumerate() {
            let state = apply_all(&permutation);
            let proposal = find_proposal(&state, "parser-impl");
            assert_eq!(
                proposal.state,
                WorkTaskState::Completed,
                "permutation {index} must converge"
            );
            assert_eq!(
                proposal.decision.as_ref().map(|d| d.message_id.clone()),
                Some(hex64(b'c'))
            );
        }
    }

    #[test]
    fn duplicate_delivery_is_idempotent() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("c", "human", accept('a')),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(state.tasks[0].proposals.len(), 1);
        assert_eq!(state.evidence.len(), 0);
    }

    #[test]
    fn unknown_and_malformed_profiles_only_advance_observation_bookkeeping() {
        let mut state = apply_all(&[(
            "a",
            "linux-dev",
            intent("parser-impl", "linux-dev", 1, Some("human")),
        )]);
        let typed_before = serde_json::to_value((
            &state.tasks,
            &state.evidence,
            &state.applied,
            &state.pending,
            state.incomplete,
            state.dropped_count,
        ))
        .unwrap();

        let messages = [
            AgentMessage {
                message_id: hex64(b'c'),
                from: "future-peer".to_string(),
                to: "human".to_string(),
                kind: AgentMessageKind::Status,
                body: r#"ffwork2:{"type":"work_intent"}"#.to_string(),
                about_snapshot: hex64(b'b'),
                reply_to: None,
                created_at_ms: 1_785_852_000_001,
            },
            AgentMessage {
                message_id: hex64(b'd'),
                from: "malformed-peer".to_string(),
                to: "human".to_string(),
                kind: AgentMessageKind::Status,
                body: r#"ffwork1:{"type":"work_intent"}"#.to_string(),
                about_snapshot: hex64(b'b'),
                reply_to: None,
                created_at_ms: 1_785_852_000_002,
            },
        ];

        assert_eq!(apply_batch(&mut state, &messages, false), 0);
        let typed_after = serde_json::to_value((
            &state.tasks,
            &state.evidence,
            &state.applied,
            &state.pending,
            state.incomplete,
            state.dropped_count,
        ))
        .unwrap();
        assert_eq!(typed_after, typed_before);
        assert!(state.seen.contains(&hex64(b'c')));
        assert!(state.seen.contains(&hex64(b'd')));
    }

    #[test]
    fn unauthorized_coordinator_decision_does_not_mutate_state() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("dispatcher-1")),
            ),
            ("c", "human", accept('a')),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Proposed);
        assert!(proposal.decision.is_none());
        assert_eq!(state.evidence.len(), 1);
        assert_eq!(state.evidence[0].disposition, "unauthorized_coordinator");
    }

    #[test]
    fn decision_without_proposal_is_pending_not_applied() {
        let state = apply_all(&[("c", "human", accept('a'))]);
        assert!(state.tasks.is_empty());
        assert_eq!(state.pending.len(), 1);
        assert!(state.evidence.is_empty());
    }

    #[test]
    fn self_dependency_and_cycles_reject_explicitly() {
        let mut self_dep = intent("task-a", "agent-a", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut self_dep else {
            unreachable!()
        };
        inner.dependencies = vec!["task-a".to_string()];
        let state = apply_all(&[("a", "agent-a", self_dep)]);
        assert!(state.tasks.is_empty());
        assert_eq!(state.evidence[0].disposition, "self_dependency");

        let a = intent("task-a", "agent-a", 1, None);
        let mut b = intent("task-b", "agent-b", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut b else {
            unreachable!()
        };
        inner.dependencies = vec!["task-a".to_string()];
        let mut a2 = a.clone();
        let WorkProfile::WorkIntent(inner) = &mut a2 else {
            unreachable!()
        };
        inner.sequence = 2;
        inner.dependencies = vec!["task-b".to_string()];
        let state = apply_all(&[
            ("a", "agent-a", a),
            ("b", "agent-b", b),
            ("c", "agent-a", a2),
        ]);
        // a (no deps) applies; b depends on a applies; a2 depends on b would cycle.
        assert_eq!(state.tasks.len(), 2);
        let cycle_evidence = state
            .evidence
            .iter()
            .find(|e| e.disposition == "dependency_cycle");
        assert!(
            cycle_evidence.is_some(),
            "cycle must be recorded as evidence"
        );
        assert_eq!(cycle_evidence.unwrap().message_id, hex64(b'c'));
    }

    #[test]
    fn reject_does_not_accept_and_sequence_cannot_decrease() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", reject('a')),
            ("d", "linux-dev", settled("parser-impl", 'a', 2)),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Rejected);
        // The settle from a rejected proposal is definitively wrong-state.
        assert!(state
            .evidence
            .iter()
            .any(|e| e.disposition == "wrong_state"));
    }

    #[test]
    fn amendment_applies_after_acceptance_and_is_visible_in_status() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkAmendment(feanorfs_common::WorkAmendmentProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    // src/parser/ast.rs is not covered by the original
                    // scope; the expansion is authorized by the applied
                    // decision message id.
                    paths: Some(vec!["src/parser/ast.rs".to_string()]),
                    concerns: None,
                    dependencies: None,
                    approval_decision_id: Some(hex64(b'c')),
                    reason: Some("split module".to_string()),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(proposal.scope.paths, vec!["src/parser/ast.rs".to_string()]);
        assert_eq!(proposal.amendments.len(), 1);
        let result = status_result(&state, 0, false);
        assert_eq!(result.tasks[0].proposals[0].amendments.len(), 1);
    }

    #[test]
    fn narrow_decision_reduces_scope_and_out_of_scope_narrow_is_evidence() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", narrow('a', "src/parser-impl.rs")),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(proposal.scope.paths, vec!["src/parser-impl.rs".to_string()]);

        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", narrow('a', "outside/scope.rs")),
        ]);
        assert!(find_proposal(&state, "parser-impl").decision.is_none());
        assert!(state
            .evidence
            .iter()
            .any(|e| e.disposition == "narrow_outside_scope"));
    }

    #[test]
    fn accept_overlap_requires_derivable_claims() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("b", "mac-test", {
                WorkProfile::WorkIntent(feanorfs_common::WorkIntentProfile {
                    task_id: "lexer-impl".to_string(),
                    agent: "mac-test".to_string(),
                    sequence: 1,
                    causal_base: None,
                    coordinator: None,
                    paths: vec![
                        "src/lexer-impl.rs".to_string(),
                        "src/parser-impl.rs".to_string(),
                    ],
                    concerns: vec![],
                    dependencies: vec![],
                    capabilities: vec![],
                })
            }),
            ("c", "human", {
                WorkProfile::WorkDecision(feanorfs_common::WorkDecisionProfile {
                    proposal_message_id: hex64(b'a'),
                    kind: WorkDecisionKind::AcceptOverlap(
                        feanorfs_common::WorkDecisionAcceptOverlap {
                            overlap: vec![feanorfs_common::WorkOverlapAcceptance {
                                kind: feanorfs_common::WorkOverlapKind::ExactPath,
                                path_a: Some("src/parser-impl.rs".to_string()),
                                path_b: Some("src/parser-impl.rs".to_string()),
                                concern: None,
                            }],
                            reason: None,
                        },
                    ),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(proposal.accepted_overlap.len(), 1);

        // A claim referencing a scope that does not exist stays pending.
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", {
                WorkProfile::WorkDecision(feanorfs_common::WorkDecisionProfile {
                    proposal_message_id: hex64(b'a'),
                    kind: WorkDecisionKind::AcceptOverlap(
                        feanorfs_common::WorkDecisionAcceptOverlap {
                            overlap: vec![feanorfs_common::WorkOverlapAcceptance {
                                kind: feanorfs_common::WorkOverlapKind::ExactPath,
                                path_a: Some("src/parser-impl.rs".to_string()),
                                path_b: Some("src/lexer-impl.rs".to_string()),
                                concern: None,
                            }],
                            reason: None,
                        },
                    ),
                })
            }),
        ]);
        assert!(
            !state.pending.is_empty(),
            "unprovable overlap stays pending"
        );
        assert!(find_proposal(&state, "parser-impl").decision.is_none());
    }

    #[test]
    fn concurrent_same_author_fork_keeps_canonical_branch_and_evidence() {
        let mut amended_a = intent("parser-impl", "linux-dev", 1, Some("human"));
        let WorkProfile::WorkIntent(inner) = &mut amended_a else {
            unreachable!()
        };
        inner.paths = vec!["src/canonical.rs".to_string()];
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkYield(feanorfs_common::WorkYieldProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    reason: Some("handing overlap to mac-test".to_string()),
                })
            }),
            ("f", "linux-dev", {
                WorkProfile::WorkYield(feanorfs_common::WorkYieldProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    reason: Some("handing overlap to mac-test (dup fork)".to_string()),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Yielded);
        assert_eq!(proposal.source_message_id, hex64(b'e'));
        assert!(state
            .evidence
            .iter()
            .any(|e| e.disposition == "sequence_decreased"));
    }

    #[test]
    fn yield_preserves_scope_and_blocks_completion_without_settle() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkYield(feanorfs_common::WorkYieldProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    reason: None,
                })
            }),
            ("f", "linux-dev", completed("parser-impl", 'a', 3)),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Yielded);
        assert_eq!(proposal.scope.paths.len(), 2, "yield preserves local work");
        // Completion without a prior settle is premature from yielded: it
        // stays pending (a settle may still arrive), never applied.
        assert!(state.pending.iter().any(|p| p.message_id == hex64(b'f')));
    }

    #[test]
    fn supersede_replaces_decision_then_new_decision_applies() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("d", "human", {
                WorkProfile::WorkSuperseded(feanorfs_common::WorkSupersededProfile {
                    task_id: "parser-impl".to_string(),
                    proposal_message_id: hex64(b'a'),
                    superseded_decision_message_id: hex64(b'c'),
                    reason: Some("narrow after review".to_string()),
                })
            }),
            ("e", "human", narrow('a', "src/parser-impl.rs")),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(proposal.decision.as_ref().unwrap().message_id, hex64(b'e'));
        assert_eq!(proposal.scope.paths, vec!["src/parser-impl.rs".to_string()]);
        assert!(proposal.superseded_decisions.contains(&hex64(b'c')));
    }

    #[test]
    fn store_roundtrips_and_fails_closed_on_corrupt_or_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkStore::open(dir.path()).unwrap();
        let mut state = store.load().unwrap();
        apply_batch(
            &mut state,
            &[message(
                'a',
                "linux-dev",
                &intent("parser-impl", "linux-dev", 1, None),
            )],
            false,
        );
        store
            .update(|persisted| {
                *persisted = state.clone();
                Ok(())
            })
            .unwrap();
        let reloaded = WorkStore::open(dir.path()).unwrap().load().unwrap();
        assert_eq!(reloaded.tasks.len(), 1);

        // Corrupt state fails closed on the already-open handle.
        std::fs::write(store.path(), "not json at all").unwrap();
        assert!(store.load().is_err());

        // Newer schema fails closed at open time.
        let mut newer = WorkStateFile::fresh();
        newer.schema_version = WORK_STATE_SCHEMA_VERSION + 1;
        std::fs::write(store.path(), serde_json::to_string_pretty(&newer).unwrap()).unwrap();
        assert!(WorkStore::open(dir.path()).is_err());
    }

    #[test]
    fn rebuild_after_cursor_reset_is_incomplete_without_acceptance_inference() {
        let mut state = WorkStateFile::fresh();
        // Decision observed in the rebuild window without its proposal.
        let messages = vec![message('c', "human", &accept('a'))];
        apply_batch(&mut state, &messages, true);
        assert!(state.incomplete || state.tasks.is_empty());
        assert!(
            state.tasks.is_empty(),
            "no acceptance inference without proposal"
        );
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn bound_exhaustion_drops_new_intents_deterministically() {
        let mut state = WorkStateFile::fresh();
        for index in 0..(WORK_MAX_ACTIVE_TASKS + WORK_MAX_TERMINAL_TASKS + 2) {
            let task_id = format!("task-{index:03}");
            let messages = vec![message(
                (b'a' + (index % 26) as u8) as char,
                "agent-a",
                &intent(&task_id, "agent-a", 1, None),
            )];
            apply_batch(&mut state, &messages, false);
        }
        assert!(state.tasks.len() <= WORK_MAX_ACTIVE_TASKS + WORK_MAX_TERMINAL_TASKS);
        assert!(state.dropped_count >= 2, "overflow intents must be dropped");
        // Dropped intents are bound exhaustion on non-terminal data.
        assert!(
            state.incomplete,
            "dropped intents mark the projection incomplete"
        );
    }

    fn yield_profile(task_id: &str, intent_id: char, sequence: u64, reason: &str) -> WorkProfile {
        WorkProfile::WorkYield(feanorfs_common::WorkYieldProfile {
            task_id: task_id.to_string(),
            intent_message_id: hex64(intent_id as u8),
            sequence,
            reason: Some(reason.to_string()),
        })
    }

    #[test]
    fn equal_sequence_author_forks_converge_smaller_id_wins_in_both_orders_and_batches() {
        let intent_a = (
            "a",
            "linux-dev",
            intent("parser-impl", "linux-dev", 1, Some("human")),
        );
        let accept_c = ("c", "human", accept('a'));
        let yield_e = (
            "e",
            "linux-dev",
            yield_profile("parser-impl", 'a', 2, "handing overlap to mac-test"),
        );
        let yield_f = (
            "f",
            "linux-dev",
            yield_profile("parser-impl", 'a', 2, "handing overlap to mac-test (fork)"),
        );

        // Same batch, canonical order (e before f): e applies, f evidenced.
        let state = apply_all(&[
            intent_a.clone(),
            accept_c.clone(),
            yield_e.clone(),
            yield_f.clone(),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Yielded);
        assert_eq!(proposal.source_message_id, hex64(b'e'));
        assert!(state
            .evidence
            .iter()
            .any(|e| e.disposition == "sequence_decreased"));
        assert!(state.evidence.iter().any(|e| e.message_id == hex64(b'f')));

        // Same batch, reversed author order (f before e): candidates are
        // sorted by canonical id so e still wins deterministically.
        let state = apply_all(&[
            intent_a.clone(),
            accept_c.clone(),
            yield_f.clone(),
            yield_e.clone(),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Yielded);
        assert_eq!(proposal.source_message_id, hex64(b'e'));

        // Across separate batches: the larger fork arrives first and is
        // applied, then the smaller fork arrives and displaces it via its
        // restore snapshot; both orders converge to e applied, f evidenced.
        let mut state = apply_all(&[intent_a.clone(), accept_c.clone(), yield_f.clone()]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(
            proposal.source_message_id,
            hex64(b'f'),
            "larger arrives first"
        );
        apply_batch(&mut state, &[message('e', "linux-dev", &yield_e.2)], false);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Yielded);
        assert_eq!(
            proposal.source_message_id,
            hex64(b'e'),
            "smaller displaces larger"
        );
        assert!(state
            .evidence
            .iter()
            .any(|e| e.message_id == hex64(b'f') && e.disposition == "sequence_decreased"));
    }

    #[test]
    fn equal_sequence_complete_fork_smaller_id_wins_across_batches() {
        let intent_a = (
            "a",
            "linux-dev",
            intent("parser-impl", "linux-dev", 1, Some("human")),
        );
        let accept_c = ("c", "human", accept('a'));
        let settle_e = ("e", "linux-dev", settled("parser-impl", 'a', 2));
        let complete_g = ("g", "linux-dev", completed("parser-impl", 'a', 3));
        let complete_h = ("h", "linux-dev", {
            WorkProfile::WorkCompleted(feanorfs_common::WorkCompletedProfile {
                task_id: "parser-impl".to_string(),
                intent_message_id: hex64(b'a'),
                sequence: 3,
                outcome: "Done via fork.".to_string(),
            })
        });

        let mut state = apply_all(&[
            intent_a.clone(),
            accept_c.clone(),
            settle_e.clone(),
            complete_h.clone(),
        ]);
        assert_eq!(
            find_proposal(&state, "parser-impl").source_message_id,
            hex64(b'h')
        );
        // The smaller complete arrives later and displaces the larger one.
        apply_batch(
            &mut state,
            &[message('g', "linux-dev", &complete_g.2)],
            false,
        );
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Completed);
        assert_eq!(proposal.source_message_id, hex64(b'g'));
        assert!(state
            .evidence
            .iter()
            .any(|e| e.message_id == hex64(b'h') && e.disposition == "sequence_decreased"));
    }

    #[test]
    fn concurrent_decisions_converge_smaller_id_wins_in_both_orders() {
        let intent_a = (
            "a",
            "linux-dev",
            intent("parser-impl", "linux-dev", 1, Some("human")),
        );
        // accept 'c' (smaller) and reject 'd' (larger).
        let accept_c = ("c", "human", accept('a'));
        let reject_d = ("d", "human", reject('a'));

        // Smaller first: c applies, d is evidenced.
        let state = apply_all(&[intent_a.clone(), accept_c.clone(), reject_d.clone()]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(proposal.decision.as_ref().unwrap().message_id, hex64(b'c'));
        assert!(state
            .evidence
            .iter()
            .any(|e| e.message_id == hex64(b'd') && e.disposition == "decision_already_applied"));

        // Larger first (separate batch): d applies, then c displaces it via
        // the decision restore snapshot; both orders converge to c applied.
        let mut state = apply_all(&[intent_a.clone(), reject_d.clone()]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Rejected);
        assert_eq!(proposal.decision.as_ref().unwrap().message_id, hex64(b'd'));
        apply_batch(&mut state, &[message('c', "human", &accept_c.2)], false);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(
            proposal.state,
            WorkTaskState::Accepted,
            "smaller decision displaces"
        );
        assert_eq!(proposal.decision.as_ref().unwrap().message_id, hex64(b'c'));
        assert!(state
            .evidence
            .iter()
            .any(|e| e.message_id == hex64(b'd') && e.disposition == "decision_already_applied"));
    }

    #[test]
    fn concurrent_decisions_do_not_unwind_built_upon_author_transitions() {
        // A decision that an author transition built on is not displaced by a
        // smaller decision: source_message_id no longer matches the applied
        // decision, so the incoming decision is evidenced.
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            (
                "e",
                "linux-dev",
                yield_profile("parser-impl", 'a', 2, "yield"),
            ),
            ("d", "human", reject('a')),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Yielded);
        assert_eq!(proposal.decision.as_ref().unwrap().message_id, hex64(b'c'));
        assert!(state
            .evidence
            .iter()
            .any(|e| e.message_id == hex64(b'd') && e.disposition == "decision_already_applied"));
    }

    #[test]
    fn mutual_dependency_cycle_smaller_intent_wins_in_both_orders() {
        // A (task-a, depends on task-b, id 'b') and B (task-b, depends on
        // task-a, id 'c'): 'b' < 'c', so A must apply and B must be
        // evidenced as a dependency cycle in both arrival orders.
        let mut a = intent("task-a", "agent-a", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut a else {
            unreachable!()
        };
        inner.dependencies = vec!["task-b".to_string()];
        let mut b = intent("task-b", "agent-b", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut b else {
            unreachable!()
        };
        inner.dependencies = vec!["task-a".to_string()];

        // Order 1: A ('b') first, then B ('c') is evidenced.
        let state = apply_all(&[("b", "agent-a", a.clone()), ("c", "agent-b", b.clone())]);
        assert!(any_proposal_with_intent(&state.tasks, &hex64(b'b')));
        assert!(!any_proposal_with_intent(&state.tasks, &hex64(b'c')));
        let cycle = state
            .evidence
            .iter()
            .find(|e| e.disposition == "dependency_cycle");
        assert!(cycle.is_some(), "cycle must be evidenced");
        assert_eq!(cycle.unwrap().message_id, hex64(b'c'));

        // Order 2: B ('c') arrives first and is applied, then A ('b')
        // arrives and displaces B's record to evidence; converge to A.
        let mut state = apply_all(&[("c", "agent-b", b.clone())]);
        assert!(any_proposal_with_intent(&state.tasks, &hex64(b'c')));
        apply_batch(&mut state, &[message('b', "agent-a", &a)], false);
        assert!(
            any_proposal_with_intent(&state.tasks, &hex64(b'b')),
            "smaller intent applies"
        );
        assert!(
            !any_proposal_with_intent(&state.tasks, &hex64(b'c')),
            "larger record downgraded to evidence"
        );
        let cycle = state
            .evidence
            .iter()
            .find(|e| e.disposition == "dependency_cycle");
        assert!(cycle.is_some(), "cycle must be evidenced");
        assert_eq!(cycle.unwrap().message_id, hex64(b'c'));
    }

    #[test]
    fn maintain_bounds_eviction_of_non_terminal_sets_incomplete() {
        let mut state = WorkStateFile::fresh();
        // WORK_MAX_PROPOSALS_PER_TASK + 1 proposals for one task: the lowest
        // (sequence, message id) proposal is non-terminal and is evicted.
        for index in 0..=WORK_MAX_PROPOSALS_PER_TASK {
            let messages = vec![message(
                (b'a' + index as u8) as char,
                "agent-a",
                &intent("task-a", "agent-a", index as u64 + 1, None),
            )];
            apply_batch(&mut state, &messages, false);
        }
        assert!(state.tasks[0].proposals.len() <= WORK_MAX_PROPOSALS_PER_TASK);
        assert!(
            state.incomplete,
            "evicting a non-terminal proposal marks the projection incomplete"
        );
    }

    #[test]
    fn maintain_bounds_terminal_eviction_does_not_mark_incomplete() {
        let mut state = WorkStateFile::fresh();
        // All terminal tasks: routine eviction must not mark incomplete.
        for index in 0..(WORK_MAX_TERMINAL_TASKS + 1) {
            let task_id = format!("done-{index:03}");
            let mut state_now = WorkStateFile::fresh();
            let messages = vec![
                message(
                    'a',
                    "agent-a",
                    &intent(&task_id, "agent-a", 1, Some("human")),
                ),
                message('b', "human", &accept('a')),
                message('c', "agent-a", &settled(&task_id, 'a', 2)),
                message('d', "agent-a", &completed(&task_id, 'a', 3)),
            ];
            apply_batch(&mut state_now, &messages, false);
            let completed_proposal = state_now.tasks[0].proposals.remove(0);
            assert_eq!(completed_proposal.state, WorkTaskState::Completed);
            state.tasks.push(WorkTaskRecord {
                task_id,
                proposals: vec![completed_proposal],
                updated_at_ms: 1,
            });
        }
        maintain_bounds(&mut state);
        assert!(
            !state.incomplete,
            "routine trimming of terminal records never marks incomplete"
        );
        assert!(state.tasks.len() <= WORK_MAX_TERMINAL_TASKS);
    }

    #[test]
    fn rebuild_clears_incomplete_after_clean_scan() {
        // A deliberately rebuilt projection is complete again when the
        // bounded window covers the closure without bound exhaustion.
        let mut state = WorkStateFile::fresh();
        state.incomplete = true;
        let messages = vec![
            message(
                'a',
                "linux-dev",
                &intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            message('c', "human", &accept('a')),
        ];
        apply_batch(&mut state, &messages, true);
        assert!(!state.incomplete, "clean rebuild clears incomplete");
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].proposals[0].state, WorkTaskState::Accepted);
    }

    #[test]
    fn causal_base_satisfied_via_applied_set_not_seen() {
        // A base in `applied` satisfies the causal gate.
        let mut a = intent("task-a", "agent-a", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut a else {
            unreachable!()
        };
        inner.causal_base = None;
        let mut b = intent("task-b", "agent-b", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut b else {
            unreachable!()
        };
        inner.causal_base = Some(hex64(b'a'));
        let state = apply_all(&[("a", "agent-a", a), ("b", "agent-b", b.clone())]);
        assert!(any_proposal_with_intent(&state.tasks, &hex64(b'b')));
        assert!(state.applied.contains(&hex64(b'a')));
        assert!(state.applied.contains(&hex64(b'b')));

        // A base that was only seen (evidenced fork loser) is NOT satisfied:
        // the dependent intent stays pending even though the id is in seen.
        let mut c = intent("task-c", "agent-c", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut c else {
            unreachable!()
        };
        // 'd' is the fork loser: in seen, never applied.
        inner.causal_base = Some(hex64(b'd'));
        // Fork: 'd' repeats 'b's (task, agent, sequence); 'd' > 'b' loses.
        let mut d = intent("task-b", "agent-b", 1, None);
        let WorkProfile::WorkIntent(inner) = &mut d else {
            unreachable!()
        };
        inner.causal_base = Some(hex64(b'a'));
        let mut state = apply_all(&[
            ("a", "agent-a", intent("task-a", "agent-a", 1, None)),
            ("b", "agent-b", b.clone()),
            ("d", "agent-b", d),
        ]);
        assert!(state
            .evidence
            .iter()
            .any(|e| e.message_id == hex64(b'd') && e.disposition == "sequence_decreased"));
        assert!(state.seen.contains(&hex64(b'd')));
        assert!(!state.applied.contains(&hex64(b'd')));
        apply_batch(&mut state, &[message('c', "agent-c", &c)], false);
        assert!(
            !any_proposal_with_intent(&state.tasks, &hex64(b'c')),
            "base only in seen never satisfies the causal gate"
        );
        assert!(state.pending.iter().any(|p| p.message_id == hex64(b'c')));
    }

    #[test]
    fn capabilities_present_on_status_and_preserved_through_transitions() {
        let mut intent_profile = intent("parser-impl", "linux-dev", 1, Some("human"));
        let WorkProfile::WorkIntent(inner) = &mut intent_profile else {
            unreachable!()
        };
        inner.capabilities = vec!["rust".to_string(), "wasm".to_string()];
        let state = apply_all(&[
            ("a", "linux-dev", intent_profile),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkAmendment(feanorfs_common::WorkAmendmentProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    paths: Some(vec!["src/parser-impl.rs".to_string()]),
                    concerns: None,
                    dependencies: None,
                    approval_decision_id: None,
                    reason: Some("narrow within original scope".to_string()),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(
            proposal.capabilities,
            vec!["rust".to_string(), "wasm".to_string()]
        );
        let result = status_result(&state, 0, false);
        assert_eq!(
            result.tasks[0].proposals[0].capabilities,
            vec!["rust".to_string(), "wasm".to_string()]
        );
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(proposal.scope.paths, vec!["src/parser-impl.rs".to_string()]);
    }

    #[test]
    fn amendment_expansion_requires_approval_and_applies_with_valid_approval() {
        let intent_a = intent("parser-impl", "linux-dev", 1, Some("human"));
        // Original scope: src/parser-impl.rs only.
        let state = apply_all(&[("a", "linux-dev", intent_a), ("c", "human", accept('a'))]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);

        // Expansion beyond the original scope without approval is evidenced
        // and leaves the record unchanged.
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkAmendment(feanorfs_common::WorkAmendmentProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    paths: Some(vec!["outside/scope.rs".to_string()]),
                    concerns: None,
                    dependencies: None,
                    approval_decision_id: None,
                    reason: Some("expand".to_string()),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(
            proposal.state,
            WorkTaskState::Accepted,
            "rejected amendment leaves the record unchanged"
        );
        assert_eq!(proposal.scope.paths.len(), 2, "original scope preserved");
        assert!(state
            .evidence
            .iter()
            .any(|e| { e.disposition == "amendment_expands_scope_without_approval" }));

        // Same expansion WITH the applied decision id as approval applies.
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkAmendment(feanorfs_common::WorkAmendmentProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    paths: Some(vec!["outside/scope.rs".to_string()]),
                    concerns: None,
                    dependencies: None,
                    approval_decision_id: Some(hex64(b'c')),
                    reason: Some("coordinator-approved expansion".to_string()),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(
            proposal.scope.paths,
            vec!["outside/scope.rs".to_string()],
            "approved expansion applies"
        );
        assert_eq!(proposal.amendments.len(), 1);
        assert_eq!(
            proposal.amendments[0].approval_decision_id.as_deref(),
            Some(hex64(b'c').as_str())
        );
        // The approval ref is exposed in status causal_refs.
        let result = status_result(&state, 0, false);
        assert!(result.tasks[0].proposals[0]
            .causal_refs
            .contains(&hex64(b'c')));
    }

    #[test]
    fn amendment_with_wrong_approval_reference_is_rejected() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkAmendment(feanorfs_common::WorkAmendmentProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    paths: Some(vec!["outside/scope.rs".to_string()]),
                    concerns: None,
                    dependencies: None,
                    // 'd' is not the applied decision id ('c').
                    approval_decision_id: Some(hex64(b'd')),
                    reason: Some("stale approval".to_string()),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.scope.paths.len(), 2, "scope unchanged");
        assert!(state
            .evidence
            .iter()
            .any(|e| { e.disposition == "amendment_expands_scope_without_approval" }));
    }

    #[test]
    fn amendment_within_original_scope_applies_without_approval() {
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, Some("human")),
            ),
            ("c", "human", accept('a')),
            ("e", "linux-dev", {
                WorkProfile::WorkAmendment(feanorfs_common::WorkAmendmentProfile {
                    task_id: "parser-impl".to_string(),
                    intent_message_id: hex64(b'a'),
                    sequence: 2,
                    paths: Some(vec!["src/parser-impl.rs".to_string()]),
                    concerns: None,
                    dependencies: None,
                    approval_decision_id: None,
                    reason: Some("narrow within original".to_string()),
                })
            }),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(
            proposal.scope.paths,
            vec!["src/parser-impl.rs".to_string()],
            "in-scope amendment applies without approval"
        );
        assert_eq!(proposal.amendments.len(), 1);
    }

    #[test]
    fn coordinator_less_proposal_author_is_the_authority() {
        // Without a named coordinator, the proposal's author is the
        // authorized decision maker; the observer/human fallback is gone.
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, None),
            ),
            ("c", "human", accept('a')),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Proposed);
        assert!(proposal.decision.is_none());
        assert!(state
            .evidence
            .iter()
            .any(|e| e.disposition == "unauthorized_coordinator"));

        // The author's own decision applies without a named coordinator.
        let state = apply_all(&[
            (
                "a",
                "linux-dev",
                intent("parser-impl", "linux-dev", 1, None),
            ),
            ("c", "linux-dev", accept('a')),
        ]);
        let proposal = find_proposal(&state, "parser-impl");
        assert_eq!(proposal.state, WorkTaskState::Accepted);
        assert_eq!(proposal.decision.as_ref().unwrap().message_id, hex64(b'c'));
    }
}
