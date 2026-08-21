//! Accepted-work scope resolution and admission for continuous runners.
//!
//! This module is the single admission authority shared by the configured
//! runner worker (`client/src/cli/agent_runner.rs`) and the continuous
//! controller (`client/src/cli/agent_live.rs`). It resolves one direct
//! request message to the exact accepted intent through the `ffwork1` reducer
//! projection and returns typed rejection reasons — never
//! text matching. The land guard itself stays in the canonical land/diff
//! layer; this module only decides *whether* a launch or a publication is
//! authorized.

use anyhow::{ensure, Context as _, Result};
use feanorfs_common::{
    evaluate_scope_overlap, is_valid_hash, is_valid_task_id, parse_work_profile,
    validate_work_scope, WorkIntentProfile, WorkOverlap, WorkOverlapAcceptance, WorkProfile,
    WorkProposalStatus, WorkScope, WorkStatusResult, WorkTaskState, WorkTaskStatus,
};
use serde::{Deserialize, Serialize};

use super::diff::compute_agent_diff;
use crate::ctx::SyncCtx;
use crate::work::WorkStateFile;

/// One bounded, versioned accepted-work identity bound into the runner
/// pre-spawn checkpoint and the child invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedWorkDescriptor {
    pub schema_version: u32,
    /// Canonical task id the accepted intent belongs to.
    pub task_id: String,
    /// Exact intent message id; equals the direct request message id.
    pub intent_message_id: String,
    /// Proposal author; must equal the configured runner agent.
    pub agent: String,
    /// Author sequence of the accepted intent.
    pub sequence: u64,
    /// Accepted scope (post-narrow and post-amendment).
    pub scope: WorkScope,
    /// Advertised capabilities from the accepted record, preserved through
    /// every transition and rebuild. The runner request must advertise the
    /// exact same set (see [`RunnerAdmissionReject::CapabilityMismatch`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Named coordinator whose decisions authorized this acceptance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    /// Causal base message id the intent built on, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causal_base: Option<String>,
    /// Snapshot the request concerned (the accepted base).
    pub base_snapshot: String,
    /// Binding fingerprint of the exact request message; a checkpoint whose
    /// fingerprint differs from the re-read message is never launched.
    pub message_fingerprint: String,
    /// Message id of the last applied transition on the accepted record.
    pub source_message_id: String,
    /// Display/liveness hint only; never drives decisions.
    pub updated_at_ms: i64,
}

/// Current schema version of [`AcceptedWorkDescriptor`].
pub const ACCEPTED_WORK_SCHEMA_VERSION: u32 = 1;

/// Typed admission rejection reasons. These are internal, typed decisions —
/// never inferred from rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerAdmissionReject {
    /// The request body carries no work intent (or is not a work request).
    RequestWithoutIntent,
    /// No proposal exists for this request's intent message.
    MissingIntent,
    /// The proposal exists but is not accepted (proposed/rejected/etc).
    ProposalNotAccepted,
    /// The proposal author differs from the configured runner agent.
    WrongAgent,
    /// The projection is incomplete (cursor reset or bound exhaustion);
    /// acceptance cannot be proven, so admission is denied.
    ProjectionIncomplete,
    /// The proposal's causal base message has not been observed.
    UnreachableBase,
    /// A dependency task is not settled/completed.
    UnsettledDependency,
    /// A newer accepted intent supersedes this request's intent.
    SupersededIntent,
    /// Another accepted nonterminal owner overlaps this scope.
    OverlappingOwner,
    /// The request advertises capabilities that differ from the accepted
    /// record's authenticated capability set.
    CapabilityMismatch,
    /// The accepted scope expands beyond the request-declared grant without
    /// an applied coordinator approval decision.
    ScopeExpansionWithoutApproval,
    /// A scope-amendment wait is active for this exact intent and the
    /// accepted scope has not changed; the runner keeps waiting instead of
    /// relaunching stale work.
    ScopeAmendmentPending,
}

impl RunnerAdmissionReject {
    /// Stable wire string for this reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestWithoutIntent => "request_without_intent",
            Self::MissingIntent => "missing_intent",
            Self::ProposalNotAccepted => "proposal_not_accepted",
            Self::WrongAgent => "wrong_agent",
            Self::ProjectionIncomplete => "projection_incomplete",
            Self::UnreachableBase => "unreachable_base",
            Self::UnsettledDependency => "unsettled_dependency",
            Self::SupersededIntent => "superseded_intent",
            Self::OverlappingOwner => "overlapping_owner",
            Self::CapabilityMismatch => "capability_mismatch",
            Self::ScopeExpansionWithoutApproval => "scope_expansion_without_approval",
            Self::ScopeAmendmentPending => "scope_amendment_pending",
        }
    }
}

