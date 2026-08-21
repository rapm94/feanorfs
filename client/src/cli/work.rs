use clap::Subcommand;
use feanorfs_client::{
    load_config, work_amend, work_block, work_complete, work_decide, work_propose, work_settle,
    work_status, work_yield, WorkAmendInput, WorkBlockInput, WorkCompleteInput, WorkDecideInput,
    WorkOverlapAcceptance, WorkProposeInput, WorkSettleInput, WorkStatusInput, WorkYieldInput,
};
use feanorfs_common::{
    WorkDecisionAccept, WorkDecisionAcceptOverlap, WorkDecisionKind, WorkDecisionNarrow,
    WorkDecisionOrder, WorkDecisionReject, WorkVerification, WorkVerificationStatus,
};
use std::path::Path;

use super::util::output_json;

/// `feanorfs agent work` — encrypted work-intent coordination.
///
/// Proposals and decisions are ordinary `ffmsg1` signals carrying `ffwork1`
/// profiles; the local reducer only changes state after observing them.
#[derive(Subcommand)]
pub enum WorkAction {
    /// Propose bounded work scope for one task (not an acceptance claim).
    Propose {
        /// Canonical task id (lowercase letters, digits, `-`, `_`).
        #[arg(long)]
        task: String,
        /// Proposal author; defaults to FEANORFS_AGENT or human.
        #[arg(long)]
        agent: Option<String>,
        /// Author sequence; must exceed every prior intent for (task, agent).
        #[arg(long, default_value_t = 1)]
        sequence: u64,
        /// Immutable message id this proposal builds on.
        #[arg(long = "causal-base")]
        causal_base: Option<String>,
        /// Named coordinator identity whose decisions are authorized.
        #[arg(long)]
        coordinator: Option<String>,
        /// Exact path or `dir/**` containment glob (repeatable).
        #[arg(long = "path")]
        path: Vec<String>,
        /// Concern this work touches (repeatable).
        #[arg(long = "concern")]
        concern: Vec<String>,
        /// Task this proposal depends on (repeatable).
        #[arg(long = "dependency")]
        dependency: Vec<String>,
        /// Required capability (repeatable).
        #[arg(long = "capability")]
        capability: Vec<String>,
        /// Snapshot this proposal concerns; defaults to the current head.
        #[arg(long)]
        about: Option<String>,
        /// Recipient override; defaults to the named coordinator or `*`.
        #[arg(long)]
        to: Option<String>,
    },
    /// Send one coordinator decision for an exact proposal.
    Decide {
        /// Exact proposal message id (the intent's signal id).
        proposal_message_id: String,
        /// Decision kind: accept, reject, narrow, order, or accept-overlap.
        #[arg(long)]
        kind: String,
        /// Bounded reason (reject requires one).
        #[arg(long)]
        reason: Option<String>,
        /// Narrow: accepted paths (repeatable).
        #[arg(long = "path")]
        path: Vec<String>,
        /// Narrow: accepted concerns (repeatable).
        #[arg(long = "concern")]
        concern: Vec<String>,
        /// Order: proposal message id this proposal is sequenced after.
        #[arg(long)]
        after: Option<String>,
        /// Accept-overlap: one overlap entry as JSON (repeatable).
        #[arg(long = "overlap")]
        overlap: Vec<String>,
        #[arg(long)]
        about: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Amend the accepted scope of one intent (author-side).
    Amend {
        #[arg(long)]
        task: String,
        /// Exact accepted intent message id.
        #[arg(long)]
        intent: String,
        #[arg(long, default_value_t = 1)]
        sequence: u64,
        /// Replacement paths (repeatable; when present, replaces the scope).
        #[arg(long = "path")]
        path: Vec<String>,
        /// Replacement concerns (repeatable).
        #[arg(long = "concern")]
        concern: Vec<String>,
        /// Replacement dependencies (repeatable).
        #[arg(long = "dependency")]
        dependency: Vec<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        about: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Explicitly relinquish accepted overlap while preserving local work.
    Yield {
        #[arg(long)]
        task: String,
        /// Exact accepted intent message id.
        #[arg(long)]
        intent: String,
        #[arg(long, default_value_t = 1)]
        sequence: u64,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        about: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Mark accepted changes reconciled with verification evidence.
    Settle {
        #[arg(long)]
        task: String,
        /// Exact accepted intent message id.
        #[arg(long)]
        intent: String,
        #[arg(long, default_value_t = 1)]
        sequence: u64,
        /// Full 64-hex snapshot actually inspected/reconciled.
        #[arg(long)]
        inspected: String,
        /// Verification status: passed, failed, or skipped.
        #[arg(long)]
        verification: String,
        /// Bounded verification summary.
        #[arg(long)]
        summary: String,
        #[arg(long)]
        about: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Terminal success for a settled task.
    Complete {
        #[arg(long)]
        task: String,
        /// Exact accepted intent message id.
        #[arg(long)]
        intent: String,
        #[arg(long, default_value_t = 1)]
        sequence: u64,
        /// Bounded outcome summary.
        #[arg(long)]
        outcome: String,
        #[arg(long)]
        about: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Terminal automation blocker (does not imply human escalation).
    Block {
        #[arg(long)]
        task: String,
        /// Exact accepted intent message id.
        #[arg(long)]
        intent: String,
        #[arg(long, default_value_t = 1)]
        sequence: u64,
        /// Bounded blocker reason.
        #[arg(long)]
        reason: String,
        #[arg(long)]
        about: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Observe signals through the reducer and report the bounded projection.
    /// Coordinator authority comes from authenticated protocol state only.
    Status,
}

fn sender(explicit: Option<&str>) -> Option<String> {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("FEANORFS_AGENT").ok())
        .filter(|value| !value.trim().is_empty())
}

fn render_send(result: &feanorfs_common::WorkSendResult, verb: &str, note: &str) {
    println!(
        "{verb} {profile} {message_id} for task '{task}' by '{agent}' (about {about}).",
        profile = result.profile,
        message_id = &result.message_id[..8],
        task = result.task_id,
        agent = result.agent,
        about = &result.about_snapshot[..8]
    );
    if !result.scope.paths.is_empty() || !result.scope.concerns.is_empty() {
        println!(
            "  Scope: {} path(s), {} concern(s), {} dependency/dependencies.",
            result.scope.paths.len(),
            result.scope.concerns.len(),
            result.scope.dependencies.len()
        );
    }
    if !result.causal_refs.is_empty() {
        println!(
            "  References: {}.",
            result
                .causal_refs
                .iter()
                .map(|id| &id[..8])
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !result.overlap.is_empty() {
        println!("  Accepted overlap: {} entrie(s).", result.overlap.len());
    }
    println!("  {note}");
}

pub async fn run(current_dir: &Path, action: WorkAction, json: bool) -> anyhow::Result<()> {
    let control_root = super::agent::control_workspace_root(current_dir)?;
    let config = load_config(&control_root)?;
    let db = crate::open_client_db(&control_root).await?;
    let api = crate::open_api_client(&control_root, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, &control_root, &config)?;
    let from = sender(None);
    match action {
        WorkAction::Propose {
            task,
            agent,
            sequence,
            causal_base,
            coordinator,
            path,
            concern,
            dependency,
            capability,
            about,
            to,
        } => {
            if path.is_empty() {
                anyhow::bail!("at least one --path is required for a work proposal");
            }
            let result = work_propose(
                &ctx,
                WorkProposeInput {
                    task_id: task,
                    agent,
                    sequence,
                    causal_base,
                    coordinator: coordinator.clone(),
                    paths: path,
                    concerns: concern,
                    dependencies: dependency,
                    capabilities: capability,
                    about_snapshot: about,
                    to,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                render_send(
                    &result,
                    "Proposed",
                    "Not accepted: the scope applies only after an observed coordinator decision.",
                );
            }
        }
        WorkAction::Decide {
            proposal_message_id,
            kind,
            reason,
            path,
            concern,
            after,
            overlap,
            about,
            to,
        } => {
            let decision = match kind.as_str() {
                "accept" => WorkDecisionKind::Accept(WorkDecisionAccept { reason }),
                "reject" => WorkDecisionKind::Reject(WorkDecisionReject {
                    reason: reason.ok_or_else(|| {
                        anyhow::anyhow!("reject requires --reason")
                    })?,
                }),
                "narrow" => {
                    if path.is_empty() {
                        anyhow::bail!("narrow requires at least one --path");
                    }
                    WorkDecisionKind::Narrow(WorkDecisionNarrow {
                        paths: path,
                        concerns: concern,
                        reason,
                    })
                }
                "order" => WorkDecisionKind::Order(WorkDecisionOrder {
                    after,
                    reason,
                }),
                "accept-overlap" | "accept_overlap" => {
                    if overlap.is_empty() {
                        anyhow::bail!(
                            "accept-overlap requires at least one --overlap <json> entry"
                        );
                    }
                    let entries: Vec<WorkOverlapAcceptance> = overlap
                        .iter()
                        .map(|entry| {
                            serde_json::from_str(entry).map_err(|error| {
                                anyhow::anyhow!("invalid --overlap JSON {entry:?}: {error}")
                            })
                        })
                        .collect::<anyhow::Result<_>>()?;
                    WorkDecisionKind::AcceptOverlap(WorkDecisionAcceptOverlap {
                        overlap: entries,
                        reason,
                    })
                }
                other => anyhow::bail!(
                    "unknown decision kind {other:?}; use accept, reject, narrow, order, or accept-overlap"
                ),
            };
            let result = work_decide(
                &ctx,
                WorkDecideInput {
                    proposal_message_id,
                    kind: decision,
                    about_snapshot: about,
                    to,
                    from,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                render_send(
                    &result,
                    "Sent",
                    "Decision applies once the reducer observes it.",
                );
            }
        }
        WorkAction::Amend {
            task,
            intent,
            sequence,
            path,
            concern,
            dependency,
            reason,
            about,
            to,
        } => {
            let result = work_amend(
                &ctx,
                WorkAmendInput {
                    task_id: task,
                    intent_message_id: intent,
                    sequence,
                    paths: (!path.is_empty()).then_some(path),
                    concerns: (!concern.is_empty()).then_some(concern),
                    dependencies: (!dependency.is_empty()).then_some(dependency),
                    reason,
                    about_snapshot: about,
                    to,
                    from,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                render_send(
                    &result,
                    "Amended",
                    "The amended scope applies to the accepted intent when observed.",
                );
            }
        }
        WorkAction::Yield {
            task,
            intent,
            sequence,
            reason,
            about,
            to,
        } => {
            let result = work_yield(
                &ctx,
                WorkYieldInput {
                    task_id: task,
                    intent_message_id: intent,
                    sequence,
                    reason,
                    about_snapshot: about,
                    to,
                    from,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                render_send(
                    &result,
                    "Yielded",
                    "Accepted overlap relinquished; local work preserved.",
                );
            }
        }
        WorkAction::Settle {
            task,
            intent,
            sequence,
            inspected,
            verification,
            summary,
            about,
            to,
        } => {
            let status = match verification.as_str() {
                "passed" => WorkVerificationStatus::Passed,
                "failed" => WorkVerificationStatus::Failed,
                "skipped" => WorkVerificationStatus::Skipped,
                other => anyhow::bail!(
                    "unknown verification status {other:?}; use passed, failed, or skipped"
                ),
            };
            let result = work_settle(
                &ctx,
                WorkSettleInput {
                    task_id: task,
                    intent_message_id: intent,
                    sequence,
                    inspected_snapshot: inspected,
                    verification: WorkVerification { status, summary },
                    about_snapshot: about,
                    to,
                    from,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                render_send(
                    &result,
                    "Settled",
                    "Verification evidence attached when observed.",
                );
            }
        }
        WorkAction::Complete {
            task,
            intent,
            sequence,
            outcome,
            about,
            to,
        } => {
            let result = work_complete(
                &ctx,
                WorkCompleteInput {
                    task_id: task,
                    intent_message_id: intent,
                    sequence,
                    outcome,
                    about_snapshot: about,
                    to,
                    from,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                render_send(&result, "Completed", "Terminal success when observed.");
            }
        }
        WorkAction::Block {
            task,
            intent,
            sequence,
            reason,
            about,
            to,
        } => {
            let result = work_block(
                &ctx,
                WorkBlockInput {
                    task_id: task,
                    intent_message_id: intent,
                    sequence,
                    reason,
                    about_snapshot: about,
                    to,
                    from,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                render_send(
                    &result,
                    "Blocked",
                    "Terminal automation blocker when observed.",
                );
            }
        }
        WorkAction::Status => {
            let result = work_status(&ctx, WorkStatusInput::default()).await?;
            if json {
                output_json(&result)?;
            } else {
                render_status(&result);
            }
        }
    }
    Ok(())
}

fn render_status(result: &feanorfs_common::WorkStatusResult) {
    if result.projection_incomplete {
        println!(
            "Warning: the signal closure was incomplete (cursor reset or bound exhaustion); \
             acceptance is not fully provable."
        );
    }
    if result.tasks.is_empty() {
        println!("No work proposals observed.");
    }
    for task in &result.tasks {
        println!("Task '{}': {}", task.task_id, task.state.as_str());
        for proposal in &task.proposals {
            let decision = proposal
                .decision
                .as_ref()
                .map(|d| d.kind.type_name())
                .unwrap_or("-");
            println!(
                "  {} (seq {}) intent {}: {} [decision: {}] {}",
                proposal.agent,
                proposal.sequence,
                &proposal.intent_message_id[..8],
                proposal.state.as_str(),
                decision,
                proposal
                    .reason
                    .as_deref()
                    .map(|reason| format!("({})", super::util::terminal_line(reason)))
                    .unwrap_or_default()
            );
            if !proposal.accepted_overlap.is_empty() {
                println!(
                    "    Accepted overlap: {} entrie(s).",
                    proposal.accepted_overlap.len()
                );
            }
        }
    }
    println!(
        "Evidence: {} record(s); dropped: {}; projection incomplete: {}.",
        result.evidence_count, result.dropped_count, result.projection_incomplete
    );
}
