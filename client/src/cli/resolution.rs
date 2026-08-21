use anyhow::Context as _;
use clap::Subcommand;
use feanorfs_client::{
    answer_resolution, apply_resolution_job, defer_resolution, load_config,
    materialize_resolution_legs, prepare_resolution_job, put_resolution_candidate,
    resolution_protocol_status, resolution_status, send_human_answer, send_resolution_assignment,
    send_resolution_result, send_resolution_revoke, submit_resolution_result,
    ResolutionApplyOutcome,
};
use feanorfs_common::{
    validate_human_resolution_answer, HumanResolutionAnswer, HumanResolutionOption,
    PreventionReason, ResolutionResult, VerificationStatus, VerificationSummary,
    RESOLUTION_SCHEMA_VERSION,
};
use std::io::Read as _;
use std::path::Path;

use super::util::output_json;

/// Maximum plaintext size of one resolution candidate accepted by the engine.
const MAX_CANDIDATE_BYTES: u64 = 64 * 1024 * 1024;

/// `feanorfs agent resolution` — exact-fingerprint automatic conflict
/// resolution.
///
/// Prepare creates one immutable job bound to the exact current conflict;
/// submit records a validated resolver result (submit NEVER applies); apply
/// publishes with guarded revalidation; status reads a bounded
/// ids/state/counts projection. Legacy unfingerprinted conflicts can be
/// resolved manually with `conflicts keep` but can never enter automatic
/// prepare/apply. The `ffres1` signal operations (materialize, put, answer,
/// defer, protocol-status, assign, reply, revoke, publish-answer) bind every
/// identity field to the live projection and never reimplement engine
/// validation.
#[derive(Subcommand)]
pub enum ResolutionAction {
    /// Prepare one automatic resolution job for the exact current conflict.
    ///
    /// Requires a real current conflict in the workspace head and a typed
    /// prevention-exhausted/violated reason. Read-only: writes a job under
    /// the protected orchestrator boundary without changing the worktree,
    /// conflict registry, artifacts, or head.
    Prepare {
        /// Canonical workspace-relative conflict path.
        path: String,
        /// Prevention reason type: exhausted (default) or violated.
        #[arg(long, default_value = "exhausted")]
        reason: String,
        /// Bounded plain-language prevention detail (max 1024 bytes).
        #[arg(long)]
        detail: String,
    },
    /// Read the bounded resolution status projection (ids/state/counts only).
    Status {
        /// Restrict the projection to one job.
        job_id: Option<String>,
    },
    /// Submit one resolution result. Submission NEVER applies: it validates
    /// the result and records it without mutating the worktree, registry,
    /// artifacts, or head. Apply is a separate explicit command.
    Submit {
        /// Job id returned by prepare.
        job_id: String,
        /// Path to the result JSON document, or `-` to read stdin.
        #[arg(long, default_value = "-")]
        result: String,
    },
    /// Apply a submitted result with guarded publication: revalidates every
    /// identity field and the candidate descriptor immediately before a
    /// single CAS. A lost CAS restarts complete validation; the current
    /// conflict survives unchanged for any typed stale outcome.
    Apply { job_id: String },
    /// Materialize the authenticated base/ours/theirs legs of one job into
    /// the engine-owned job directory (create-new, no-follow, fsync'd) so a
    /// designated machine can reconstruct the conflict context by ID and
    /// fingerprint. Read-only: never changes the worktree, registry,
    /// artifacts, or head.
    Materialize { job_id: String },
    /// Write the immutable engine-owned candidate file for one job from a
    /// bounded local file (create-new, no-follow, fsync'd) and print its
    /// plaintext descriptor. Allowed while the job is active and carries no
    /// candidate-bearing result.
    Put { job_id: String, file: String },
    /// Record one typed human answer bound to the exact current escalation.
    /// The answer is bound to the live projection's
    /// job/assignment/attempt/fingerprint/question generation — the caller
    /// never supplies identity fields, so stale answers are impossible by
    /// construction (the engine re-validates). Never publishes; use
    /// publish-answer to emit the `ffres1` profile.
    Answer {
        /// Job id carrying the outstanding question.
        job_id: String,
        /// Record the terminal `Deferred` state without publication.
        #[arg(long)]
        defer: bool,
        /// Record the terminal `KeepUnresolved` state without publication.
        #[arg(long)]
        keep_unresolved: bool,
        /// Bounded candidate file; the engine runs the inline verification.
        #[arg(long)]
        candidate: Option<String>,
    },
    /// Record the terminal `Deferred` state for one assignment without any
    /// publication; the conflict is preserved for later manual action.
    Defer { job_id: String },
    /// Observe the encrypted signal stream through the deterministic `ffres1`
    /// reducer and report the bounded metadata-only projection. `--rebuild`
    /// resets the cursor and re-observes the bounded window.
    ProtocolStatus {
        /// Reset the observation cursor and re-apply the bounded window.
        #[arg(long)]
        rebuild: bool,
    },
    /// Publish the `ffres1` assignment profile (with the complete immutable
    /// job) for one locally prepared job.
    Assign { job_id: String },
    /// Publish the `ffres1` result profile for one locally submitted job.
    Reply { job_id: String },
    /// Publish the `ffres1` revoke/supersede profile for one local job.
    Revoke {
        job_id: String,
        /// Mark the assignment superseded rather than revoked.
        #[arg(long)]
        superseded: bool,
    },
    /// Publish one typed human answer as an `ffres1` profile. The answer is
    /// built exactly like `answer` (bound to the live projection), then
    /// validated and sent for remote observation; the local store is never
    /// mutated by publication.
    PublishAnswer {
        /// Job id carrying the outstanding question.
        job_id: String,
        /// Publish the `Defer` option.
        #[arg(long)]
        defer: bool,
        /// Publish the `KeepUnresolved` option.
        #[arg(long)]
        keep_unresolved: bool,
        /// Bounded candidate file; the descriptor is engine-validated by
        /// `put` before publication.
        #[arg(long)]
        candidate: Option<String>,
    },
}

