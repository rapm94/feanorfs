//! Unified coordination view (`agent next`) and the edit guard.
//!
//! Reads the existing `ffwork1`, `ffint1`, and `ffres1` projections plus the
//! local conflict registry, then derives one lifecycle per object and the
//! prefilled next actions. Derivation is pure; only the gatherers touch
//! state. Nothing here publishes signals or mutates files.

use crate::ctx::SyncCtx;
use crate::integrator::{active_integrator_status, integrator_offers, offers_from_messages};
use crate::messages::{send_message, signals_since};
use crate::resolution::{resolution_status, ResolutionAssignmentState, ResolutionJobStatus};
use crate::resolution_protocol::{
    resolution_protocol_status, ProtocolAssignmentState, ResolutionProtocolStatus,
};
use anyhow::{bail, Result};
use feanorfs_common::coordination_contract::{
    bounded_text, COORDINATION_MAX_ACTIONS, COORDINATION_MAX_ITEMS, COORDINATION_MAX_WARNINGS,
    GUARD_MAX_PATHS,
};
use feanorfs_common::{
    encode_capability_announcement, normalize_capability, parse_capability_announcement,
    AgentMessage, AgentMessageInput, AgentMessageKind, CapabilityRoster, RosterEntry,
    AGENT_INBOX_MAX_LIMIT,
};
use feanorfs_common::{
    evaluate_scope_overlap, is_safe_rel_path, ConflictKind, CoordinationStatus, GuardFinding,
    GuardResult, GuardVerdict, IntegratorAssignmentState, IntegratorOffer, IntegratorStatusResult,
    LifecycleItem, LifecycleKind, LifecycleStage, NextAction, ResolutionOutcome,
    WorkProposalStatus, WorkStatusInput, WorkStatusResult, WorkTaskState,
    COORDINATION_SCHEMA_VERSION,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Resolves the acting identity: explicit, then `FEANORFS_AGENT`, then
/// `human`.
#[must_use]
pub fn agent_identity(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("FEANORFS_AGENT").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "human".to_string())
}

/// One pending local conflict.
#[derive(Debug, Clone)]
pub struct PendingConflict {
    pub path: String,
    pub kind: ConflictKind,
    /// `None` for legacy path-only records (manual resolution only).
    pub fingerprint: Option<String>,
}

/// Everything the pure derivation reads.
#[derive(Debug, Default)]
pub struct CoordinationInputs {
    pub agent: String,
    pub work: Option<WorkStatusResult>,
    pub dispatch: Option<IntegratorStatusResult>,
    pub offers: Vec<IntegratorOffer>,
    pub jobs: Vec<ResolutionJobStatus>,
    pub protocol: Option<ResolutionProtocolStatus>,
    pub conflicts: Vec<PendingConflict>,
    pub roster: Vec<RosterEntry>,
    /// The agent's live settled snapshot when its controller is idle with no
    /// pending local edits; prefills settle actions.
    pub settled_snapshot: Option<String>,
    pub warnings: Vec<String>,
    pub incomplete: bool,
}

struct Builder<'a> {
    agent: &'a str,
    settled: Option<&'a str>,
    items: Vec<LifecycleItem>,
    actions: Vec<NextAction>,
}

impl Builder<'_> {
    fn item(
        &mut self,
        kind: LifecycleKind,
        id: &str,
        stage: LifecycleStage,
        owner: Option<&str>,
        detail: &str,
    ) {
        self.items.push(LifecycleItem {
            kind,
            id: id.to_string(),
            stage,
            owner: owner.map(str::to_string),
            detail: bounded_text(detail),
        });
    }

    fn action(&mut self, actor: &str, tool: &str, args: Value, cli: String, reason: &str) {
        let Value::Object(args) = args else {
            unreachable!("action args are always objects");
        };
        self.actions.push(NextAction {
            actor: actor.to_string(),
            tool: tool.to_string(),
            args,
            cli,
            reason: bounded_text(reason),
        });
    }

    fn is_me(&self, who: &str) -> bool {
        who == self.agent
    }
}

/// Derives the unified lifecycle and next actions for `inputs.agent`.
#[must_use]
pub fn derive_coordination(inputs: CoordinationInputs) -> CoordinationStatus {
    let mut b = Builder {
        agent: &inputs.agent,
        settled: inputs.settled_snapshot.as_deref(),
        items: Vec::new(),
        actions: Vec::new(),
    };
    if let Some(work) = &inputs.work {
        for task in &work.tasks {
            for proposal in &task.proposals {
                derive_task(&mut b, &task.task_id, proposal);
            }
        }
    }
    let local_jobs: HashSet<&str> = inputs.jobs.iter().map(|job| job.job_id.as_str()).collect();
    for conflict in &inputs.conflicts {
        derive_conflict(&mut b, conflict, &inputs.jobs);
    }
    if let Some(protocol) = &inputs.protocol {
        for entry in &protocol.entries {
            if local_jobs.contains(entry.job_id.as_str()) {
                continue;
            }
            derive_protocol_entry(&mut b, entry);
        }
    }
    let offered: HashSet<&str> = inputs
        .offers
        .iter()
        .map(|offer| offer.assignment_id.as_str())
        .collect();
    let mut warnings = inputs.warnings;
    for offer in &inputs.offers {
        if let Some(warning) = derive_offer(&mut b, offer) {
            warnings.push(warning);
        }
    }
    if let Some(dispatch) = inputs
        .dispatch
        .as_ref()
        .filter(|dispatch| !offered.contains(dispatch.assignment_id.as_str()))
    {
        derive_dispatch(&mut b, dispatch);
    }

    let Builder {
        agent,
        mut items,
        actions,
        ..
    } = b;
    items.sort_by(|a, b| a.stage.cmp(&b.stage).then_with(|| a.id.cmp(&b.id)));
    items.truncate(COORDINATION_MAX_ITEMS);
    let (mut next_actions, others): (Vec<_>, Vec<_>) = actions
        .into_iter()
        .partition(|action| action.actor == agent);
    next_actions.extend(others);
    next_actions.truncate(COORDINATION_MAX_ACTIONS);
    warnings.truncate(COORDINATION_MAX_WARNINGS);
    CoordinationStatus {
        schema_version: COORDINATION_SCHEMA_VERSION,
        agent: agent.to_string(),
        items,
        next_actions,
        warnings: warnings.iter().map(|w| bounded_text(w)).collect(),
        roster: inputs.roster,
        projection_incomplete: inputs.incomplete,
    }
}