/// Validates one accepted-work descriptor in full (schema, ids, bounds,
/// causal references). Called on state load and at admission.
///
/// # Errors
/// Returns an error for any invalid, out-of-bounds, or causally inconsistent
/// descriptor field.
pub fn validate_accepted_work(
    descriptor: &AcceptedWorkDescriptor,
    agent: &str,
    intent_message_id: &str,
) -> Result<()> {
    ensure!(
        descriptor.schema_version == ACCEPTED_WORK_SCHEMA_VERSION,
        "unsupported accepted-work descriptor schema {}",
        descriptor.schema_version
    );
    ensure!(
        is_valid_task_id(&descriptor.task_id),
        "accepted-work descriptor has an invalid task id"
    );
    ensure!(
        is_valid_hash(&descriptor.intent_message_id)
            && descriptor.intent_message_id == intent_message_id,
        "accepted-work descriptor intent does not match the checkpoint request"
    );
    ensure!(
        descriptor.agent == agent,
        "accepted-work descriptor agent '{}' does not match the configured runner agent '{agent}'",
        descriptor.agent
    );
    ensure!(
        is_valid_hash(&descriptor.base_snapshot),
        "accepted-work descriptor base snapshot is invalid"
    );
    ensure!(
        !descriptor.message_fingerprint.is_empty()
            && descriptor.message_fingerprint.len() <= 128
            && descriptor
                .message_fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
        "accepted-work descriptor fingerprint is invalid"
    );
    ensure!(
        is_valid_hash(&descriptor.source_message_id),
        "accepted-work descriptor source message id is invalid"
    );
    if let Some(base) = &descriptor.causal_base {
        ensure!(
            is_valid_hash(base),
            "accepted-work descriptor causal base is invalid"
        );
    }
    if let Some(coordinator) = &descriptor.coordinator {
        ensure!(
            feanorfs_common::is_valid_agent_name(coordinator),
            "accepted-work descriptor coordinator is invalid"
        );
    }
    validate_work_scope(
        &descriptor.scope.paths,
        &descriptor.scope.concerns,
        &descriptor.scope.dependencies,
        &descriptor.capabilities,
    )
    .context("accepted-work descriptor scope is invalid")
}

/// Computes the deterministic binding fingerprint of one request message.
#[must_use]
pub fn message_fingerprint(message: &feanorfs_common::AgentMessage) -> String {
    blake3::hash(
        format!(
            "{}|{}|{}|{}|{}|{}",
            message.message_id,
            message.from,
            message.to,
            message.body,
            message.about_snapshot,
            message.reply_to.as_deref().unwrap_or("")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

fn find_task<'a>(projection: &'a WorkStatusResult, task_id: &str) -> Option<&'a WorkTaskStatus> {
    projection.tasks.iter().find(|task| task.task_id == task_id)
}

fn find_proposal<'a>(
    task: &'a WorkTaskStatus,
    intent_message_id: &str,
) -> Option<&'a WorkProposalStatus> {
    task.proposals
        .iter()
        .find(|proposal| proposal.intent_message_id == intent_message_id)
}

fn causal_base_of(proposal: &WorkProposalStatus) -> Option<&str> {
    proposal.causal_refs.first().map(String::as_str)
}

fn overlap_claim_matches(claim: &WorkOverlapAcceptance, derived: &WorkOverlap) -> bool {
    claim.kind == derived.kind
        && claim.path_a == derived.path_a
        && claim.path_b == derived.path_b
        && claim.concern == derived.concern
}

/// Whether another accepted nonterminal proposal blocks this one through an
/// overlap the coordinator did not explicitly accept. Same-`(task, agent)`
/// proposals with a lower sequence are superseded by this intent and never
/// count as blocking owners.
fn blocking_overlap(
    projection: &WorkStatusResult,
    task_id: &str,
    proposal: &WorkProposalStatus,
) -> bool {
    for task in &projection.tasks {
        for other in &task.proposals {
            if other.intent_message_id == proposal.intent_message_id {
                continue;
            }
            if other.state != WorkTaskState::Accepted {
                continue;
            }
            if task.task_id == task_id
                && other.agent == proposal.agent
                && other.sequence < proposal.sequence
            {
                continue;
            }
            let overlaps = evaluate_scope_overlap(
                &proposal.accepted_scope.paths,
                &proposal.accepted_scope.concerns,
                &other.accepted_scope.paths,
                &other.accepted_scope.concerns,
            );
            if overlaps.is_empty() {
                continue;
            }
            if overlaps.iter().all(|overlap| {
                proposal
                    .accepted_overlap
                    .iter()
                    .any(|claim| overlap_claim_matches(claim, overlap))
            }) {
                continue;
            }
            return true;
        }
    }
    false
}

/// Whether a newer generation for the same `(task, agent)` supersedes
/// this proposal. Any newer generation in a live or terminal state
/// ({Proposed, Accepted, Yielded, Settled, Completed, Blocked}) supersedes
/// an older one; a newer rejected generation never does. (The projection
/// has no `Superseded` state: superseding a decision returns the proposal
/// to `Proposed`, which is a live superseding generation.)
fn superseded_by_newer(
    projection: &WorkStatusResult,
    task_id: &str,
    proposal: &WorkProposalStatus,
) -> bool {
    let Some(task) = find_task(projection, task_id) else {
        return false;
    };
    task.proposals.iter().any(|other| {
        other.agent == proposal.agent
            && other.state != WorkTaskState::Rejected
            && (other.sequence > proposal.sequence
                || (other.sequence == proposal.sequence
                    && other.intent_message_id != proposal.intent_message_id
                    && other.intent_message_id < proposal.intent_message_id))
    })
}