fn read_result_json(source: &str) -> anyhow::Result<String> {
    if source == "-" {
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer)?;
        return Ok(buffer);
    }
    std::fs::read_to_string(source)
        .map_err(|error| anyhow::anyhow!("cannot read result file {source}: {error}"))
}

fn render_apply(outcome: &ResolutionApplyOutcome, job_id: &str) {
    match outcome {
        ResolutionApplyOutcome::Published { head } => {
            println!(
                "Published resolution job {} as head {}.",
                &job_id[..job_id.len().min(8)],
                &head[..head.len().min(8)]
            );
        }
        ResolutionApplyOutcome::Stale { kind, diagnostics } => {
            println!(
                "Apply refused for job {}: {:?}. The current conflict survives unchanged.",
                &job_id[..job_id.len().min(8)],
                kind
            );
            for diagnostic in diagnostics.iter().take(8) {
                println!("  {diagnostic}");
            }
        }
    }
}

/// Reads one candidate file with a hard 64 MiB plaintext bound (matches the
/// engine's `RESOLUTION_MAX_CANDIDATE_BYTES`; oversized files fail closed).
fn read_candidate_bytes(source: &str) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(source)
        .map_err(|error| anyhow::anyhow!("cannot read candidate file {source}: {error}"))?;
    let mut bounded = file.take(MAX_CANDIDATE_BYTES + 1);
    let mut bytes = Vec::new();
    bounded.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CANDIDATE_BYTES {
        anyhow::bail!("candidate file {source} exceeds the 64 MiB bound");
    }
    Ok(bytes)
}