fn derive_task(b: &mut Builder<'_>, task_id: &str, p: &WorkProposalStatus) {
    let id = format!("{task_id}:{}", p.agent);
    let next = p.sequence.saturating_add(1);
    match p.state {
        WorkTaskState::Proposed => {
            let coordinator = p.coordinator.as_deref().unwrap_or(&p.agent);
            b.item(
                LifecycleKind::Task,
                &id,
                LifecycleStage::Proposed,
                Some(coordinator),
                &format!(
                    "{} proposed scope; waiting for {coordinator} to decide",
                    p.agent
                ),
            );
            b.action(
                coordinator,
                "work",
                json!({"op": "decide", "proposal_message_id": p.intent_message_id, "kind": "accept"}),
                format!(
                    "feanorfs agent work decide {} --kind accept",
                    p.intent_message_id
                ),
                &format!("decide {}'s proposal for task {task_id} (accept, reject, or narrow)", p.agent),
            );
        }
        WorkTaskState::Accepted => {
            b.item(
                LifecycleKind::Task,
                &id,
                LifecycleStage::Accepted,
                Some(&p.agent),
                &format!("scope accepted: {}", p.accepted_scope.paths.join(", ")),
            );
            if b.is_me(&p.agent) {
                // The live controller's settled snapshot is what was landed;
                // until edits land the agent must not guess an id.
                let snapshot = b.settled.unwrap_or(AWAITING_SNAPSHOT).to_string();
                b.action(
                    &p.agent,
                    "work",
                    json!({
                        "op": "settle",
                        "task_id": task_id,
                        "intent_message_id": p.intent_message_id,
                        "sequence": next,
                        "inspected_snapshot": snapshot,
                        "verification": {"status": "passed", "summary": "<checks you ran>", "applied_message_ids": []}
                    }),
                    format!(
                        "feanorfs agent work settle --task {task_id} --intent {} --sequence {next} --inspected {snapshot} --verification passed --summary '<checks>'",
                        p.intent_message_id
                    ),
                    "edit only inside the accepted scope, let the edits land (agent next --wait), verify, then settle",
                );
            }
        }
        WorkTaskState::Settled => {
            b.item(
                LifecycleKind::Task,
                &id,
                LifecycleStage::Settled,
                Some(&p.agent),
                "verified; waiting for completion",
            );
            if b.is_me(&p.agent) {
                b.action(
                    &p.agent,
                    "work",
                    json!({
                        "op": "complete",
                        "task_id": task_id,
                        "intent_message_id": p.intent_message_id,
                        "sequence": next,
                        "outcome": "<one-line outcome>"
                    }),
                    format!(
                        "feanorfs agent work complete --task {task_id} --intent {} --sequence {next} --outcome '<outcome>'",
                        p.intent_message_id
                    ),
                    "mark the settled task complete",
                );
            }
        }
        WorkTaskState::Completed => b.item(
            LifecycleKind::Task,
            &id,
            LifecycleStage::Done,
            None,
            p.outcome.as_deref().unwrap_or("completed"),
        ),
        WorkTaskState::Blocked | WorkTaskState::Rejected => b.item(
            LifecycleKind::Task,
            &id,
            LifecycleStage::Blocked,
            None,
            p.reason.as_deref().unwrap_or(p.state.as_str()),
        ),
        WorkTaskState::Yielded => b.item(
            LifecycleKind::Task,
            &id,
            LifecycleStage::Stopped,
            None,
            "yielded",
        ),
    }
}

fn manual_keep_action(b: &mut Builder<'_>, actor: &str, path: &str, reason: &str) {
    b.action(
        actor,
        "conflicts",
        json!({"op": "keep", "path": path, "keep": "<local|cloud|both|file>"}),
        format!("feanorfs conflicts keep {path} --local|--cloud|--both|--file <path>"),
        reason,
    );
}

