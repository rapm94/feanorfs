//! One-call coordination so agents spend turns on work, not protocol.
//!
//! `claim_scope` proposes scope and waits for the decision; `finish_work`
//! waits for the agent's edits to land, then settles and completes every
//! task it holds; `coordinate_pass` is the automatic coordinator that
//! accepts proposals whose scope overlaps no other agent's live scope.
//! Everything is built from the existing `ffwork1` operations.

use crate::agent::continuous::live_continuous_status;
use crate::ctx::SyncCtx;
use crate::work::{work_complete, work_decide, work_propose, work_settle, work_status};
use anyhow::{bail, ensure, Result};
use feanorfs_common::coordination_contract::COORDINATION_MAX_WAIT_SECONDS;
use feanorfs_common::{
    is_valid_scope_entry, ClaimOutcome, ClaimResult, ContinuousPhase, CoordinatePass, DoneResult,
    WorkCompleteInput, WorkDecideInput, WorkDecisionAccept, WorkDecisionKind, WorkProposalStatus,
    WorkProposeInput, WorkSettleInput, WorkStatusInput, WorkStatusResult, WorkTaskState,
    WorkVerification, WorkVerificationStatus,
};
use std::time::Duration;
use tokio::time::Instant;

const POLL: Duration = Duration::from_millis(500);
/// Longest a claim waits for the worktree to catch up after acceptance.
const FRESH_WORKTREE_WAIT: Duration = Duration::from_secs(30);

/// Converts an optional seconds bound into a capped wait (default 300 s).
#[must_use]
pub fn bounded_wait(seconds: Option<u64>) -> Duration {
    Duration::from_secs(seconds.unwrap_or(300).min(COORDINATION_MAX_WAIT_SECONDS))
}

fn proposals(work: &WorkStatusResult) -> impl Iterator<Item = (&str, &WorkProposalStatus)> {
    work.tasks.iter().flat_map(|task| {
        task.proposals
            .iter()
            .map(move |proposal| (task.task_id.as_str(), proposal))
    })
}

fn is_live(proposal: &WorkProposalStatus) -> bool {
    matches!(
        proposal.state,
        WorkTaskState::Accepted | WorkTaskState::Settled
    )
}

/// Whether a scope entry (exact path or `dir/**`) covers `path`.
fn covers(scope: &str, path: &str) -> bool {
    scope == path
        || scope
            .strip_suffix("**")
            .is_some_and(|prefix| prefix.ends_with('/') && path.starts_with(prefix))
}

fn overlaps(a: &[String], b: &[String]) -> bool {
    a.iter()
        .any(|x| b.iter().any(|y| covers(x, y) || covers(y, x)))
}

/// Whether every path is already inside `agent`'s live accepted scope.
#[must_use]
pub fn claim_covered(work: &WorkStatusResult, agent: &str, paths: &[String]) -> bool {
    paths.iter().all(|path| {
        proposals(work).any(|(_, proposal)| {
            proposal.agent == agent
                && is_live(proposal)
                && proposal
                    .accepted_scope
                    .paths
                    .iter()
                    .any(|scope| covers(scope, path))
        })
    })
}

/// Other agents' live scopes that overlap `paths`, as `agent (task)`.
fn blockers(work: &WorkStatusResult, agent: &str, paths: &[String]) -> Vec<String> {
    proposals(work)
        .filter(|(_, proposal)| {
            proposal.agent != agent
                && is_live(proposal)
                && overlaps(paths, &proposal.accepted_scope.paths)
        })
        .map(|(task, proposal)| format!("{} ({task})", proposal.agent))
        .collect()
}

/// Proposals `coordinator` may accept now: addressed to it, and
/// overlapping no live scope of another agent (including proposals accepted
/// earlier in the same pass). Returns (accept, wait) intent ids.
#[must_use]
pub fn auto_decisions(work: &WorkStatusResult, coordinator: &str) -> (Vec<String>, Vec<String>) {
    let mut live: Vec<(&str, &[String])> = proposals(work)
        .filter(|(_, proposal)| is_live(proposal))
        .map(|(_, proposal)| {
            (
                proposal.agent.as_str(),
                proposal.accepted_scope.paths.as_slice(),
            )
        })
        .collect();
    let (mut accept, mut wait) = (Vec::new(), Vec::new());
    for (_, proposal) in proposals(work) {
        let addressed = proposal
            .coordinator
            .as_deref()
            .unwrap_or(proposal.agent.as_str());
        if proposal.state != WorkTaskState::Proposed || addressed != coordinator {
            continue;
        }
        let paths = proposal.accepted_scope.paths.as_slice();
        if live
            .iter()
            .any(|(agent, scope)| *agent != proposal.agent && overlaps(paths, scope))
        {
            wait.push(proposal.intent_message_id.clone());
        } else {
            live.push((proposal.agent.as_str(), paths));
            accept.push(proposal.intent_message_id.clone());
        }
    }
    (accept, wait)
}