/// Picks exactly one typed human option from the CLI flags.
fn pick_answer_option(
    defer: bool,
    keep_unresolved: bool,
    submit_candidate: bool,
) -> anyhow::Result<HumanResolutionOption> {
    let selected = [defer, keep_unresolved, submit_candidate]
        .into_iter()
        .filter(|flag| *flag)
        .count();
    anyhow::ensure!(
        selected == 1,
        "exactly one of --defer, --keep-unresolved, or --candidate <path> is required"
    );
    if defer {
        Ok(HumanResolutionOption::Defer)
    } else if keep_unresolved {
        Ok(HumanResolutionOption::KeepUnresolved)
    } else {
        Ok(HumanResolutionOption::SubmitCandidate)
    }
}

/// Builds one human answer bound to the exact current escalation.
///
/// Every identity field (job, assignment, attempt, fingerprint, question
/// generation) is read from the bounded `resolution_status` projection — the
/// caller never supplies them — so a stale answer is impossible by
/// construction. `--candidate <path>` reads the file bounded (64 MiB cap)
/// and records the engine-owned candidate via `put_resolution_candidate`
/// first; verification evidence is left `None` and the engine's answer path
/// runs the inline verification.
async fn bind_human_answer(
    ctx: &feanorfs_client::SyncCtx<'_>,
    job_id: &str,
    chosen_option: HumanResolutionOption,
    candidate_source: Option<&str>,
) -> anyhow::Result<HumanResolutionAnswer> {
    let projection = resolution_status(ctx, Some(job_id)).await?;
    let job = projection
        .jobs
        .iter()
        .find(|job| job.job_id == job_id)
        .with_context(|| format!("unknown resolution job {job_id}; answer refused"))?;
    let candidate = match candidate_source {
        Some(source) => {
            let bytes = read_candidate_bytes(source)?;
            Some(put_resolution_candidate(ctx, job_id, &bytes).await?)
        }
        None => None,
    };
    Ok(HumanResolutionAnswer {
        schema_version: RESOLUTION_SCHEMA_VERSION,
        job_id: job.job_id.clone(),
        assignment_id: job.assignment_id.clone(),
        attempt: job.attempt,
        conflict_fingerprint: job.conflict_fingerprint.clone(),
        question_generation: job.question_generation,
        chosen_option,
        candidate,
        verification: None,
    })
}

/// Prints one `ffres1` send result (message id).
fn print_message_id(
    json: bool,
    action: &str,
    job_id: &str,
    message_id: &str,
) -> anyhow::Result<()> {
    if json {
        output_json(&serde_json::json!({ "message_id": message_id }))?;
    } else {
        println!(
            "{action} {} as protocol message {}.",
            &job_id[..job_id.len().min(8)],
            &message_id[..message_id.len().min(8)]
        );
    }
    Ok(())
}