fn derive_conflict(b: &mut Builder<'_>, conflict: &PendingConflict, jobs: &[ResolutionJobStatus]) {
    let path = conflict.path.as_str();
    let kind = conflict.kind.as_db_str();
    let job = conflict.fingerprint.as_deref().and_then(|fingerprint| {
        jobs.iter()
            .filter(|job| job.conflict_fingerprint == fingerprint)
            .max_by_key(|job| (!job.assignment_state.is_terminal(), job.created_at_ms))
    });
    let Some(job) = job else {
        b.item(
            LifecycleKind::Conflict,
            path,
            LifecycleStage::Conflicted,
            None,
            &format!("{kind} conflict; no resolver assigned"),
        );
        if conflict.fingerprint.is_some() {
            let agent = b.agent.to_string();
            b.action(
                &agent,
                "resolve",
                json!({"op": "prepare", "path": path, "prevention": {"type": "exhausted", "detail": "<why scoping could not prevent this>"}}),
                format!("feanorfs agent resolution prepare {path} --reason exhausted --detail '<why>'"),
                "prepare an exact resolution job; the engine designates the owner",
            );
        } else {
            let agent = b.agent.to_string();
            manual_keep_action(
                b,
                &agent,
                path,
                "legacy conflict: edit the file, then keep one version",
            );
        }
        return;
    };
    let owner = job.owner.as_str();
    let job_id = job.job_id.as_str();
    match (job.assignment_state, job.outcome) {
        (ResolutionAssignmentState::Active, None) => {
            b.item(
                LifecycleKind::Conflict,
                path,
                LifecycleStage::Assigned,
                Some(owner),
                &format!("job {job_id} assigned to {owner}"),
            );
            b.action(
                owner,
                "resolve",
                json!({"op": "materialize", "job_id": job_id}),
                format!("feanorfs agent resolution materialize {job_id}"),
                "materialize legs, write the candidate (op=put), then submit (op=submit)",
            );
        }
        (
            ResolutionAssignmentState::Active,
            Some(ResolutionOutcome::CandidateReady | ResolutionOutcome::NoChangeRequired),
        ) => {
            b.item(
                LifecycleKind::Conflict,
                path,
                LifecycleStage::Resolving,
                Some(owner),
                "verified candidate ready to apply",
            );
            b.action(
                owner,
                "resolve",
                json!({"op": "apply", "job_id": job_id}),
                format!("feanorfs agent resolution apply {job_id}"),
                "apply the submitted candidate with guarded publication",
            );
        }
        (ResolutionAssignmentState::Active, Some(ResolutionOutcome::RequiresHuman)) => {
            b.item(
                LifecycleKind::Conflict,
                path,
                LifecycleStage::AwaitingHuman,
                Some("human"),
                "resolver escalated one question",
            );
            b.action(
                "human",
                "resolve",
                json!({"op": "answer", "job_id": job_id, "option": "<defer|keep_unresolved|submit_candidate>"}),
                format!("feanorfs agent resolution review {job_id}"),
                "review the question and answer it",
            );
        }
        (ResolutionAssignmentState::Active, Some(_)) => {
            b.item(
                LifecycleKind::Conflict,
                path,
                LifecycleStage::Blocked,
                Some("human"),
                "resolver could not produce a candidate",
            );
            manual_keep_action(
                b,
                "human",
                path,
                "automatic resolution failed; choose a version",
            );
        }
        (ResolutionAssignmentState::PublicationUncertain, _) => b.item(
            LifecycleKind::Conflict,
            path,
            LifecycleStage::Resolving,
            Some(owner),
            "publication in flight; the next status read recovers it",
        ),
        (ResolutionAssignmentState::Completed, _) => b.item(
            LifecycleKind::Conflict,
            path,
            LifecycleStage::Done,
            None,
            "resolved",
        ),
        (state, _) => {
            b.item(
                LifecycleKind::Conflict,
                path,
                LifecycleStage::Stopped,
                Some("human"),
                &format!("resolution job {state:?}; conflict still pending"),
            );
            manual_keep_action(b, "human", path, "no active resolver; choose a version");
        }
    }
}

fn derive_protocol_entry(
    b: &mut Builder<'_>,
    entry: &crate::resolution_protocol::ResolutionProtocolEntryStatus,
) {
    let id = format!(
        "fingerprint:{}",
        &entry.conflict_fingerprint[..entry.conflict_fingerprint.len().min(16)]
    );
    let job_id = entry.job_id.as_str();
    match entry.state {
        ProtocolAssignmentState::Assigned => {
            b.item(
                LifecycleKind::Conflict,
                &id,
                LifecycleStage::Assigned,
                Some(&entry.owner),
                &format!("remote job {job_id} assigned to {}", entry.owner),
            );
            if b.is_me(&entry.owner) {
                let owner = entry.owner.clone();
                b.action(
                    &owner,
                    "resolve",
                    json!({"op": "materialize", "job_id": job_id}),
                    format!("feanorfs agent resolution materialize {job_id}"),
                    "you were designated resolver on this machine",
                );
            }
        }
        ProtocolAssignmentState::ResultReceived
            if entry.outcome == Some(ResolutionOutcome::RequiresHuman) =>
        {
            let question = entry.question.as_deref().unwrap_or("resolver escalated");
            b.item(
                LifecycleKind::Conflict,
                &id,
                LifecycleStage::AwaitingHuman,
                Some("human"),
                question,
            );
            b.action(
                "human",
                "resolve",
                json!({"op": "publish_answer", "job_id": job_id, "option": "<defer|keep_unresolved|submit_candidate>"}),
                format!("feanorfs agent resolution review {job_id}"),
                question,
            );
        }
        ProtocolAssignmentState::ResultReceived | ProtocolAssignmentState::HumanAnswered => b.item(
            LifecycleKind::Conflict,
            &id,
            LifecycleStage::Resolving,
            Some(&entry.owner),
            &format!("remote job {job_id} {}", entry.state.as_str()),
        ),
        ProtocolAssignmentState::Revoked => b.item(
            LifecycleKind::Conflict,
            &id,
            LifecycleStage::Stopped,
            None,
            "revoked",
        ),
    }
}