/// Canonical task id for a new claim by `agent`.
fn claim_task_id(agent: &str, paths: &[String], ordinal: usize) -> String {
    let mut stem: String = agent
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(96)
        .collect();
    if stem.is_empty() {
        stem.push_str("agent");
    }
    let digest = feanorfs_common::hash_bytes(format!("{paths:?}\u{0}{ordinal}").as_bytes());
    format!("{stem}-{}", &digest[..8])
}

async fn observe(ctx: &SyncCtx<'_>) -> Result<WorkStatusResult> {
    work_status(ctx, WorkStatusInput::default()).await
}

/// Waits (bounded) until a live agent's worktree reflects the current hub
/// head, so an accepted claim never edits a stale copy. Agents without a
/// live controller return immediately.
async fn wait_for_fresh_worktree(ctx: &SyncCtx<'_>, agent: &str, deadline: Instant) {
    let deadline = deadline.min(Instant::now() + FRESH_WORKTREE_WAIT);
    while Instant::now() < deadline {
        let Ok(Some(live)) = live_continuous_status(ctx.base, agent) else {
            return;
        };
        let Ok(head) = ctx.api.get_head(ctx.workspace_id()).await else {
            return;
        };
        if live.phase == ContinuousPhase::Idle
            && live.settled_snapshot.is_some()
            && live.observed_head == head
        {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Claims `paths` for `agent`: returns at once when already covered,
/// otherwise proposes (or reuses an identical pending proposal) and waits
/// for the coordinator's decision.
///
/// # Errors
/// Returns an error for invalid input or failed signal publication.
pub async fn claim_scope(
    ctx: &SyncCtx<'_>,
    agent: &str,
    paths: &[String],
    coordinator: Option<&str>,
    wait: Duration,
) -> Result<ClaimResult> {
    crate::paths::validate_name(agent)?;
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();
    ensure!(!paths.is_empty(), "claim needs at least one path");
    for path in &paths {
        ensure!(
            is_valid_scope_entry(path),
            "'{path}' is not a canonical path or `dir/**` glob"
        );
    }
    let deadline = Instant::now() + wait;
    let result =
        |outcome, task: Option<&str>, intent: Option<&str>, reason: Option<String>| ClaimResult {
            agent: agent.to_string(),
            outcome,
            task_id: task.map(str::to_string),
            intent_message_id: intent.map(str::to_string),
            paths: paths.clone(),
            reason,
        };
    let work = observe(ctx).await?;
    if claim_covered(&work, agent, &paths) {
        return Ok(result(ClaimOutcome::Covered, None, None, None));
    }
    let pending = proposals(&work).find(|(_, proposal)| {
        proposal.agent == agent
            && proposal.state == WorkTaskState::Proposed
            && proposal.accepted_scope.paths == paths
    });
    let (task_id, intent) = match pending {
        Some((task, proposal)) => (task.to_string(), proposal.intent_message_id.clone()),
        None => {
            let ordinal = proposals(&work).filter(|(_, p)| p.agent == agent).count();
            let task_id = claim_task_id(agent, &paths, ordinal);
            let sent = work_propose(
                ctx,
                WorkProposeInput {
                    task_id: task_id.clone(),
                    agent: Some(agent.to_string()),
                    sequence: 1,
                    causal_base: None,
                    coordinator: Some(coordinator.unwrap_or("human").to_string()),
                    paths: paths.clone(),
                    concerns: Vec::new(),
                    dependencies: Vec::new(),
                    capabilities: Vec::new(),
                    about_snapshot: None,
                    to: None,
                },
            )
            .await?;
            (task_id, sent.message_id)
        }
    };
    loop {
        let work = observe(ctx).await?;
        let state = proposals(&work)
            .find(|(_, proposal)| proposal.intent_message_id == intent)
            .map(|(_, proposal)| (proposal.state, proposal.reason.clone()));
        match state {
            Some((WorkTaskState::Accepted | WorkTaskState::Settled, _)) => {
                wait_for_fresh_worktree(ctx, agent, deadline).await;
                return Ok(result(
                    ClaimOutcome::Accepted,
                    Some(&task_id),
                    Some(&intent),
                    None,
                ));
            }
            Some((WorkTaskState::Proposed, _)) | None if Instant::now() < deadline => {
                tokio::time::sleep(POLL).await;
            }
            Some((WorkTaskState::Proposed, _)) | None => {
                let held = blockers(&work, agent, &paths);
                let reason = if held.is_empty() {
                    "no coordinator decision yet (is `feanorfs agent coordinate` running?)"
                        .to_string()
                } else {
                    format!("waiting for {} to finish", held.join(", "))
                };
                return Ok(result(
                    ClaimOutcome::Pending,
                    Some(&task_id),
                    Some(&intent),
                    Some(reason),
                ));
            }
            Some((state, reason)) => {
                return Ok(result(
                    ClaimOutcome::Rejected,
                    Some(&task_id),
                    Some(&intent),
                    Some(reason.unwrap_or_else(|| state.as_str().to_string())),
                ));
            }
        }
    }
}

/// Waits (bounded) until `agent`'s edits have landed and returns the
/// snapshot to record as inspected: the live settled snapshot, or the hub
/// head for an agent without a live controller.
async fn landed_snapshot(ctx: &SyncCtx<'_>, agent: &str, deadline: Instant) -> Result<String> {
    loop {
        match live_continuous_status(ctx.base, agent)? {
            None => {
                return ctx
                    .api
                    .get_head(ctx.workspace_id())
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("workspace has no head"));
            }
            Some(live) => {
                if let Some(attention) = &live.attention {
                    bail!(
                        "edits need attention before finishing ({}): {}",
                        attention.reason,
                        attention.detail
                    );
                }
                let unlanded = crate::agent::check_agent(
                    ctx.base,
                    ctx.db,
                    ctx.api,
                    ctx.workspace_id(),
                    agent,
                    ctx.password(),
                )
                .await
                .map(|check| !check.our_changes.is_empty())
                .unwrap_or(true);
                // An idle controller, or one that stopped after its final flush.
                let quiet = live.phase == ContinuousPhase::Idle
                    || (!live.active && live.phase == ContinuousPhase::Stopping);
                if let (false, true, false, Some(snapshot)) =
                    (unlanded, quiet, live.pending_local, live.settled_snapshot)
                {
                    return Ok(snapshot);
                }
            }
        }
        ensure!(
            Instant::now() < deadline,
            "edits have not landed yet; finish again shortly"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Settles and completes every task `agent` holds, after its edits land.
///
/// # Errors
/// Returns an error when edits need attention or do not land in time, or
/// when signal publication fails.
pub async fn finish_work(
    ctx: &SyncCtx<'_>,
    agent: &str,
    summary: Option<&str>,
    verification: Option<WorkVerificationStatus>,
    wait: Duration,
) -> Result<DoneResult> {
    crate::paths::validate_name(agent)?;
    let deadline = Instant::now() + wait;
    let work = observe(ctx).await?;
    let held: Vec<(String, WorkProposalStatus)> = proposals(&work)
        .filter(|(_, proposal)| proposal.agent == agent && is_live(proposal))
        .map(|(task, proposal)| (task.to_string(), proposal.clone()))
        .collect();
    if held.is_empty() {
        return Ok(DoneResult {
            agent: agent.to_string(),
            completed: Vec::new(),
            inspected_snapshot: None,
        });
    }
    let snapshot = landed_snapshot(ctx, agent, deadline).await?;
    let summary = summary.unwrap_or("finished; no verification reported");
    let status = verification.unwrap_or(WorkVerificationStatus::Skipped);
    let mut completed = Vec::new();
    for (task, proposal) in held {
        let mut sequence = proposal.sequence;
        if proposal.state == WorkTaskState::Accepted {
            sequence += 1;
            work_settle(
                ctx,
                WorkSettleInput {
                    task_id: task.clone(),
                    intent_message_id: proposal.intent_message_id.clone(),
                    sequence,
                    inspected_snapshot: snapshot.clone(),
                    verification: WorkVerification {
                        status,
                        summary: summary.to_string(),
                    },
                    about_snapshot: None,
                    to: None,
                    from: Some(agent.to_string()),
                },
            )
            .await?;
        }
        work_complete(
            ctx,
            WorkCompleteInput {
                task_id: task.clone(),
                intent_message_id: proposal.intent_message_id.clone(),
                sequence: sequence + 1,
                outcome: summary.to_string(),
                about_snapshot: None,
                to: None,
                from: Some(agent.to_string()),
            },
        )
        .await?;
        completed.push(task);
    }
    Ok(DoneResult {
        agent: agent.to_string(),
        completed,
        inspected_snapshot: Some(snapshot),
    })
}

/// One automatic coordinator pass for `coordinator`: accepts every
/// proposal addressed to it whose scope overlaps no other agent's live
/// scope. Overlapping proposals wait for a later pass; nothing is rejected.
///
/// # Errors
/// Returns an error for unreadable projections or failed publication.
pub async fn coordinate_pass(ctx: &SyncCtx<'_>, coordinator: &str) -> Result<CoordinatePass> {
    crate::paths::validate_name(coordinator)?;
    let work = observe(ctx).await?;
    let (accept, waiting) = auto_decisions(&work, coordinator);
    for intent in &accept {
        work_decide(
            ctx,
            WorkDecideInput {
                proposal_message_id: intent.clone(),
                kind: WorkDecisionKind::Accept(WorkDecisionAccept {
                    reason: Some("no overlapping live scope".to_string()),
                }),
                about_snapshot: None,
                to: None,
                from: Some(coordinator.to_string()),
            },
        )
        .await?;
    }
    Ok(CoordinatePass {
        coordinator: coordinator.to_string(),
        accepted: accept,
        waiting,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{WorkScope, WorkTaskStatus};

    fn proposal(agent: &str, state: WorkTaskState, paths: &[&str], id: char) -> WorkProposalStatus {
        WorkProposalStatus {
            agent: agent.to_string(),
            state,
            sequence: 1,
            intent_message_id: std::iter::repeat_n(id, 64).collect(),
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
            source_message_id: std::iter::repeat_n(id, 64).collect(),
            updated_at_ms: 0,
        }
    }

    fn work(proposals: Vec<(&str, WorkProposalStatus)>) -> WorkStatusResult {
        WorkStatusResult {
            cursor: String::new(),
            cursor_reset: false,
            projection_incomplete: false,
            messages_processed: 0,
            tasks: proposals
                .into_iter()
                .map(|(task, p)| WorkTaskStatus {
                    task_id: task.to_string(),
                    state: p.state,
                    proposals: vec![p],
                })
                .collect(),
            evidence_count: 0,
            dropped_count: 0,
            updated_at_ms: 0,
            applied_message_ids: vec![],
        }
    }

    #[test]
    fn coverage_respects_globs_and_ownership() {
        let projection = work(vec![(
            "a",
            proposal("linux", WorkTaskState::Accepted, &["src/**"], 'a'),
        )]);
        let paths = |p: &[&str]| p.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert!(claim_covered(
            &projection,
            "linux",
            &paths(&["src/lib.rs", "src/x/y.rs"])
        ));
        assert!(!claim_covered(&projection, "linux", &paths(&["README.md"])));
        assert!(!claim_covered(&projection, "mac", &paths(&["src/lib.rs"])));
        assert!(!covers("src/**", "srcx/lib.rs"));
    }

    #[test]
    fn auto_coordinator_accepts_disjoint_and_serializes_overlap() {
        let projection = work(vec![
            (
                "a",
                proposal("linux", WorkTaskState::Proposed, &["src/parser.py"], 'a'),
            ),
            (
                "b",
                proposal("mac", WorkTaskState::Proposed, &["src/parser.py"], 'b'),
            ),
            (
                "c",
                proposal("ci", WorkTaskState::Proposed, &["tests/**"], 'c'),
            ),
        ]);
        let (accept, wait) = auto_decisions(&projection, "human");
        assert_eq!(accept, vec!["a".repeat(64), "c".repeat(64)]);
        assert_eq!(wait, vec!["b".repeat(64)]);
        assert!(auto_decisions(&projection, "someone-else").0.is_empty());

        let busy = work(vec![
            (
                "a",
                proposal("linux", WorkTaskState::Accepted, &["src/**"], 'a'),
            ),
            (
                "b",
                proposal("mac", WorkTaskState::Proposed, &["src/parser.py"], 'b'),
            ),
        ]);
        assert_eq!(
            auto_decisions(&busy, "human"),
            (vec![], vec!["b".repeat(64)])
        );
    }

    #[test]
    fn claim_task_ids_are_canonical_and_distinct() {
        let paths = vec!["src/a.rs".to_string()];
        let first = claim_task_id("Mac.Dev", &paths, 0);
        assert!(feanorfs_common::is_valid_task_id(&first), "{first}");
        assert!(first.starts_with("mac-dev-"));
        assert_ne!(first, claim_task_id("Mac.Dev", &paths, 1));
        assert_eq!(bounded_wait(Some(9_999)), Duration::from_secs(600));
    }
}