pub async fn run(current_dir: &Path, action: ResolutionAction, json: bool) -> anyhow::Result<()> {
    let control_root = super::agent::control_workspace_root(current_dir)?;
    let config = load_config(&control_root)?;
    let db = crate::open_client_db(&control_root).await?;
    let api = crate::open_api_client(&control_root, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, &control_root, &config)?;
    match action {
        ResolutionAction::Prepare {
            path,
            reason,
            detail,
        } => {
            let prevention = match reason.as_str() {
                "exhausted" => PreventionReason::Exhausted { detail },
                "violated" => PreventionReason::Violated { detail },
                other => {
                    anyhow::bail!(
                        "unknown prevention reason {other:?}; use `exhausted` or `violated`"
                    )
                }
            };
            let job = prepare_resolution_job(&ctx, &path, prevention).await?;
            if json {
                output_json(&job)?;
            } else {
                println!(
                    "Prepared resolution job {} for '{}' (assignment {}, attempt {}, fingerprint {}).",
                    &job.job_id[..8],
                    job.conflict.path,
                    &job.assignment_id[..8],
                    job.attempt,
                    &job.conflict_fingerprint[..8]
                );
                println!(
                    "  Candidate destination: {}",
                    job.candidate_destination.path
                );
                println!(
                    "  Submit a result with `feanorfs agent resolution submit <job-id> --result <file>`; submit never applies."
                );
            }
        }
        ResolutionAction::Status { job_id } => {
            let projection = resolution_status(&ctx, job_id.as_deref()).await?;
            if json {
                output_json(&projection)?;
            } else if projection.jobs.is_empty() {
                println!("No resolution jobs.");
            } else {
                for job in &projection.jobs {
                    println!(
                        "Job {} assignment {} attempt {} owner {} state {:?} outcome {:?} fingerprint {}",
                        &job.job_id[..8],
                        &job.assignment_id[..8],
                        job.attempt,
                        job.owner,
                        job.assignment_state,
                        job.outcome,
                        &job.conflict_fingerprint[..8]
                    );
                }
            }
        }
        ResolutionAction::Submit { job_id, result } => {
            let result_json = read_result_json(&result)?;
            let result: ResolutionResult = serde_json::from_str(&result_json)
                .map_err(|error| anyhow::anyhow!("invalid resolution result JSON: {error}"))?;
            let submitted = submit_resolution_result(&ctx, &job_id, result).await?;
            if json {
                output_json(&submitted)?;
            } else {
                println!(
                    "Submitted {} result for job {} (assignment {}, attempt {}). Submit never applies; run `feanorfs agent resolution apply {}` to publish.",
                    submitted.outcome.as_str(),
                    &job_id[..job_id.len().min(8)],
                    &submitted.assignment_id[..8],
                    submitted.attempt,
                    &job_id[..job_id.len().min(8)]
                );
            }
        }
        ResolutionAction::Apply { job_id } => {
            let outcome = apply_resolution_job(&ctx, &job_id).await?;
            if json {
                output_json(&outcome)?;
            } else {
                render_apply(&outcome, &job_id);
            }
        }
        ResolutionAction::Materialize { job_id } => {
            let legs = materialize_resolution_legs(&ctx, &job_id).await?;
            if json {
                let mapped: Vec<serde_json::Value> = legs
                    .iter()
                    .map(|(role, path)| {
                        serde_json::to_value(path)
                            .map(|path| serde_json::json!({ "role": role.as_str(), "path": path }))
                    })
                    .collect::<serde_json::Result<_>>()?;
                output_json(&mapped)?;
            } else {
                for (role, path) in &legs {
                    println!("{} {}", role.as_str(), path.display());
                }
            }
        }
        ResolutionAction::Put { job_id, file } => {
            let bytes = read_candidate_bytes(&file)?;
            let descriptor = put_resolution_candidate(&ctx, &job_id, &bytes).await?;
            output_json(&descriptor)?;
        }
        ResolutionAction::Answer {
            job_id,
            defer,
            keep_unresolved,
            candidate,
        } => {
            let option = pick_answer_option(defer, keep_unresolved, candidate.is_some())?;
            let answer = bind_human_answer(&ctx, &job_id, option, candidate.as_deref()).await?;
            let recorded = answer_resolution(&ctx, answer).await?;
            if json {
                output_json(&recorded)?;
            } else {
                match recorded.chosen_option {
                    HumanResolutionOption::Defer => {
                        println!(
                            "Deferred resolution job {}.",
                            &job_id[..job_id.len().min(8)]
                        );
                    }
                    HumanResolutionOption::KeepUnresolved => {
                        println!(
                            "Recorded keep-unresolved for resolution job {}.",
                            &job_id[..job_id.len().min(8)]
                        );
                    }
                    HumanResolutionOption::SubmitCandidate => {
                        println!(
                            "Recorded candidate_ready result for job {}; run `feanorfs agent resolution apply {}` to publish.",
                            &job_id[..job_id.len().min(8)],
                            job_id
                        );
                    }
                }
            }
        }
        ResolutionAction::Defer { job_id } => {
            defer_resolution(&ctx, &job_id).await?;
            if json {
                output_json(&serde_json::Value::Null)?;
            } else {
                println!(
                    "Deferred resolution job {}.",
                    &job_id[..job_id.len().min(8)]
                );
            }
        }
        ResolutionAction::ProtocolStatus { rebuild } => {
            let status = resolution_protocol_status(&ctx, rebuild).await?;
            if json {
                output_json(&status)?;
            } else if status.entries.is_empty() {
                println!("No resolution protocol entries.");
            } else {
                for entry in &status.entries {
                    println!(
                        "Fingerprint {} job {} assignment {} attempt {} owner {} state {} outcome {:?} question_generation {}",
                        &entry.conflict_fingerprint[..entry.conflict_fingerprint.len().min(8)],
                        &entry.job_id[..entry.job_id.len().min(8)],
                        &entry.assignment_id[..entry.assignment_id.len().min(8)],
                        entry.attempt,
                        entry.owner,
                        entry.state.as_str(),
                        entry.outcome,
                        entry.question_generation
                    );
                }
            }
        }
        ResolutionAction::Assign { job_id } => {
            let message_id = send_resolution_assignment(&ctx, &job_id).await?;
            print_message_id(json, "Assigned resolution job", &job_id, &message_id)?;
        }
        ResolutionAction::Reply { job_id } => {
            let message_id = send_resolution_result(&ctx, &job_id).await?;
            print_message_id(
                json,
                "Published resolution result for job",
                &job_id,
                &message_id,
            )?;
        }
        ResolutionAction::Revoke { job_id, superseded } => {
            let message_id = send_resolution_revoke(&ctx, &job_id, superseded).await?;
            print_message_id(
                json,
                "Revoked resolution assignment for job",
                &job_id,
                &message_id,
            )?;
        }
        ResolutionAction::PublishAnswer {
            job_id,
            defer,
            keep_unresolved,
            candidate,
        } => {
            let option = pick_answer_option(defer, keep_unresolved, candidate.is_some())?;
            let mut answer = bind_human_answer(&ctx, &job_id, option, candidate.as_deref()).await?;
            if matches!(answer.chosen_option, HumanResolutionOption::SubmitCandidate) {
                // The `ffres1` profile requires verification evidence; the
                // answering machine cannot fabricate engine evidence, so the
                // published answer carries an explicit Unknown status (the
                // candidate descriptor itself was engine-validated by the
                // `put` step above).
                answer.verification = Some(VerificationSummary {
                    status: VerificationStatus::Unknown,
                    summary: "human submit_candidate answer; engine inline verification \
                              not executed on this machine"
                        .to_string(),
                    ..VerificationSummary::default()
                });
            }
            validate_human_resolution_answer(&answer)?;
            let message_id = send_human_answer(&ctx, &answer).await?;
            print_message_id(
                json,
                "Published human answer for resolution job",
                &job_id,
                &message_id,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{pick_answer_option, read_candidate_bytes};
    use feanorfs_common::HumanResolutionOption;

    #[test]
    fn answer_requires_exactly_one_typed_option() {
        assert_eq!(
            pick_answer_option(true, false, false).unwrap(),
            HumanResolutionOption::Defer
        );
        assert_eq!(
            pick_answer_option(false, true, false).unwrap(),
            HumanResolutionOption::KeepUnresolved
        );
        assert_eq!(
            pick_answer_option(false, false, true).unwrap(),
            HumanResolutionOption::SubmitCandidate
        );
        // Zero or multiple selections are refused; the caller can never
        // supply an ambiguous answer.
        assert!(pick_answer_option(false, false, false).is_err());
        assert!(pick_answer_option(true, true, false).is_err());
        assert!(pick_answer_option(true, false, true).is_err());
        assert!(pick_answer_option(false, true, true).is_err());
    }

    #[test]
    fn candidate_read_is_bounded_and_exact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.bin");
        std::fs::write(&path, b"reconciled content").unwrap();
        assert_eq!(
            read_candidate_bytes(path.to_str().unwrap()).unwrap(),
            b"reconciled content"
        );
        // A missing candidate file is a typed error, never a panic.
        let error = read_candidate_bytes("/nonexistent/definitely-missing").unwrap_err();
        assert!(error.to_string().contains("cannot read candidate file"));
    }
}