fn derive_offer(b: &mut Builder<'_>, offer: &IntegratorOffer) -> Option<String> {
    let id = offer.assignment_id.as_str();
    if let Some((attempt, selected)) = &offer.superseded_by {
        let stage = if offer.terminal {
            LifecycleStage::Done
        } else {
            LifecycleStage::Stopped
        };
        b.item(
            LifecycleKind::Integration,
            id,
            stage,
            Some(selected),
            &format!("superseded by attempt {attempt} ({selected})"),
        );
        return (!offer.terminal && offer.accepted).then(|| {
            format!(
                "stop integrating {id}: attempt {} was superseded by {selected}",
                offer.attempt
            )
        });
    }
    if offer.terminal {
        b.item(
            LifecycleKind::Integration,
            id,
            LifecycleStage::Done,
            None,
            "result sent",
        );
        return None;
    }
    let agent = b.agent.to_string();
    if offer.accepted {
        b.item(
            LifecycleKind::Integration,
            id,
            LifecycleStage::Resolving,
            Some(&agent),
            &offer.task,
        );
        b.action(
            &agent,
            "integrator",
            json!({"op": "reply", "kind": "result", "assignment_id": id, "outcome": "<summary>", "verification": {"status": "passed", "summary": "<checks you ran>"}}),
            format!("feanorfs agent integrator reply result --assignment {id} --outcome '<summary>' --verification passed --summary '<checks>'"),
            "integrate the batch, verify, then send one result (or kind=blocked)",
        );
    } else {
        b.item(
            LifecycleKind::Integration,
            id,
            LifecycleStage::Assigned,
            Some(&agent),
            &offer.task,
        );
        b.action(
            &agent,
            "integrator",
            json!({"op": "reply", "kind": "accept", "assignment_id": id}),
            format!("feanorfs agent integrator reply accept --assignment {id}"),
            &format!(
                "{} offered you the integrator role: {}",
                offer.dispatcher, offer.task
            ),
        );
    }
    None
}

fn derive_dispatch(b: &mut Builder<'_>, dispatch: &IntegratorStatusResult) {
    let id = dispatch.assignment_id.as_str();
    let selected = dispatch.selected.as_deref();
    match dispatch.state {
        IntegratorAssignmentState::Created | IntegratorAssignmentState::Offered => {
            b.item(
                LifecycleKind::Integration,
                id,
                LifecycleStage::Assigned,
                selected,
                "offer sent; waiting for acceptance",
            );
            b.action(
                "human",
                "integrator",
                json!({"op": "resume"}),
                "feanorfs agent integrator resume".to_string(),
                "observe integrator replies",
            );
        }
        IntegratorAssignmentState::Accepted => {
            b.item(
                LifecycleKind::Integration,
                id,
                LifecycleStage::Resolving,
                selected,
                "integrator working",
            );
            b.action(
                "human",
                "integrator",
                json!({"op": "resume"}),
                "feanorfs agent integrator resume".to_string(),
                "observe the integrator result",
            );
        }
        IntegratorAssignmentState::RequiresHuman => {
            let question = dispatch
                .digest
                .as_ref()
                .and_then(|digest| digest.decision_required.as_deref())
                .unwrap_or("integrator needs a decision");
            b.item(
                LifecycleKind::Integration,
                id,
                LifecycleStage::AwaitingHuman,
                Some("human"),
                question,
            );
            b.action(
                "human",
                "integrator",
                json!({"op": "status", "assignment_id": id}),
                format!("feanorfs agent integrator status {id}"),
                question,
            );
        }
        IntegratorAssignmentState::Completed => b.item(
            LifecycleKind::Integration,
            id,
            LifecycleStage::Done,
            None,
            "integration completed",
        ),
        IntegratorAssignmentState::Blocked => b.item(
            LifecycleKind::Integration,
            id,
            LifecycleStage::Blocked,
            Some("human"),
            "integrator blocked",
        ),
        IntegratorAssignmentState::Revoked | IntegratorAssignmentState::Cancelled => {
            b.item(
                LifecycleKind::Integration,
                id,
                LifecycleStage::Stopped,
                None,
                "revoked or cancelled",
            );
        }
    }
}