/// Resolves one direct request message to its accepted intent.
///
/// Returns `Ok(Some(descriptor))` when the request is an admissible work
/// request; `Ok(None)` when the message is not a work request at all (only
/// meaningful for non-enforced runners); and `Err(reason)` for every typed
/// rejection. The resolution is pure over the reducer projection and its
/// authenticated applied ancestry, so re-running it after every refresh
/// yields identical results for identical observations. The bounded
/// observation cache (`seen`) never satisfies admission.
#[allow(clippy::result_large_err)]
pub fn resolve_request_admission(
    projection: &WorkStatusResult,
    _work_state: &WorkStateFile,
    request: &feanorfs_common::AgentMessage,
    agent: &str,
) -> std::result::Result<Option<AcceptedWorkDescriptor>, RunnerAdmissionReject> {
    let WorkProfile::WorkIntent(intent) =
        parse_work_profile(&request.body).ok_or(RunnerAdmissionReject::RequestWithoutIntent)?
    else {
        return Err(RunnerAdmissionReject::RequestWithoutIntent);
    };

    // Fail closed before trusting any projection-derived acceptance.
    if projection.projection_incomplete {
        return Err(RunnerAdmissionReject::ProjectionIncomplete);
    }

    let task =
        find_task(projection, &intent.task_id).ok_or(RunnerAdmissionReject::MissingIntent)?;
    let proposal =
        find_proposal(task, &request.message_id).ok_or(RunnerAdmissionReject::MissingIntent)?;

    if proposal.agent != agent {
        return Err(RunnerAdmissionReject::WrongAgent);
    }
    if proposal.state != WorkTaskState::Accepted {
        return Err(RunnerAdmissionReject::ProposalNotAccepted);
    }

    // A request that advertises capabilities must match the accepted
    // record's authenticated capability set exactly.
    if !intent.capabilities.is_empty() && intent.capabilities != proposal.capabilities {
        return Err(RunnerAdmissionReject::CapabilityMismatch);
    }

    // Causal base must be in the authenticated applied ancestry, or be the
    // intent id of an applied record present in the projection (mirrors
    // apply_intent). The bounded observation cache never satisfies.
    if let Some(base) = causal_base_of(proposal) {
        let base_applied = projection.applied_message_ids.iter().any(|id| id == base)
            || projection
                .tasks
                .iter()
                .any(|t| t.proposals.iter().any(|p| p.intent_message_id == base));
        if !base_applied {
            return Err(RunnerAdmissionReject::UnreachableBase);
        }
    }

    // A dependency must be completed; blocked (or missing, accepted,
    // rejected, yielded) dependencies never permit dependent work.
    for dependency in &proposal.accepted_scope.dependencies {
        match find_task(projection, dependency) {
            Some(dep) if dep.state == WorkTaskState::Completed => {}
            Some(_) | None => return Err(RunnerAdmissionReject::UnsettledDependency),
        }
    }

    if superseded_by_newer(projection, &intent.task_id, proposal) {
        return Err(RunnerAdmissionReject::SupersededIntent);
    }

    if blocking_overlap(projection, &intent.task_id, proposal) {
        return Err(RunnerAdmissionReject::OverlappingOwner);
    }

    // The accepted scope may not grow beyond the grant the request itself
    // declared without an applied coordinator approval decision (the
    // reducer records the approval ref on the applied amendment).
    if !scope_contained_in_declared(&proposal.accepted_scope, &intent)
        && !proposal.causal_refs.iter().any(|ref_id| {
            projection
                .applied_message_ids
                .iter()
                .any(|applied| applied == ref_id)
        })
    {
        return Err(RunnerAdmissionReject::ScopeExpansionWithoutApproval);
    }

    Ok(Some(AcceptedWorkDescriptor {
        schema_version: ACCEPTED_WORK_SCHEMA_VERSION,
        task_id: intent.task_id.clone(),
        intent_message_id: request.message_id.clone(),
        agent: proposal.agent.clone(),
        sequence: proposal.sequence,
        scope: proposal.accepted_scope.clone(),
        capabilities: proposal.capabilities.clone(),
        coordinator: proposal.coordinator.clone(),
        causal_base: causal_base_of(proposal).map(str::to_string),
        base_snapshot: request.about_snapshot.clone(),
        message_fingerprint: message_fingerprint(request),
        source_message_id: proposal.source_message_id.clone(),
        updated_at_ms: proposal.updated_at_ms,
    }))
}

