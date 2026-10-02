use clap::{Subcommand, ValueEnum};
use feanorfs_client::{
    agent_identity, integrator_assign, integrator_reply, integrator_resume, integrator_revoke,
    integrator_status, load_config, IntegratorAssignInput, IntegratorCandidate,
    IntegratorObserveOptions,
};
use feanorfs_common::{
    IntegratorOutcomeState, IntegratorReplyInput, IntegratorReplyKind, VerificationStatus,
    VerificationSummary,
};
use std::path::Path;

use super::util::output_json;

/// `feanorfs agent integrator` — randomized integrator assignment.
#[derive(Subcommand)]
pub enum IntegratorAction {
    /// Randomly rank eligible candidates and offer one the assignment.
    Assign {
        /// Full reachable format-v3 snapshot the batch concerns.
        #[arg(long)]
        about: String,
        /// Candidate agent (repeatable): `name` uses its advertised
        /// capabilities, `name=cap1,cap2` states them. Omit to offer to every
        /// agent in the capability roster.
        #[arg(long = "candidate", value_name = "AGENT")]
        candidate: Vec<String>,
        /// Required capability (repeatable; every eligible candidate needs all).
        #[arg(long = "require", value_name = "CAPABILITY")]
        require: Vec<String>,
        /// Explicit user exclusion (repeatable).
        #[arg(long = "exclude", value_name = "AGENT")]
        exclude: Vec<String>,
        /// Name of an agent that authored a conflicting side (repeatable).
        #[arg(long = "exclude-author", value_name = "AGENT")]
        exclude_author: Vec<String>,
        /// Pre-acceptance acknowledgement timeout (e.g. 5m, 60s).
        #[arg(long = "ack-timeout", value_name = "DURATION")]
        ack_timeout: Option<String>,
        /// Bounded plain-language objective.
        task_summary: String,
    },
    /// Show the active assignment or one assignment's state.
    Status {
        /// Assignment id (defaults to the active assignment).
        assignment_id: Option<String>,
    },
    /// Explicitly revoke the active integrator (records the reason; may offer
    /// the next ranked candidate when one remains).
    Revoke {
        assignment_id: String,
        /// Bounded reason recorded in the audit trail.
        #[arg(long)]
        reason: String,
    },
    /// Resume dispatcher observation after a restart: reads replies since the
    /// persisted cursor and applies lifecycle transitions. Never re-sends a
    /// recorded request.
    Resume {
        /// Pre-acceptance acknowledgement timeout (e.g. 5m, 60s).
        #[arg(long = "ack-timeout", value_name = "DURATION")]
        ack_timeout: Option<String>,
        /// Allow fallback to the next ranked candidate after a candidate
        /// blocker (off by default: post-acceptance fallback requires an
        /// explicit stop, revocation, or blocker policy).
        #[arg(long)]
        fallback_on_blocked: bool,
    },
    /// Candidate side: accept an offer, send the terminal result, or report
    /// a blocker. The engine binds assignment, attempt, snapshot, dispatcher,
    /// and request ids, and refuses superseded attempts.
    Reply {
        kind: ReplyKindArg,
        /// Assignment id; defaults to the newest open offer to you.
        #[arg(long = "assignment")]
        assignment_id: Option<String>,
        /// Replying candidate; defaults to FEANORFS_AGENT or human.
        #[arg(long = "for")]
        for_agent: Option<String>,
        /// Blocker reason (`blocked`).
        #[arg(long)]
        reason: Option<String>,
        /// Outcome summary (`result`).
        #[arg(long)]
        outcome: Option<String>,
        /// Outcome state (`result`); defaults to completed.
        #[arg(long)]
        state: Option<OutcomeStateArg>,
        /// Verification status (`result`).
        #[arg(long)]
        verification: Option<VerificationArg>,
        /// Verification summary (`result`).
        #[arg(long)]
        summary: Option<String>,
        /// Snapshot actually inspected; defaults to the current head.
        #[arg(long)]
        inspected: Option<String>,
        #[arg(long, default_value_t = 0)]
        landed: u64,
        #[arg(long, default_value_t = 0)]
        resolved: u64,
        #[arg(long, default_value_t = 0)]
        remaining: u64,
        /// Risk line (repeatable).
        #[arg(long = "risk")]
        risks: Vec<String>,
        /// The one decision question for a requires-human result.
        #[arg(long)]
        decision: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReplyKindArg {
    Accept,
    Result,
    Blocked,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutcomeStateArg {
    Completed,
    Blocked,
    RequiresHuman,
    Cancelled,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum VerificationArg {
    Passed,
    Failed,
    Unknown,
}

fn parse_duration_ms(value: &str) -> anyhow::Result<u64> {
    let trimmed = value.trim().to_ascii_lowercase();
    let (number, unit) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map(|index| trimmed.split_at(index))
        .unwrap_or((trimmed.as_str(), "ms"));
    let number: u64 = number.parse().map_err(|_| {
        anyhow::anyhow!("invalid duration {value:?}; use e.g. '5m', '60s', '300000ms'")
    })?;
    match unit {
        "ms" => Ok(number),
        "s" => Ok(number.saturating_mul(1000)),
        "m" => Ok(number.saturating_mul(60_000)),
        "h" => Ok(number.saturating_mul(3_600_000)),
        "" => Ok(number),
        other => anyhow::bail!("invalid duration unit {other:?} in {value:?}; use ms, s, m, or h"),
    }
}

pub async fn run(current_dir: &Path, action: IntegratorAction, json: bool) -> anyhow::Result<()> {
    let root = super::agent::control_workspace_root(current_dir)?;
    let current_dir = root.as_path();
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, current_dir, &config)?;
    match action {
        IntegratorAction::Reply {
            kind,
            assignment_id,
            for_agent,
            reason,
            outcome,
            state,
            verification,
            summary,
            inspected,
            landed,
            resolved,
            remaining,
            risks,
            decision,
        } => {
            let agent = agent_identity(for_agent.as_deref());
            let verification = verification.map(|status| VerificationSummary {
                status: match status {
                    VerificationArg::Passed => VerificationStatus::Passed,
                    VerificationArg::Failed => VerificationStatus::Failed,
                    VerificationArg::Unknown => VerificationStatus::Unknown,
                },
                summary: summary.clone().unwrap_or_default(),
                ..VerificationSummary::default()
            });
            let input = IntegratorReplyInput {
                agent: Some(agent.clone()),
                assignment_id,
                kind: match kind {
                    ReplyKindArg::Accept => IntegratorReplyKind::Accept,
                    ReplyKindArg::Result => IntegratorReplyKind::Result,
                    ReplyKindArg::Blocked => IntegratorReplyKind::Blocked,
                },
                reason,
                state: state.map(|state| match state {
                    OutcomeStateArg::Completed => IntegratorOutcomeState::Completed,
                    OutcomeStateArg::Blocked => IntegratorOutcomeState::Blocked,
                    OutcomeStateArg::RequiresHuman => IntegratorOutcomeState::RequiresHuman,
                    OutcomeStateArg::Cancelled => IntegratorOutcomeState::Cancelled,
                }),
                outcome,
                verification,
                inspected_snapshot: inspected,
                landed_paths: landed,
                resolved_conflicts: resolved,
                remaining_conflicts: remaining,
                risks,
                decision_required: decision,
            };
            let result = integrator_reply(&ctx, &agent, input).await?;
            if json {
                output_json(&result)?;
            } else {
                println!(
                    "Sent {:?} for assignment {} attempt {} to '{}' (signal {}).",
                    result.kind,
                    &result.assignment_id[..8],
                    result.attempt,
                    result.dispatcher,
                    &result.message_id[..8]
                );
            }
        }
        IntegratorAction::Assign {
            about,
            candidate,
            require,
            exclude,
            exclude_author,
            ack_timeout,
            task_summary,
        } => {
            // Bare names take their advertised capabilities from the
            // roster; `name=cap1,cap2` states them explicitly; no candidates
            // lets the engine offer to the whole roster.
            let roster = if candidate.iter().any(|name| !name.contains('=')) {
                feanorfs_client::capability_roster(&ctx).await?.agents
            } else {
                Vec::new()
            };
            let candidates = candidate
                .iter()
                .map(|spec| {
                    let (name, capabilities) = match spec.split_once('=') {
                        Some((name, caps)) => (
                            name.to_string(),
                            caps.split(',')
                                .filter(|cap| !cap.is_empty())
                                .map(str::to_string)
                                .collect(),
                        ),
                        None => (
                            spec.clone(),
                            roster
                                .iter()
                                .find(|entry| &entry.agent == spec)
                                .map(|entry| entry.capabilities.clone())
                                .unwrap_or_default(),
                        ),
                    };
                    IntegratorCandidate {
                        name,
                        capabilities,
                        enabled: true,
                        available: true,
                    }
                })
                .collect();
            let result = integrator_assign(
                &ctx,
                IntegratorAssignInput {
                    about_snapshot: about,
                    candidates,
                    required_capabilities: require,
                    conflict_authors: exclude_author,
                    excluded: exclude,
                    task_summary,
                    ack_timeout_ms: Some(
                        ack_timeout
                            .as_deref()
                            .map(parse_duration_ms)
                            .transpose()?
                            .unwrap_or(feanorfs_common::INTEGRATOR_DEFAULT_ACK_TIMEOUT_MS),
                    ),
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                println!(
                    "Assignment {} offered to '{}' (attempt {}; about {}).",
                    &result.assignment_id[..8],
                    result.selected,
                    result.attempt,
                    &result.about_snapshot[..8]
                );
                if !result.neutral_integrator {
                    println!(
                        "Note: no neutral candidate existed; the draw used the full eligible pool."
                    );
                }
                println!(
                    "Fallback order: {}. Run `feanorfs agent integrator resume` to process replies.",
                    result.fallback_order.join(", ")
                );
            }
        }
        IntegratorAction::Status { assignment_id } => {
            let result = integrator_status(&ctx, assignment_id.as_deref()).await?;
            if json {
                output_json(&result)?;
            } else {
                println!("Assignment {}:", &result.assignment_id[..8]);
                println!("  State:        {:?}", result.state);
                println!(
                    "  Integrator:   {} (attempt {})",
                    result.selected.as_deref().unwrap_or("-"),
                    result.attempt
                );
                println!("  About:        {}", &result.about_snapshot[..8]);
                println!("  Neutral draw: {}", result.neutral_integrator);
                println!("  Fallback:     {}", result.fallback_order.join(", "));
                if let Some(digest) = &result.digest {
                    println!(
                        "  Outcome:      {} ({} verification)",
                        digest.outcome,
                        digest.verification.status.as_str()
                    );
                }
                if result.state == feanorfs_common::IntegratorAssignmentState::RequiresHuman {
                    println!(
                        "  Action:       dispatcher state is uncertain; stop automatic mutation \
                         and recover the orchestrator state"
                    );
                }
            }
        }
        IntegratorAction::Revoke {
            assignment_id,
            reason,
        } => {
            let result = integrator_revoke(&ctx, &assignment_id, &reason).await?;
            if json {
                output_json(&result)?;
            } else {
                println!(
                    "Revoked assignment {} ({:?}).",
                    &result.assignment_id[..8],
                    result.state
                );
                if let Some(selected) = &result.selected {
                    println!("  Next integrator: {selected}");
                }
            }
        }
        IntegratorAction::Resume {
            ack_timeout,
            fallback_on_blocked,
        } => {
            let result = integrator_resume(
                &ctx,
                IntegratorObserveOptions {
                    ack_timeout_ms: ack_timeout.as_deref().map(parse_duration_ms).transpose()?,
                    fallback_on_blocked,
                },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                match result.action.as_str() {
                    "none" => println!("No active assignment to resume."),
                    action => println!(
                        "Observed assignment {}: state {:?} (action {action}).",
                        result
                            .assignment_id
                            .as_deref()
                            .map(|id| &id[..8])
                            .unwrap_or("-"),
                        result.state
                    ),
                }
            }
        }
    }
    Ok(())
}