/// Evaluates the edit guard for `paths` (already canonical and relative).
#[must_use]
pub fn evaluate_guard(
    agent: &str,
    paths: &[String],
    require_scope: bool,
    work: Option<&WorkStatusResult>,
    conflicts: &HashSet<String>,
    offers: &[IntegratorOffer],
    incomplete: bool,
) -> GuardResult {
    let proposals: Vec<(&str, &WorkProposalStatus)> = work
        .map(|work| {
            work.tasks
                .iter()
                .flat_map(|task| {
                    task.proposals
                        .iter()
                        .map(move |p| (task.task_id.as_str(), p))
                })
                .collect()
        })
        .unwrap_or_default();
    let live = |p: &WorkProposalStatus| {
        matches!(p.state, WorkTaskState::Accepted | WorkTaskState::Settled)
    };
    let mine: Vec<&WorkProposalStatus> = proposals
        .iter()
        .filter(|(_, p)| p.agent == agent && live(p))
        .map(|(_, p)| *p)
        .collect();
    let superseded = offers
        .iter()
        .find(|offer| offer.accepted && !offer.terminal && offer.superseded_by.is_some());
    let mut findings = Vec::new();
    if incomplete {
        findings.push(GuardFinding {
            path: "*".to_string(),
            verdict: GuardVerdict::Warn,
            reason: "coordination projection is incomplete; overlaps cannot be proven absent"
                .to_string(),
        });
    }
    for path in paths {
        let mut verdict = GuardVerdict::Allow;
        let mut reasons: Vec<String> = Vec::new();
        let mut raise = |level: GuardVerdict, reason: String| {
            verdict = verdict.max(level);
            reasons.push(reason);
        };
        if let Some(offer) = superseded {
            let (attempt, selected) = offer.superseded_by.as_ref().expect("filtered");
            raise(
                GuardVerdict::Deny,
                format!(
                    "your integrator attempt on {} was superseded by attempt {attempt} ({selected}); stop editing",
                    offer.assignment_id
                ),
            );
        }
        if conflicts.contains(path) {
            raise(
                GuardVerdict::Warn,
                "path has a pending conflict; after editing, record the choice with `feanorfs conflicts keep`".to_string(),
            );
        }
        let one = std::slice::from_ref(path);
        for (task_id, other) in proposals.iter().filter(|(_, p)| p.agent != agent) {
            let overlap = evaluate_scope_overlap(one, &[], &other.accepted_scope.paths, &[]);
            if overlap.is_empty() {
                continue;
            }
            let accepted_overlap = mine.iter().any(|p| {
                p.accepted_overlap.iter().any(|accepted| {
                    accepted.path_a.as_deref() == Some(path.as_str())
                        || accepted.path_b.as_deref() == Some(path.as_str())
                        || overlap
                            .iter()
                            .any(|o| accepted.path_a == o.path_b || accepted.path_b == o.path_b)
                })
            });
            if live(other) && !accepted_overlap {
                raise(
                    GuardVerdict::Deny,
                    format!(
                        "inside {}'s accepted scope for task {task_id}; propose overlap or ask the coordinator",
                        other.agent
                    ),
                );
            } else if other.state == WorkTaskState::Proposed {
                raise(
                    GuardVerdict::Warn,
                    format!("{} proposed this path for task {task_id}", other.agent),
                );
            }
        }
        if require_scope
            && !mine.is_empty()
            && !mine
                .iter()
                .any(|p| !evaluate_scope_overlap(one, &[], &p.accepted_scope.paths, &[]).is_empty())
        {
            raise(
                GuardVerdict::Deny,
                "outside your accepted scope; amend the scope first (`feanorfs agent work amend`)"
                    .to_string(),
            );
        }
        if verdict != GuardVerdict::Allow {
            findings.push(GuardFinding {
                path: path.clone(),
                verdict,
                reason: bounded_text(&reasons.join("; ")),
            });
        }
    }
    GuardResult {
        schema_version: COORDINATION_SCHEMA_VERSION,
        agent: agent.to_string(),
        verdict: findings
            .iter()
            .map(|finding| finding.verdict)
            .max()
            .unwrap_or(GuardVerdict::Allow),
        findings,
    }
}

async fn pending_conflicts(ctx: &SyncCtx<'_>) -> Result<Vec<PendingConflict>> {
    let fingerprints = ctx.db.pending_conflict_fingerprints().await?;
    Ok(ctx
        .db
        .list_conflict_records()
        .await?
        .into_iter()
        .map(|record| PendingConflict {
            fingerprint: fingerprints.get(&record.path).cloned(),
            path: record.path,
            kind: record.kind,
        })
        .collect())
}

fn note<T>(
    result: Result<T>,
    label: &str,
    warnings: &mut Vec<String>,
    incomplete: &mut bool,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(format!("{label} unavailable: {error:#}"));
            *incomplete = true;
            None
        }
    }
}

/// Reads every coordination projection and derives the unified lifecycle.
/// Source failures become warnings plus `projection_incomplete` instead of
/// hiding the remaining projections.
///
/// # Errors
/// Returns an error only for an invalid agent identity.
pub async fn coordination_status(ctx: &SyncCtx<'_>, agent: &str) -> Result<CoordinationStatus> {
    crate::paths::validate_name(agent)?;
    let mut warnings = Vec::new();
    let mut incomplete = false;
    let work = note(
        crate::work::work_status(ctx, WorkStatusInput::default()).await,
        "work-intent projection",
        &mut warnings,
        &mut incomplete,
    );
    incomplete |= work.as_ref().is_some_and(|work| work.projection_incomplete);
    let dispatch = note(
        active_integrator_status(ctx).await,
        "integrator dispatcher state",
        &mut warnings,
        &mut incomplete,
    )
    .flatten();
    let signals = note(
        signals_since(ctx, None, AGENT_INBOX_MAX_LIMIT).await,
        "signal history",
        &mut warnings,
        &mut incomplete,
    );
    incomplete |= signals.as_ref().is_some_and(|signals| signals.cursor_reset);
    let messages = signals.map(|signals| signals.messages).unwrap_or_default();
    let offers = offers_from_messages(agent, &messages);
    let roster = roster_from(&messages, work.as_ref());
    let jobs = note(
        resolution_status(ctx, None).await,
        "resolution jobs",
        &mut warnings,
        &mut incomplete,
    )
    .map(|projection| projection.jobs)
    .unwrap_or_default();
    let protocol = note(
        resolution_protocol_status(ctx, false).await,
        "resolution protocol",
        &mut warnings,
        &mut incomplete,
    );
    incomplete |= protocol.as_ref().is_some_and(|p| p.projection_incomplete);
    let conflicts = note(
        pending_conflicts(ctx).await,
        "conflict registry",
        &mut warnings,
        &mut incomplete,
    )
    .unwrap_or_default();
    let settled_snapshot = crate::agent::continuous::read_continuous_status(ctx.base, agent)
        .ok()
        .flatten()
        .filter(|live| live.phase == feanorfs_common::ContinuousPhase::Idle && !live.pending_local)
        .and_then(|live| live.settled_snapshot);
    Ok(derive_coordination(CoordinationInputs {
        agent: agent.to_string(),
        work,
        dispatch,
        offers,
        jobs,
        protocol,
        conflicts,
        roster,
        settled_snapshot,
        warnings,
        incomplete,
    }))
}