/// Whether the accepted scope stays within the grant the request itself
/// declared: every accepted path is covered by a declared path entry, and
/// every accepted concern/dependency was declared. Anything outside is an
/// expansion requiring an applied approval decision.
fn scope_contained_in_declared(accepted: &WorkScope, intent: &WorkIntentProfile) -> bool {
    accepted.paths.iter().all(|path| {
        intent
            .paths
            .iter()
            .any(|entry| feanorfs_common::work_contract::scope_entry_covers_path(entry, path))
    }) && accepted
        .concerns
        .iter()
        .all(|concern| intent.concerns.contains(concern))
        && accepted
            .dependencies
            .iter()
            .all(|dependency| intent.dependencies.contains(dependency))
}

/// Bounded outbound partition of one agent's local changes against an
/// accepted scope. The canonical land layer re-partitions its own fresh diff
/// at mutation time; this read-only probe only decides whether to publish a
/// scope-change request and how many paths are involved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentScopePartition {
    pub in_scope_changes: usize,
    pub out_of_scope: Vec<String>,
    /// Exact diff entries for the out-of-scope paths (same authoritative
    /// diff that produced the partition). Consumers derive the blocked
    /// operation set from these — never from a racy worktree re-scan.
    pub out_of_scope_changes: Vec<feanorfs_common::FileState>,
}

/// Computes the bounded partition of the agent worktree diff against one
/// accepted scope without mutating anything.
///
/// # Errors
/// Returns an error when the agent diff cannot be computed (missing agent,
/// corrupt state, transport failure).
pub async fn partition_agent_scope(
    ctx: &SyncCtx<'_>,
    name: &str,
    scope: &WorkScope,
) -> Result<AgentScopePartition> {
    let diff = compute_agent_diff(ctx, name).await?;
    let paths = diff
        .our_changes
        .iter()
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    let partition = feanorfs_common::work_contract::partition_scope_paths(&paths, scope);
    let out_of_scope_changes = diff
        .our_changes
        .into_iter()
        .filter(|change| !feanorfs_common::work_contract::scope_covers_path(scope, &change.path))
        .collect::<Vec<_>>();
    Ok(AgentScopePartition {
        in_scope_changes: partition.in_scope.len(),
        out_of_scope: partition.out_of_scope,
        out_of_scope_changes,
    })
}

