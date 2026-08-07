use clap::Subcommand;
use feanorfs_client::{
    integrator_assign, integrator_resume, integrator_revoke, integrator_status, load_config,
    IntegratorAssignInput, IntegratorCandidate, IntegratorObserveOptions,
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
        /// Candidate agent name (repeatable; include capabilities via JSON).
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
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, current_dir, &config)?;
    match action {
        IntegratorAction::Assign {
            about,
            candidate,
            require,
            exclude,
            exclude_author,
            ack_timeout,
            task_summary,
        } => {
            if candidate.is_empty() {
                anyhow::bail!(
                    "at least one --candidate is required; the dispatcher owns the roster"
                );
            }
            let candidates = candidate
                .iter()
                .map(|name| IntegratorCandidate {
                    name: name.clone(),
                    capabilities: require.clone(),
                    enabled: true,
                    available: true,
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