/// Upper bound for one blocking wait.
pub const COORDINATION_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Placeholder a settle action carries until the agent's edits land.
const AWAITING_SNAPSHOT: &str = "<snapshot you verified>";

/// Whether `agent` has an action it can take now (not one that still waits
/// for its own edits to land).
fn has_ready_action(status: &CoordinationStatus) -> bool {
    status.next_actions.iter().any(|action| {
        action.actor == status.agent
            && action
                .args
                .get("inspected_snapshot")
                .and_then(Value::as_str)
                != Some(AWAITING_SNAPSHOT)
    })
}

/// Like [`coordination_status`], but blocks up to `wait` while `agent` has
/// nothing ready: it returns at once when an own action is ready, and
/// otherwise as soon as its own actions change (a decision arrives, edits
/// land). Agents wait inside one call instead of polling turn by turn.
///
/// # Errors
/// Returns an error for an invalid agent identity.
pub async fn coordination_status_wait(
    ctx: &SyncCtx<'_>,
    agent: &str,
    wait: std::time::Duration,
) -> Result<CoordinationStatus> {
    let own = |status: &CoordinationStatus| -> Vec<NextAction> {
        status
            .next_actions
            .iter()
            .filter(|action| action.actor == status.agent)
            .cloned()
            .collect()
    };
    let deadline = tokio::time::Instant::now() + wait.min(COORDINATION_MAX_WAIT);
    let mut latest = coordination_status(ctx, agent).await?;
    let initial = own(&latest);
    while !has_ready_action(&latest)
        && own(&latest) == initial
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        latest = coordination_status(ctx, agent).await?;
    }
    Ok(latest)
}

/// Capability roster: the newest `ffcap1` announcement per sender plus the
/// capabilities each agent advertised in its work intents.
#[must_use]
pub fn roster_from(messages: &[AgentMessage], work: Option<&WorkStatusResult>) -> Vec<RosterEntry> {
    let mut roster: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut announced: HashSet<&str> = HashSet::new();
    // Signal windows are newest first, so the first announcement wins.
    for message in messages {
        if message.kind != AgentMessageKind::Status || announced.contains(message.from.as_str()) {
            continue;
        }
        if let Some(capabilities) = parse_capability_announcement(&message.body) {
            announced.insert(&message.from);
            roster
                .entry(message.from.clone())
                .or_default()
                .extend(capabilities);
        }
    }
    for proposal in work
        .iter()
        .flat_map(|work| work.tasks.iter())
        .flat_map(|task| task.proposals.iter())
        .filter(|proposal| !proposal.capabilities.is_empty())
    {
        roster
            .entry(proposal.agent.clone())
            .or_default()
            .extend(proposal.capabilities.iter().cloned());
    }
    roster
        .into_iter()
        .map(|(agent, capabilities)| RosterEntry {
            agent,
            capabilities: capabilities.into_iter().collect(),
        })
        .collect()
}

/// Reads the capability roster.
///
/// # Errors
/// Returns an error for unreadable history.
pub async fn capability_roster(ctx: &SyncCtx<'_>) -> Result<CapabilityRoster> {
    let signals = signals_since(ctx, None, AGENT_INBOX_MAX_LIMIT).await?;
    let work = crate::work::work_status(ctx, WorkStatusInput::default()).await;
    let incomplete = signals.cursor_reset
        || work
            .as_ref()
            .map_or(true, |work| work.projection_incomplete);
    Ok(CapabilityRoster {
        announced: None,
        agents: roster_from(&signals.messages, work.as_ref().ok()),
        projection_incomplete: incomplete,
    })
}

/// Announces `agent`'s complete capability set (when given) as a broadcast
/// `ffcap1` status signal, then returns the roster.
///
/// # Errors
/// Returns an error for invalid names or capabilities, or failed publication.
pub async fn capabilities(
    ctx: &SyncCtx<'_>,
    agent: &str,
    announce: Option<Vec<String>>,
) -> Result<CapabilityRoster> {
    crate::paths::validate_name(agent)?;
    let announced = match announce {
        Some(capabilities) => Some(
            send_message(
                ctx,
                AgentMessageInput {
                    to: "*".to_string(),
                    kind: AgentMessageKind::Status,
                    body: encode_capability_announcement(&capabilities)?,
                    about_snapshot: None,
                    reply_to: None,
                    from: Some(agent.to_string()),
                },
            )
            .await?
            .message_id,
        ),
        None => None,
    };
    let mut roster = capability_roster(ctx).await?;
    roster.announced = announced;
    Ok(roster)
}