/// Canonical engine-level scope filter over one computed agent diff. Keeps
/// only in-scope changes and in-scope conflicts; everything else stays local
/// and unlanded. The land algorithm itself is not copied here.
pub(crate) fn filter_diff_by_scope(
    mut diff: super::diff::AgentDiff,
    scope: &WorkScope,
) -> super::diff::AgentDiff {
    diff.our_changes
        .retain(|change| feanorfs_common::work_contract::scope_covers_path(scope, &change.path));
    diff.conflicts
        .retain(|(edit, _)| feanorfs_common::work_contract::scope_covers_path(scope, &edit.path));
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::WorkStateFile;
    use feanorfs_common::{
        encode_work_profile, AgentMessage, AgentMessageKind, WorkIntentProfile, WorkProposalStatus,
        WorkTaskStatus, WorkVerification,
    };

    fn hex(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn intent(task_id: &str, agent: &str, sequence: u64, causal_base: Option<char>) -> WorkProfile {
        intent_full(
            task_id,
            agent,
            sequence,
            causal_base,
            vec![],
            vec![],
            vec![],
        )
    }

    fn intent_full(
        task_id: &str,
        agent: &str,
        sequence: u64,
        causal_base: Option<char>,
        paths: Vec<&str>,
        dependencies: Vec<&str>,
        capabilities: Vec<&str>,
    ) -> WorkProfile {
        WorkProfile::WorkIntent(WorkIntentProfile {
            task_id: task_id.to_string(),
            agent: agent.to_string(),
            sequence,
            causal_base: causal_base.map(hex),
            coordinator: Some("human".to_string()),
            paths: if paths.is_empty() {
                vec!["src/task.rs".to_string()]
            } else {
                paths.into_iter().map(str::to_string).collect()
            },
            concerns: vec!["task behavior".to_string()],
            dependencies: dependencies.into_iter().map(str::to_string).collect(),
            capabilities: capabilities.into_iter().map(str::to_string).collect(),
        })
    }

    fn intent_with_capabilities(
        task_id: &str,
        agent: &str,
        sequence: u64,
        causal_base: Option<char>,
        capabilities: Vec<&str>,
    ) -> WorkProfile {
        intent_full(
            task_id,
            agent,
            sequence,
            causal_base,
            vec![],
            vec![],
            capabilities,
        )
    }

    fn req(profile: &WorkProfile) -> AgentMessage {
        AgentMessage {
            message_id: hex('1'),
            from: "human".to_string(),
            to: "worker".to_string(),
            kind: AgentMessageKind::Request,
            body: encode_work_profile(profile).unwrap(),
            about_snapshot: hex('f'),
            reply_to: None,
            created_at_ms: 1,
        }
    }

    fn scope(paths: Vec<&str>, dependencies: Vec<&str>) -> WorkScope {
        WorkScope {
            paths: paths.into_iter().map(str::to_string).collect(),
            concerns: vec!["task behavior".to_string()],
            dependencies: dependencies.into_iter().map(str::to_string).collect(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn proposal(
        task_id: &str,
        agent: &str,
        sequence: u64,
        intent_message_id: char,
        state: WorkTaskState,
        accepted_scope: WorkScope,
        causal_base: Option<char>,
        accepted_overlap: Vec<WorkOverlapAcceptance>,
    ) -> (String, WorkProposalStatus) {
        proposal_with_capabilities(
            task_id,
            agent,
            sequence,
            intent_message_id,
            state,
            accepted_scope,
            causal_base,
            accepted_overlap,
            vec![],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn proposal_with_capabilities(
        task_id: &str,
        agent: &str,
        sequence: u64,
        intent_message_id: char,
        state: WorkTaskState,
        accepted_scope: WorkScope,
        causal_base: Option<char>,
        accepted_overlap: Vec<WorkOverlapAcceptance>,
        capabilities: Vec<&str>,
    ) -> (String, WorkProposalStatus) {
        let mut causal_refs = Vec::new();
        if let Some(base) = causal_base {
            causal_refs.push(hex(base));
        }
        (
            task_id.to_string(),
            WorkProposalStatus {
                agent: agent.to_string(),
                state,
                sequence,
                intent_message_id: hex(intent_message_id),
                coordinator: Some("human".to_string()),
                accepted_scope,
                capabilities: capabilities.into_iter().map(str::to_string).collect(),
                decision: None,
                accepted_overlap,
                amendments: Vec::new(),
                causal_refs,
                inspected_snapshot: None,
                verification: None,
                outcome: None,
                reason: None,
                source_message_id: hex(intent_message_id),
                updated_at_ms: sequence as i64,
            },
        )
    }

    fn project(
        tasks: Vec<(String, WorkProposalStatus)>,
        seen: Vec<String>,
        incomplete: bool,
    ) -> (WorkStatusResult, WorkStateFile) {
        project_with_applied(tasks, seen, incomplete, Vec::new())
    }

    fn project_with_applied(
        tasks: Vec<(String, WorkProposalStatus)>,
        seen: Vec<String>,
        incomplete: bool,
        applied_message_ids: Vec<String>,
    ) -> (WorkStatusResult, WorkStateFile) {
        use std::collections::BTreeMap;
        let mut grouped: BTreeMap<String, Vec<WorkProposalStatus>> = BTreeMap::new();
        for (task_id, proposal) in tasks {
            grouped.entry(task_id).or_default().push(proposal);
        }
        let tasks = grouped
            .into_iter()
            .map(|(task_id, proposals)| {
                let state = proposals
                    .iter()
                    .max_by(|a, b| {
                        a.sequence
                            .cmp(&b.sequence)
                            .then_with(|| a.intent_message_id.cmp(&b.intent_message_id))
                    })
                    .map(|p| p.state)
                    .unwrap_or(WorkTaskState::Proposed);
                WorkTaskStatus {
                    state,
                    task_id,
                    proposals,
                }
            })
            .collect::<Vec<_>>();
        let projection = WorkStatusResult {
            cursor: hex('z'),
            cursor_reset: false,
            projection_incomplete: incomplete,
            messages_processed: 1,
            tasks,
            evidence_count: 0,
            dropped_count: 0,
            updated_at_ms: 1,
            applied_message_ids: applied_message_ids.clone(),
        };
        let work_state = WorkStateFile {
            schema_version: 1,
            cursor: Some(hex('z')),
            incomplete,
            tasks: Vec::new(),
            evidence: Vec::new(),
            seen,
            applied: applied_message_ids.clone(),
            pending: Vec::new(),
            dropped_count: 0,
            updated_at_ms: 1,
        };
        (projection, work_state)
    }

    fn accepted_project() -> (WorkStatusResult, WorkStateFile) {
        project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs"], vec![]),
                None,
                vec![],
            )],
            vec![hex('1')],
            false,
        )
    }

    #[test]
    fn request_without_intent_is_rejected() {
        let (projection, work_state) = accepted_project();
        let mut plain = req(&intent("task-one", "worker", 1, None));
        plain.body = "perform the configured task".to_string();
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &plain, "worker"),
            Err(RunnerAdmissionReject::RequestWithoutIntent)
        );
        let mut decision = req(&intent("task-one", "worker", 1, None));
        decision.body = encode_work_profile(&WorkProfile::WorkDecision(
            feanorfs_common::WorkDecisionProfile {
                proposal_message_id: hex('1'),
                kind: feanorfs_common::WorkDecisionKind::Accept(
                    feanorfs_common::WorkDecisionAccept { reason: None },
                ),
            },
        ))
        .unwrap();
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &decision, "worker"),
            Err(RunnerAdmissionReject::RequestWithoutIntent)
        );
    }

    #[test]
    fn proposed_but_not_accepted_is_rejected() {
        let (projection, work_state) = project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Proposed,
                scope(vec!["src/task.rs"], vec![]),
                None,
                vec![],
            )],
            vec![hex('1')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::ProposalNotAccepted)
        );
    }

    #[test]
    fn wrong_agent_and_missing_intent_are_rejected() {
        let (projection, work_state) = accepted_project();
        let request = req(&intent("task-one", "worker", 1, None));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "other"),
            Err(RunnerAdmissionReject::WrongAgent)
        );
        let (projection, work_state) = project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs"], vec![]),
                None,
                vec![],
            )],
            vec![hex('1')],
            false,
        );
        let mut other_task = req(&intent("task-two", "worker", 1, None));
        other_task.message_id = hex('2');
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &other_task, "worker"),
            Err(RunnerAdmissionReject::MissingIntent)
        );
    }

    #[test]
    fn incomplete_projection_never_proves_acceptance() {
        let (projection, work_state) = project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs"], vec![]),
                None,
                vec![],
            )],
            vec![hex('1')],
            true,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::ProjectionIncomplete)
        );
    }

    #[test]
    fn unreachable_causal_base_is_rejected() {
        let (projection, work_state) = project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs"], vec![]),
                Some('9'),
                vec![],
            )],
            // The base message '9' is neither in the applied ancestry nor
            // an intent id in the projection.
            vec![hex('1')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, Some('9')));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::UnreachableBase)
        );
    }

    #[test]
    fn unsettled_dependency_is_rejected() {
        let (projection, work_state) = project(
            vec![
                proposal(
                    "task-one",
                    "worker",
                    1,
                    '1',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec!["dep-task"]),
                    None,
                    vec![],
                ),
                proposal(
                    "dep-task",
                    "worker",
                    1,
                    '2',
                    WorkTaskState::Accepted,
                    scope(vec!["src/dep.rs"], vec![]),
                    None,
                    vec![],
                ),
            ],
            vec![hex('1'), hex('2')],
            false,
        );
        let request = req(&intent_full(
            "task-one",
            "worker",
            1,
            None,
            vec!["src/task.rs"],
            vec!["dep-task"],
            vec![],
        ));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::UnsettledDependency)
        );

        // A completed dependency admits (the request declares the same
        // dependency the accepted scope carries).
        let (projection, work_state) = project(
            vec![
                proposal(
                    "task-one",
                    "worker",
                    1,
                    '1',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec!["dep-task"]),
                    None,
                    vec![],
                ),
                proposal(
                    "dep-task",
                    "worker",
                    1,
                    '2',
                    WorkTaskState::Completed,
                    scope(vec!["src/dep.rs"], vec![]),
                    None,
                    vec![],
                ),
            ],
            vec![hex('1'), hex('2')],
            false,
        );
        let request = req(&intent_full(
            "task-one",
            "worker",
            1,
            None,
            vec!["src/task.rs"],
            vec!["dep-task"],
            vec![],
        ));
        assert!(resolve_request_admission(&projection, &work_state, &request, "worker").is_ok());
    }

    #[test]
    fn newer_accepted_intent_supersedes() {
        let (projection, work_state) = project(
            vec![
                proposal(
                    "task-one",
                    "worker",
                    1,
                    '1',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec![]),
                    None,
                    vec![],
                ),
                proposal(
                    "task-one",
                    "worker",
                    2,
                    '2',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs", "src/more.rs"], vec![]),
                    None,
                    vec![],
                ),
            ],
            vec![hex('1'), hex('2')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::SupersededIntent)
        );
        // The newest accepted intent is the one admitted; its request
        // declares the full amended scope so it stays within its own grant.
        let mut newer = req(&intent_full(
            "task-one",
            "worker",
            2,
            None,
            vec!["src/more.rs", "src/task.rs"],
            vec![],
            vec![],
        ));
        newer.message_id = hex('2');
        assert!(resolve_request_admission(&projection, &work_state, &newer, "worker").is_ok());
    }

    #[test]
    fn overlapping_accepted_owner_blocks_unless_accept_overlap() {
        let (projection, work_state) = project(
            vec![
                proposal(
                    "task-one",
                    "worker",
                    1,
                    '1',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec![]),
                    None,
                    vec![],
                ),
                proposal(
                    "task-two",
                    "worker",
                    1,
                    '2',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec![]),
                    None,
                    vec![],
                ),
            ],
            vec![hex('1'), hex('2')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::OverlappingOwner)
        );

        // Explicit accept-overlap entries relax the block.
        let overlap = feanorfs_common::evaluate_scope_overlap(
            &["src/task.rs".to_string()],
            &["task behavior".to_string()],
            &["src/task.rs".to_string()],
            &["task behavior".to_string()],
        );
        let accepted_overlap = overlap
            .iter()
            .map(WorkOverlapAcceptance::from_overlap)
            .collect();
        let (projection, work_state) = project(
            vec![
                proposal(
                    "task-one",
                    "worker",
                    1,
                    '1',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec![]),
                    None,
                    accepted_overlap,
                ),
                proposal(
                    "task-two",
                    "worker",
                    1,
                    '2',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec![]),
                    None,
                    vec![],
                ),
            ],
            vec![hex('1'), hex('2')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert!(resolve_request_admission(&projection, &work_state, &request, "worker").is_ok());
    }

    #[test]
    fn amendment_expansion_requires_approval() {
        // An amendment that expands the scope beyond the request-declared
        // grant is not admitted without an applied approval decision.
        let (projection, work_state) = project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs", "src/extra.rs"], vec![]),
                None,
                vec![],
            )],
            vec![hex('1')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::ScopeExpansionWithoutApproval)
        );

        // The same expansion with a valid applied approval decision id
        // (recorded on the applied amendment) admits with the amended scope.
        let (task_id, mut approved) = proposal(
            "task-one",
            "worker",
            1,
            '1',
            WorkTaskState::Accepted,
            scope(vec!["src/task.rs", "src/extra.rs"], vec![]),
            None,
            vec![],
        );
        approved.causal_refs.push(hex('9'));
        let (projection, work_state) = project_with_applied(
            vec![(task_id, approved)],
            vec![hex('1')],
            false,
            vec![hex('9')],
        );
        let request = req(&intent("task-one", "worker", 1, None));
        let admission = resolve_request_admission(&projection, &work_state, &request, "worker")
            .unwrap()
            .expect("approved expansion admits with the amended scope");
        assert_eq!(admission.task_id, "task-one");
        assert_eq!(admission.intent_message_id, request.message_id);
        assert_eq!(admission.agent, "worker");
        assert_eq!(admission.sequence, 1);
        assert_eq!(admission.scope.paths, ["src/task.rs", "src/extra.rs"]);
        assert_eq!(admission.base_snapshot, request.about_snapshot);
        assert_eq!(admission.message_fingerprint, message_fingerprint(&request));
        assert_eq!(admission.source_message_id, hex('1'));

        // An amendment within the original declared scope admits without
        // approval (the accepted scope is contained in the declared grant).
        let (projection, work_state) = project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs"], vec![]),
                None,
                vec![],
            )],
            vec![hex('1')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert!(resolve_request_admission(&projection, &work_state, &request, "worker").is_ok());
    }

    #[test]
    fn capability_mismatch_rejects_admission() {
        // The record authenticates ["rust"]; a request advertising a
        // different set is rejected with a typed reason.
        let (task_id, record) = proposal_with_capabilities(
            "task-one",
            "worker",
            1,
            '1',
            WorkTaskState::Accepted,
            scope(vec!["src/task.rs"], vec![]),
            None,
            vec![],
            vec!["rust"],
        );
        let (projection, work_state) = project(vec![(task_id, record)], vec![hex('1')], false);
        let request = req(&intent_with_capabilities(
            "task-one",
            "worker",
            1,
            None,
            vec!["ts"],
        ));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::CapabilityMismatch)
        );

        // Exact-set equality: a request advertising a superset also mismatches.
        let request = req(&intent_with_capabilities(
            "task-one",
            "worker",
            1,
            None,
            vec!["rust", "ts"],
        ));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::CapabilityMismatch)
        );

        // A request advertising the exact same set admits.
        let request = req(&intent_with_capabilities(
            "task-one",
            "worker",
            1,
            None,
            vec!["rust"],
        ));
        assert!(resolve_request_admission(&projection, &work_state, &request, "worker").is_ok());
    }

    #[test]
    fn capabilities_flow_into_the_descriptor() {
        let (task_id, record) = proposal_with_capabilities(
            "task-one",
            "worker",
            1,
            '1',
            WorkTaskState::Accepted,
            scope(vec!["src/task.rs"], vec![]),
            None,
            vec![],
            vec!["rust"],
        );
        let (projection, work_state) = project(vec![(task_id, record)], vec![hex('1')], false);
        let request = req(&intent_with_capabilities(
            "task-one",
            "worker",
            1,
            None,
            vec!["rust"],
        ));
        let descriptor = resolve_request_admission(&projection, &work_state, &request, "worker")
            .unwrap()
            .expect("matching capabilities admit");
        assert_eq!(descriptor.capabilities, ["rust"]);
        validate_accepted_work(&descriptor, "worker", &request.message_id).unwrap();

        // Invalid capability format fails descriptor validation.
        let mut malformed = descriptor;
        malformed.capabilities = vec!["Not A Capability".to_string()];
        assert!(validate_accepted_work(&malformed, "worker", &request.message_id).is_err());
    }

    #[test]
    fn newer_terminal_generation_supersedes_older_accepted() {
        // A newer Completed/Blocked/Settled generation supersedes the older
        // accepted intent for the same (task, agent).
        for terminal in [
            WorkTaskState::Completed,
            WorkTaskState::Blocked,
            WorkTaskState::Settled,
        ] {
            let (projection, work_state) = project(
                vec![
                    proposal(
                        "task-one",
                        "worker",
                        1,
                        '1',
                        WorkTaskState::Accepted,
                        scope(vec!["src/task.rs"], vec![]),
                        None,
                        vec![],
                    ),
                    proposal(
                        "task-one",
                        "worker",
                        2,
                        '2',
                        terminal,
                        scope(vec!["src/task.rs"], vec![]),
                        None,
                        vec![],
                    ),
                ],
                vec![hex('1'), hex('2')],
                false,
            );
            let request = req(&intent("task-one", "worker", 1, None));
            assert_eq!(
                resolve_request_admission(&projection, &work_state, &request, "worker"),
                Err(RunnerAdmissionReject::SupersededIntent),
                "newer {terminal:?} generation must supersede older accepted"
            );
        }
    }

    #[test]
    fn newer_rejected_generation_does_not_supersede() {
        // A newer rejected generation never supersedes the older accepted
        // intent.
        let (projection, work_state) = project(
            vec![
                proposal(
                    "task-one",
                    "worker",
                    1,
                    '1',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec![]),
                    None,
                    vec![],
                ),
                proposal(
                    "task-one",
                    "worker",
                    2,
                    '2',
                    WorkTaskState::Rejected,
                    scope(vec!["src/task.rs"], vec![]),
                    None,
                    vec![],
                ),
            ],
            vec![hex('1'), hex('2')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, None));
        assert!(resolve_request_admission(&projection, &work_state, &request, "worker").is_ok());
    }

    #[test]
    fn blocked_dependency_is_unsatisfied() {
        // A blocked dependency never permits dependent work; only Completed
        // does.
        let (projection, work_state) = project(
            vec![
                proposal(
                    "task-one",
                    "worker",
                    1,
                    '1',
                    WorkTaskState::Accepted,
                    scope(vec!["src/task.rs"], vec!["dep-task"]),
                    None,
                    vec![],
                ),
                proposal(
                    "dep-task",
                    "worker",
                    1,
                    '2',
                    WorkTaskState::Blocked,
                    scope(vec!["src/dep.rs"], vec![]),
                    None,
                    vec![],
                ),
            ],
            vec![hex('1'), hex('2')],
            false,
        );
        let request = req(&intent_full(
            "task-one",
            "worker",
            1,
            None,
            vec!["src/task.rs"],
            vec!["dep-task"],
            vec![],
        ));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::UnsettledDependency)
        );
    }

    #[test]
    fn causal_base_satisfied_by_applied_ancestry() {
        // The base is in the authenticated applied ancestry: admitted.
        let (projection, work_state) = project_with_applied(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs"], vec![]),
                Some('9'),
                vec![],
            )],
            vec![hex('1')],
            false,
            vec![hex('9')],
        );
        let request = req(&intent("task-one", "worker", 1, Some('9')));
        assert!(resolve_request_admission(&projection, &work_state, &request, "worker").is_ok());

        // The base is observed in the bounded cache but NOT applied
        // (evidenced): the causal gate still rejects.
        let (projection, work_state) = project(
            vec![proposal(
                "task-one",
                "worker",
                1,
                '1',
                WorkTaskState::Accepted,
                scope(vec!["src/task.rs"], vec![]),
                Some('9'),
                vec![],
            )],
            // seen contains the base, but it was never applied.
            vec![hex('1'), hex('9')],
            false,
        );
        let request = req(&intent("task-one", "worker", 1, Some('9')));
        assert_eq!(
            resolve_request_admission(&projection, &work_state, &request, "worker"),
            Err(RunnerAdmissionReject::UnreachableBase)
        );
    }

    #[test]
    fn valid_acceptance_binds_exact_descriptor() {
        let (projection, work_state) = accepted_project();
        let request = req(&intent("task-one", "worker", 1, None));
        let descriptor = resolve_request_admission(&projection, &work_state, &request, "worker")
            .unwrap()
            .expect("accepted request admits");
        validate_accepted_work(&descriptor, "worker", &request.message_id).unwrap();
        assert_eq!(descriptor.schema_version, ACCEPTED_WORK_SCHEMA_VERSION);
        assert_eq!(descriptor.coordinator.as_deref(), Some("human"));
        assert_eq!(descriptor.causal_base, None);
        // Fingerprint shape is validated; the message binding is enforced at
        // invocation construction (runner.rs tests cover the mismatch).
        let mut malformed = descriptor;
        malformed.message_fingerprint = "not-hex".to_string();
        assert!(validate_accepted_work(&malformed, "worker", &request.message_id).is_err());
    }

    #[test]
    fn fingerprint_is_deterministic_and_binds_body_and_snapshot() {
        let profile = intent("task-one", "worker", 1, None);
        let first = req(&profile);
        let mut second = req(&profile);
        assert_eq!(message_fingerprint(&first), message_fingerprint(&second));
        second.about_snapshot = hex('e');
        assert_ne!(message_fingerprint(&first), message_fingerprint(&second));
        let mut third = req(&profile);
        third.body = "different body".to_string();
        assert_ne!(message_fingerprint(&first), message_fingerprint(&third));
        // Sanity: verification type is only imported for shape coverage.
        let _ = Option::<WorkVerification>::None;
    }
}