/// Resolves a `cap:<capability>` recipient to the single agent advertising
/// it. Several capable agents is an explicit routing decision, not a guess.
///
/// # Errors
/// Returns an error when zero or several agents advertise the capability.
pub async fn resolve_capability(ctx: &SyncCtx<'_>, capability: &str) -> Result<String> {
    let capability = normalize_capability(capability)?;
    let roster = capability_roster(ctx).await?;
    let capable: Vec<&str> = roster
        .agents
        .iter()
        .filter(|entry| entry.capabilities.contains(&capability))
        .map(|entry| entry.agent.as_str())
        .collect();
    match capable.as_slice() {
        [agent] => Ok((*agent).to_string()),
        [] => bail!(
            "no agent advertises capability `{capability}`; announce it on the capable machine with `feanorfs agent capabilities --set {capability}`"
        ),
        many => bail!(
            "{} agents advertise `{capability}` ({}); address one directly or let `feanorfs agent integrator assign --require {capability}` choose fairly",
            many.len(),
            many.join(", ")
        ),
    }
}

/// Evaluates whether `agent` may write `paths` right now.
///
/// # Errors
/// Returns an error for invalid identities, too many paths, or
/// non-canonical paths.
pub async fn guard_paths(
    ctx: &SyncCtx<'_>,
    agent: &str,
    paths: &[String],
    require_scope: bool,
) -> Result<GuardResult> {
    crate::paths::validate_name(agent)?;
    anyhow::ensure!(
        paths.len() <= GUARD_MAX_PATHS,
        "guard accepts at most {GUARD_MAX_PATHS} paths"
    );
    for path in paths {
        anyhow::ensure!(
            is_safe_rel_path(path),
            "guard path '{path}' is not a canonical workspace-relative path"
        );
    }
    let mut ignored = Vec::new();
    let mut incomplete = false;
    let work = note(
        crate::work::work_status(ctx, WorkStatusInput::default()).await,
        "work-intent projection",
        &mut ignored,
        &mut incomplete,
    );
    incomplete |= work.as_ref().is_some_and(|work| work.projection_incomplete);
    let offers = match note(
        integrator_offers(ctx, agent).await,
        "integrator offers",
        &mut ignored,
        &mut incomplete,
    ) {
        Some((offers, truncated)) => {
            incomplete |= truncated;
            offers
        }
        None => Vec::new(),
    };
    let conflicts: HashSet<String> = ctx
        .db
        .list_pending_conflict_paths()
        .await?
        .into_iter()
        .collect();
    Ok(evaluate_guard(
        agent,
        paths,
        require_scope,
        work.as_ref(),
        &conflicts,
        &offers,
        incomplete,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{WorkScope, WorkTaskStatus};

    fn proposal(agent: &str, state: WorkTaskState, paths: &[&str]) -> WorkProposalStatus {
        WorkProposalStatus {
            agent: agent.to_string(),
            state,
            sequence: 1,
            intent_message_id: "a".repeat(64),
            coordinator: Some("human".to_string()),
            accepted_scope: WorkScope {
                paths: paths.iter().map(|p| (*p).to_string()).collect(),
                ..WorkScope::default()
            },
            capabilities: vec![],
            decision: None,
            accepted_overlap: vec![],
            amendments: vec![],
            causal_refs: vec![],
            inspected_snapshot: None,
            verification: None,
            outcome: None,
            reason: None,
            source_message_id: "a".repeat(64),
            updated_at_ms: 0,
        }
    }

    fn work(proposals: Vec<WorkProposalStatus>) -> WorkStatusResult {
        WorkStatusResult {
            cursor: String::new(),
            cursor_reset: false,
            projection_incomplete: false,
            messages_processed: 0,
            tasks: vec![WorkTaskStatus {
                task_id: "parser".to_string(),
                state: WorkTaskState::Accepted,
                proposals,
            }],
            evidence_count: 0,
            dropped_count: 0,
            updated_at_ms: 0,
            applied_message_ids: vec![],
        }
    }

    #[test]
    fn coordinator_gets_decide_action_first_and_author_waits() {
        let status = derive_coordination(CoordinationInputs {
            agent: "human".to_string(),
            work: Some(work(vec![proposal(
                "linux",
                WorkTaskState::Proposed,
                &["src/a.rs"],
            )])),
            ..CoordinationInputs::default()
        });
        assert_eq!(status.items[0].stage, LifecycleStage::Proposed);
        assert_eq!(status.next_actions[0].actor, "human");
        assert_eq!(status.next_actions[0].args["op"], "decide");

        let author = derive_coordination(CoordinationInputs {
            agent: "linux".to_string(),
            work: Some(work(vec![proposal(
                "linux",
                WorkTaskState::Proposed,
                &["src/a.rs"],
            )])),
            ..CoordinationInputs::default()
        });
        assert!(author
            .next_actions
            .iter()
            .all(|action| action.actor != "linux"));
    }

    #[test]
    fn accepted_author_is_told_to_settle_with_bound_ids() {
        let status = derive_coordination(CoordinationInputs {
            agent: "linux".to_string(),
            work: Some(work(vec![proposal(
                "linux",
                WorkTaskState::Accepted,
                &["src/a.rs"],
            )])),
            ..CoordinationInputs::default()
        });
        let action = &status.next_actions[0];
        assert_eq!(action.args["op"], "settle");
        assert_eq!(action.args["sequence"], 2);
        assert_eq!(action.args["intent_message_id"], "a".repeat(64));
    }

    #[test]
    fn settle_action_uses_the_live_settled_snapshot_once_edits_land() {
        let accepted = || {
            work(vec![proposal(
                "linux",
                WorkTaskState::Accepted,
                &["src/a.rs"],
            )])
        };
        let pending = derive_coordination(CoordinationInputs {
            agent: "linux".to_string(),
            work: Some(accepted()),
            ..CoordinationInputs::default()
        });
        assert_eq!(
            pending.next_actions[0].args["inspected_snapshot"],
            "<snapshot you verified>"
        );
        let landed = derive_coordination(CoordinationInputs {
            agent: "linux".to_string(),
            work: Some(accepted()),
            settled_snapshot: Some("e".repeat(64)),
            ..CoordinationInputs::default()
        });
        assert_eq!(
            landed.next_actions[0].args["inspected_snapshot"],
            "e".repeat(64)
        );
        assert!(landed.next_actions[0].cli.contains(&"e".repeat(64)));
    }

    #[test]
    fn only_settle_actions_awaiting_a_snapshot_are_not_ready() {
        let accepted = || {
            work(vec![proposal(
                "linux",
                WorkTaskState::Accepted,
                &["src/a.rs"],
            )])
        };
        let waiting = derive_coordination(CoordinationInputs {
            agent: "linux".to_string(),
            work: Some(accepted()),
            ..CoordinationInputs::default()
        });
        assert!(!has_ready_action(&waiting));
        let landed = derive_coordination(CoordinationInputs {
            agent: "linux".to_string(),
            work: Some(accepted()),
            settled_snapshot: Some("e".repeat(64)),
            ..CoordinationInputs::default()
        });
        assert!(has_ready_action(&landed));
        let coordinator = derive_coordination(CoordinationInputs {
            agent: "human".to_string(),
            work: Some(work(vec![proposal(
                "linux",
                WorkTaskState::Proposed,
                &["src/a.rs"],
            )])),
            ..CoordinationInputs::default()
        });
        assert!(has_ready_action(&coordinator));
    }

    #[test]
    fn unassigned_fingerprinted_conflict_prepares_and_legacy_keeps() {
        let status = derive_coordination(CoordinationInputs {
            agent: "mac".to_string(),
            conflicts: vec![
                PendingConflict {
                    path: "a.rs".into(),
                    kind: ConflictKind::EditEdit,
                    fingerprint: Some("f".repeat(64)),
                },
                PendingConflict {
                    path: "b.rs".into(),
                    kind: ConflictKind::EditDelete,
                    fingerprint: None,
                },
            ],
            ..CoordinationInputs::default()
        });
        assert!(status
            .items
            .iter()
            .all(|item| item.stage == LifecycleStage::Conflicted));
        let ops: Vec<&str> = status
            .next_actions
            .iter()
            .map(|a| a.args["op"].as_str().unwrap())
            .collect();
        assert_eq!(ops, vec!["prepare", "keep"]);
    }

    #[test]
    fn superseded_accepted_offer_warns_and_guard_denies() {
        let offer = IntegratorOffer {
            assignment_id: "0".repeat(32),
            attempt: 0,
            dispatcher: "human".into(),
            about_snapshot: "b".repeat(64),
            request_message_id: "c".repeat(64),
            task: "batch".into(),
            accepted: true,
            terminal: false,
            superseded_by: Some((1, "linux".into())),
        };
        let status = derive_coordination(CoordinationInputs {
            agent: "mac".to_string(),
            offers: vec![offer.clone()],
            ..CoordinationInputs::default()
        });
        assert_eq!(status.items[0].stage, LifecycleStage::Stopped);
        assert_eq!(status.warnings.len(), 1);
        let guard = evaluate_guard(
            "mac",
            &["src/a.rs".into()],
            false,
            None,
            &HashSet::new(),
            &[offer],
            false,
        );
        assert_eq!(guard.verdict, GuardVerdict::Deny);
    }

    #[test]
    fn guard_denies_other_accepted_scope_and_warns_on_proposals_and_conflicts() {
        let projection = work(vec![
            proposal("linux", WorkTaskState::Accepted, &["src/**"]),
            proposal("mac", WorkTaskState::Proposed, &["ios/App.swift"]),
        ]);
        let conflicts = HashSet::from(["docs/a.md".to_string()]);
        let guard = evaluate_guard(
            "codex",
            &[
                "src/lib.rs".into(),
                "ios/App.swift".into(),
                "docs/a.md".into(),
                "README.md".into(),
            ],
            false,
            Some(&projection),
            &conflicts,
            &[],
            false,
        );
        let verdicts: Vec<_> = guard
            .findings
            .iter()
            .map(|f| (f.path.as_str(), f.verdict))
            .collect();
        assert_eq!(
            verdicts,
            vec![
                ("src/lib.rs", GuardVerdict::Deny),
                ("ios/App.swift", GuardVerdict::Warn),
                ("docs/a.md", GuardVerdict::Warn),
            ]
        );
        assert_eq!(guard.verdict, GuardVerdict::Deny);
        let own = evaluate_guard(
            "linux",
            &["src/lib.rs".into()],
            true,
            Some(&projection),
            &HashSet::new(),
            &[],
            false,
        );
        assert_eq!(own.verdict, GuardVerdict::Allow);
        let outside = evaluate_guard(
            "linux",
            &["README.md".into()],
            true,
            Some(&projection),
            &HashSet::new(),
            &[],
            false,
        );
        assert_eq!(outside.verdict, GuardVerdict::Deny);
    }
}
