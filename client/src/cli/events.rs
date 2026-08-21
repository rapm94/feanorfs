use feanorfs_client::commands::MirrorState;
use feanorfs_client::conflicts::load_last_synced_snapshot;
use feanorfs_client::lock::try_acquire_sync_lock;
use feanorfs_client::watch::event_warrants_sync;
use feanorfs_client::{do_status, load_config, SyncCtx};
use notify::Watcher;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

const EVENT_INBOX_LIMIT: usize = feanorfs_common::AGENT_INBOX_MAX_LIMIT;
const MAX_EMITTED_MESSAGE_IDS: usize = 10_000;

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum FeanorEvent {
    SyncState {
        mirror_state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot_id: Option<String>,
    },
    FolderChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot_id: Option<String>,
    },
    ConflictRisk {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot_id: Option<String>,
    },
    ConflictRegistered {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot_id: Option<String>,
    },
    AgentMessage {
        message_id: String,
        from: String,
        to: String,
        kind: String,
        about_snapshot: String,
    },
    AgentMessageCursorReset {
        cursor: String,
        cursor_reset: bool,
    },
    IntegratorAssigned(IntegratorEvent),
    IntegratorAccepted(IntegratorEvent),
    IntegratorCompleted(IntegratorEvent),
    IntegratorRequiresHuman(IntegratorEvent),
    IntegratorBlocked(IntegratorEvent),
    /// Metadata-only `ffwork1` lifecycle wakeups; deliberately omit paths,
    /// scopes, and bodies (IDs/state/counts only).
    WorkIntent(WorkEvent),
    WorkDecision(WorkEvent),
    WorkAmendment(WorkEvent),
    WorkYield(WorkEvent),
    WorkSettled(WorkEvent),
    WorkCompleted(WorkEvent),
    WorkBlocked(WorkEvent),
    WorkSuperseded(WorkEvent),
    /// Metadata-only automatic-resolution lifecycle wakeups derived from the
    /// local orchestrator store; deliberately omit paths, identities, and
    /// bodies (IDs/state/counts only).
    ResolutionPrepared(ResolutionEvent),
    ResolutionSubmitted(ResolutionEvent),
    ResolutionApplied(ResolutionEvent),
    ResolutionRevoked(ResolutionEvent),
    /// Metadata-only `ffres1` protocol wakeups derived from the reducer
    /// projection diff; deliberately omit paths, identities, and bodies
    /// (IDs/state/counts only).
    ResolutionAssigned(ResolutionEvent),
    ResolutionResultReceived(ResolutionEvent),
    ResolutionHumanAnswered(ResolutionEvent),
    /// Metadata-only continuous-reconciliation lifecycle wakeups.
    AgentReconcileStarted {
        agent: String,
        phase: String,
    },
    AgentReconciled {
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        settled_snapshot: Option<String>,
    },
    AgentReconcileDeferred {
        agent: String,
        deferred_count: u32,
    },
    AgentReconcileAttention {
        agent: String,
        reason: String,
    },
}

#[derive(Debug, Serialize)]
struct IntegratorEvent {
    message_id: String,
    from: String,
    to: String,
    kind: String,
    about_snapshot: String,
    assignment_id: String,
    attempt: u32,
}

/// Bounded metadata-only automatic-resolution wakeup payload; deliberately
/// omits paths, identities, and bodies (IDs/state/counts only).
#[derive(Debug, Serialize)]
struct ResolutionEvent {
    job_id: String,
    assignment_id: String,
    attempt: u32,
    owner: String,
    conflict_fingerprint: String,
    /// Assignment lifecycle state.
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    /// Total resolution jobs in the store (count only).
    job_count: u32,
}

/// Bounded metadata-only `ffwork1` wakeup payload; deliberately omits paths,
/// scopes, and bodies.
#[derive(Debug, Serialize)]
struct WorkEvent {
    message_id: String,
    from: String,
    to: String,
    kind: String,
    about_snapshot: String,
    task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proposal_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification: Option<String>,
}

struct EventPayload {
    path: Option<String>,
    mirror_state: Option<MirrorState>,
    snapshot_id: Option<String>,
}

/// Bounded wakeup record for one new agent signal; deliberately omits the body.
fn agent_message_event(message: &feanorfs_common::AgentMessage) -> FeanorEvent {
    FeanorEvent::AgentMessage {
        message_id: message.message_id.clone(),
        from: message.from.clone(),
        to: message.to.clone(),
        kind: message.kind.as_str().to_string(),
        about_snapshot: message.about_snapshot.clone(),
    }
}

/// Bounded metadata-only integrator lifecycle wakeup derived from an
/// `ffint1` profile; deliberately omits task bodies, decision details, and
/// paths.
fn integrator_event(message: &feanorfs_common::AgentMessage) -> Option<FeanorEvent> {
    let profile = feanorfs_common::parse_integrator_profile(&message.body)?;
    let (event, assignment_id, attempt) = match profile {
        feanorfs_common::IntegratorProfile::Assignment {
            assignment_id,
            attempt,
            ..
        } => ("integrator_assigned", assignment_id, attempt),
        feanorfs_common::IntegratorProfile::Accepted {
            assignment_id,
            attempt,
            ..
        } => ("integrator_accepted", assignment_id, attempt),
        feanorfs_common::IntegratorProfile::Result {
            assignment_id,
            attempt,
            digest,
            ..
        } => {
            let event = match digest.state {
                feanorfs_common::IntegratorOutcomeState::Completed => "integrator_completed",
                feanorfs_common::IntegratorOutcomeState::RequiresHuman => {
                    "integrator_requires_human"
                }
                _ => "integrator_blocked",
            };
            (event, assignment_id, attempt)
        }
        feanorfs_common::IntegratorProfile::Blocked {
            assignment_id,
            attempt,
            ..
        } => ("integrator_blocked", assignment_id, attempt),
    };
    let payload = IntegratorEvent {
        message_id: message.message_id.clone(),
        from: message.from.clone(),
        to: message.to.clone(),
        kind: message.kind.as_str().to_string(),
        about_snapshot: message.about_snapshot.clone(),
        assignment_id,
        attempt,
    };
    Some(match event {
        "integrator_assigned" => FeanorEvent::IntegratorAssigned(payload),
        "integrator_accepted" => FeanorEvent::IntegratorAccepted(payload),
        "integrator_completed" => FeanorEvent::IntegratorCompleted(payload),
        "integrator_requires_human" => FeanorEvent::IntegratorRequiresHuman(payload),
        "integrator_blocked" => FeanorEvent::IntegratorBlocked(payload),
        _ => unreachable!("integrator event names are closed above"),
    })
}

/// Bounded metadata-only `ffwork1` lifecycle wakeup derived from a profile;
/// deliberately omits paths, scopes, dependencies, and bodies.
fn work_event(message: &feanorfs_common::AgentMessage) -> Option<FeanorEvent> {
    use feanorfs_common::{WorkProfile, WorkTaskState};
    let profile = feanorfs_common::parse_work_profile(&message.body)?;
    let task_id = match profile {
        WorkProfile::WorkDecision(_) => String::new(),
        _ => profile.task_id().to_string(),
    };
    let base = WorkEvent {
        message_id: message.message_id.clone(),
        from: message.from.clone(),
        to: message.to.clone(),
        kind: message.kind.as_str().to_string(),
        about_snapshot: message.about_snapshot.clone(),
        task_id,
        agent: None,
        sequence: profile.sequence(),
        proposal_message_id: None,
        decision: None,
        verification: None,
    };
    let (event, payload) = match &profile {
        WorkProfile::WorkIntent(inner) => (
            "work_intent",
            WorkEvent {
                agent: Some(inner.agent.clone()),
                ..base
            },
        ),
        WorkProfile::WorkDecision(inner) => (
            "work_decision",
            WorkEvent {
                proposal_message_id: Some(inner.proposal_message_id.clone()),
                decision: Some(inner.kind.type_name().to_string()),
                ..base
            },
        ),
        WorkProfile::WorkAmendment(_) => ("work_amendment", base),
        WorkProfile::WorkYield(_) => ("work_yield", base),
        WorkProfile::WorkSettled(inner) => (
            "work_settled",
            WorkEvent {
                verification: Some(inner.verification.status.as_str().to_string()),
                ..base
            },
        ),
        WorkProfile::WorkCompleted(_) => ("work_completed", base),
        WorkProfile::WorkBlocked(_) => ("work_blocked", base),
        WorkProfile::WorkSuperseded(inner) => (
            "work_superseded",
            WorkEvent {
                proposal_message_id: Some(inner.proposal_message_id.clone()),
                ..base
            },
        ),
    };
    let _ = WorkTaskState::Proposed; // state names are derived by the reducer, not events
    Some(match event {
        "work_intent" => FeanorEvent::WorkIntent(payload),
        "work_decision" => FeanorEvent::WorkDecision(payload),
        "work_amendment" => FeanorEvent::WorkAmendment(payload),
        "work_yield" => FeanorEvent::WorkYield(payload),
        "work_settled" => FeanorEvent::WorkSettled(payload),
        "work_completed" => FeanorEvent::WorkCompleted(payload),
        "work_blocked" => FeanorEvent::WorkBlocked(payload),
        "work_superseded" => FeanorEvent::WorkSuperseded(payload),
        _ => unreachable!("work event names are closed above"),
    })
}

/// Projects the bounded resolution store into metadata-only lifecycle
/// wakeups, emitting on state transitions per job (prepared, submitted,
/// applied, revoked). Deliberately omits paths, identities, and bodies.
/// Pure per-job diff: which metadata-only wakeup, if any, a store projection
/// change expresses. `None` means no transition.
fn resolution_event_kind(
    previous: Option<&feanorfs_agent_core::ResolutionJobStatus>,
    job: &feanorfs_agent_core::ResolutionJobStatus,
) -> Option<&'static str> {
    use feanorfs_agent_core::ResolutionAssignmentState;
    match previous {
        None => Some("resolution_prepared"),
        Some(previous) if previous.assignment_state != job.assignment_state => {
            match job.assignment_state {
                ResolutionAssignmentState::Completed => Some("resolution_applied"),
                ResolutionAssignmentState::Revoked | ResolutionAssignmentState::Superseded => {
                    Some("resolution_revoked")
                }
                ResolutionAssignmentState::Active
                | ResolutionAssignmentState::PublicationUncertain
                | ResolutionAssignmentState::Stale
                | ResolutionAssignmentState::Deferred
                | ResolutionAssignmentState::KeepUnresolved => None,
            }
        }
        Some(previous) if previous.outcome != job.outcome && job.outcome.is_some() => {
            Some("resolution_submitted")
        }
        Some(_) => None,
    }
}

async fn project_resolution_state(
    ctx: &feanorfs_client::SyncCtx<'_>,
    last: &mut std::collections::HashMap<String, feanorfs_agent_core::ResolutionJobStatus>,
) {
    let Ok(projection) = feanorfs_agent_core::resolution_status(ctx, None).await else {
        return;
    };
    let job_count = projection.jobs.len() as u32;
    let mut seen = std::collections::HashSet::new();
    for job in projection.jobs {
        seen.insert(job.job_id.clone());
        let previous = last.get(&job.job_id);
        let kind = resolution_event_kind(previous, &job);
        if let Some(kind) = kind {
            let event = ResolutionEvent {
                job_id: job.job_id.clone(),
                assignment_id: job.assignment_id.clone(),
                attempt: job.attempt,
                owner: job.owner.clone(),
                conflict_fingerprint: job.conflict_fingerprint.clone(),
                state: assignment_state_str(job.assignment_state).to_string(),
                outcome: job.outcome.map(|outcome| outcome.as_str().to_string()),
                job_count,
            };
            let ev = match kind {
                "resolution_prepared" => FeanorEvent::ResolutionPrepared(event),
                "resolution_submitted" => FeanorEvent::ResolutionSubmitted(event),
                "resolution_applied" => FeanorEvent::ResolutionApplied(event),
                "resolution_revoked" => FeanorEvent::ResolutionRevoked(event),
                _ => unreachable!("resolution event kinds are closed above"),
            };
            emit_record(&ev);
        }
        last.insert(job.job_id.clone(), job.clone());
    }
    // Drop projections for jobs that left the bounded store (trim/cleanup).
    last.retain(|job_id, _| seen.contains(job_id));
}

/// Pure per-fingerprint diff over the `ffres1` reducer projection: which
/// metadata-only wakeup, if any, a projection change expresses. `None` means
/// no transition. First observation is `resolution_assigned`; lifecycle
/// transitions map to `resolution_result_received`, `resolution_human_answered`,
/// or `resolution_revoked`; repeated observations never re-emit.
fn protocol_resolution_event_kind(
    previous: Option<&feanorfs_agent_core::ResolutionProtocolEntryStatus>,
    entry: &feanorfs_agent_core::ResolutionProtocolEntryStatus,
) -> Option<&'static str> {
    use feanorfs_agent_core::ProtocolAssignmentState;
    match previous {
        None => Some("resolution_assigned"),
        Some(previous) if previous.state != entry.state => match entry.state {
            ProtocolAssignmentState::ResultReceived => Some("resolution_result_received"),
            ProtocolAssignmentState::HumanAnswered => Some("resolution_human_answered"),
            ProtocolAssignmentState::Revoked => Some("resolution_revoked"),
            ProtocolAssignmentState::Assigned => None,
        },
        Some(_) => None,
    }
}

/// Projects the `ffres1` reducer store into metadata-only protocol wakeups,
/// emitting on entry transitions (assigned, result_received,
/// human_answered, revoked). Deliberately omits paths, identities, bodies,
/// and question text (IDs/state/counts only).
async fn project_resolution_protocol_state(
    ctx: &feanorfs_client::SyncCtx<'_>,
    last: &mut std::collections::HashMap<
        String,
        feanorfs_agent_core::ResolutionProtocolEntryStatus,
    >,
) {
    let Ok(projection) = feanorfs_agent_core::resolution_protocol_status(ctx, false).await else {
        return;
    };
    let entry_count = projection.entries.len() as u32;
    let mut seen = std::collections::HashSet::new();
    for entry in projection.entries {
        seen.insert(entry.job_id.clone());
        let previous = last.get(&entry.job_id);
        let kind = protocol_resolution_event_kind(previous, &entry);
        if let Some(kind) = kind {
            let event = ResolutionEvent {
                job_id: entry.job_id.clone(),
                assignment_id: entry.assignment_id.clone(),
                attempt: entry.attempt,
                owner: entry.owner.clone(),
                conflict_fingerprint: entry.conflict_fingerprint.clone(),
                state: entry.state.as_str().to_string(),
                outcome: entry.outcome.map(|outcome| outcome.as_str().to_string()),
                job_count: entry_count,
            };
            let ev = match kind {
                "resolution_assigned" => FeanorEvent::ResolutionAssigned(event),
                "resolution_result_received" => FeanorEvent::ResolutionResultReceived(event),
                "resolution_human_answered" => FeanorEvent::ResolutionHumanAnswered(event),
                "resolution_revoked" => FeanorEvent::ResolutionRevoked(event),
                _ => unreachable!("protocol resolution event kinds are closed above"),
            };
            emit_record(&ev);
        }
        last.insert(entry.job_id.clone(), entry);
    }
    // Drop projections for entries that left the bounded reducer window.
    last.retain(|job_id, _| seen.contains(job_id));
}

fn assignment_state_str(state: feanorfs_agent_core::ResolutionAssignmentState) -> &'static str {
    use feanorfs_agent_core::ResolutionAssignmentState;
    match state {
        ResolutionAssignmentState::Active => "active",
        ResolutionAssignmentState::PublicationUncertain => "publication_uncertain",
        ResolutionAssignmentState::Revoked => "revoked",
        ResolutionAssignmentState::Superseded => "superseded",
        ResolutionAssignmentState::Completed => "completed",
        ResolutionAssignmentState::Stale => "stale",
        ResolutionAssignmentState::Deferred => "deferred",
        ResolutionAssignmentState::KeepUnresolved => "keep_unresolved",
    }
}

/// Bounded metadata-only notification that the signal cursor was reset or the
/// result was truncated; older wakeups may have been missed.
fn agent_message_cursor_reset_event(cursor: &str) -> FeanorEvent {
    FeanorEvent::AgentMessageCursorReset {
        cursor: cursor.to_string(),
        cursor_reset: true,
    }
}

fn update_snapshot_after_poll(
    current: &mut Option<String>,
    result: anyhow::Result<Option<String>>,
) -> bool {
    match result {
        Ok(snapshot) => {
            *current = snapshot;
            true
        }
        Err(error) => {
            tracing::warn!(
                "events poll: head read failed; preserving cursor and retrying: {error:#}"
            );
            false
        }
    }
}

fn remember_message_id(
    seen: &mut HashSet<String>,
    order: &mut VecDeque<String>,
    message_id: &str,
    capacity: usize,
) -> bool {
    if !seen.insert(message_id.to_string()) {
        return false;
    }
    order.push_back(message_id.to_string());
    while order.len() > capacity {
        if let Some(expired) = order.pop_front() {
            seen.remove(&expired);
        }
    }
    true
}

fn signal_events(
    signals: &feanorfs_common::AgentInboxResult,
    seen: &mut HashSet<String>,
    order: &mut VecDeque<String>,
) -> Vec<FeanorEvent> {
    let mut events = Vec::new();
    if signals.cursor_reset {
        events.push(agent_message_cursor_reset_event(&signals.cursor));
    }
    for message in &signals.messages {
        if remember_message_id(seen, order, &message.message_id, MAX_EMITTED_MESSAGE_IDS) {
            events.push(agent_message_event(message));
            if let Some(event) = integrator_event(message) {
                events.push(event);
            }
            if let Some(event) = work_event(message) {
                events.push(event);
            }
        }
    }
    events
}

pub async fn run_events(current_dir: &Path) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, current_dir, &config)?;
    let mut current_snapshot = api.get_head(&config.workspace_id).await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<std::path::PathBuf>>(100);
    let tx_clone = tx.clone();
    let watch_root = current_dir.to_path_buf();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if event_warrants_sync(&event) {
                    let _ = tx_clone.try_send(event.paths);
                }
            }
        })?;
    watcher.watch(&watch_root, notify::RecursiveMode::Recursive)?;

    if let Ok(status) = do_status(
        &api,
        &db,
        current_dir,
        &config.workspace_id,
        config.encryption_password.as_deref(),
    )
    .await
    {
        emit(
            "sync_state",
            EventPayload {
                path: None,
                mirror_state: Some(status.mirror_state),
                snapshot_id: current_snapshot.clone(),
            },
        );
    }

    // Reusable bounded head observer: a healthy hub wakes the loop on each
    // opaque head change; the 30-second window remains the recovery backstop.
    let mut head_observer = feanorfs_agent_core::HeadObserver::new(&api, &config.workspace_id);
    head_observer.acknowledge(current_snapshot.clone());
    let mut last_emitted_conflicts: HashSet<String> = HashSet::new();
    let mut last_signal_cursor: Option<String> = current_snapshot.clone();
    let mut last_emitted_messages: HashSet<String> = HashSet::new();
    let mut emitted_message_order = VecDeque::new();
    let mut last_live_phases: std::collections::HashMap<String, feanorfs_common::ContinuousPhase> =
        std::collections::HashMap::new();
    let mut last_resolution_jobs: std::collections::HashMap<
        String,
        feanorfs_agent_core::ResolutionJobStatus,
    > = std::collections::HashMap::new();
    let mut last_protocol_entries: std::collections::HashMap<
        String,
        feanorfs_agent_core::ResolutionProtocolEntryStatus,
    > = std::collections::HashMap::new();
    loop {
        tokio::select! {
            paths = rx.recv() => {
                if let Some(paths) = paths {
                    for p in paths {
                        if let Ok(rel) = p.strip_prefix(current_dir) {
                            emit("folder_changed", EventPayload {
                                path: rel.to_str().map(str::to_string),
                                mirror_state: None,
                                snapshot_id: current_snapshot.clone(),
                            });
                        }
                    }
                }
            }
            observation = head_observer.observe(Duration::from_secs(30)) => {
                // A head change (files or signals) wakes this loop
                // immediately; the periodic window is the backstop.
                if let Err(error) = observation {
                    tracing::warn!(?error, "events head observation failed; preserving state");
                }
                let _guard = try_acquire_sync_lock(current_dir, Duration::from_millis(200)).await;
                if _guard.is_err() {
                    continue;
                }
                let Ok(status) = do_status(
                    &api,
                    &db,
                    current_dir,
                    &config.workspace_id,
                    config.encryption_password.as_deref(),
                )
                .await
                else {
                    tracing::warn!("events poll: status check failed; will retry");
                    continue;
                };
                if !update_snapshot_after_poll(
                    &mut current_snapshot,
                    api.get_head(&config.workspace_id).await,
                ) {
                    continue;
                }
                head_observer.acknowledge(current_snapshot.clone());
                emit("sync_state", EventPayload {
                    path: None,
                    mirror_state: Some(status.mirror_state),
                    snapshot_id: current_snapshot.clone(),
                });

                let last = load_last_synced_snapshot(&ctx).await.unwrap_or_default();
                let pending_set: HashSet<&String> = status.pending_conflicts.iter().collect();
                for remote in &status.download_required {
                    if pending_set.contains(&remote.path) {
                        continue;
                    }
                    let Some(agreed) = last.get(&remote.path) else {
                        continue;
                    };
                    let local = status.local_files.get(&remote.path);
                    if local.is_some_and(|l| l.hash == agreed.hash && !l.deleted)
                        && remote.hash != agreed.hash
                    {
                        emit("conflict_risk", EventPayload {
                            path: Some(remote.path.clone()),
                            mirror_state: None,
                            snapshot_id: current_snapshot.clone(),
                        });
                    }
                }

                let new_conflicts: Vec<String> = status
                    .pending_conflicts
                    .iter()
                    .filter(|p| !last_emitted_conflicts.contains(*p))
                    .cloned()
                    .collect();
                for p in new_conflicts {
                    last_emitted_conflicts.insert(p.clone());
                    emit("conflict_registered", EventPayload {
                        path: Some(p),
                        mirror_state: None,
                        snapshot_id: current_snapshot.clone(),
                    });
                }
                last_emitted_conflicts.retain(|p| status.pending_conflicts.contains(p));

                if let Ok(signals) = feanorfs_agent_core::signals_since(
                    &ctx,
                    last_signal_cursor.as_deref(),
                    EVENT_INBOX_LIMIT,
                )
                .await
                {
                    if signals.cursor_reset {
                        tracing::warn!(
                            "events poll: signal cursor reset or bounded result overflow; older wakeups may have been missed"
                        );
                    }
                    for event in signal_events(
                        &signals,
                        &mut last_emitted_messages,
                        &mut emitted_message_order,
                    ) {
                        emit_record(&event);
                    }
                    if !signals.cursor.is_empty() {
                        last_signal_cursor = Some(signals.cursor);
                    }
                }

                project_live_reconciliation(current_dir, &mut last_live_phases);
                project_resolution_state(&ctx, &mut last_resolution_jobs).await;
                project_resolution_protocol_state(&ctx, &mut last_protocol_entries).await;
            }
        }
    }
}

/// Projects the bounded continuous-status files into metadata-only
/// lifecycle events, emitting on phase transitions per agent.
fn project_live_reconciliation(
    current_dir: &Path,
    last: &mut std::collections::HashMap<String, feanorfs_common::ContinuousPhase>,
) {
    let Ok(agents) = feanorfs_agent_core::agents_dir(current_dir) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&agents) else {
        return;
    };
    let mut seen = std::collections::HashSet::new();
    for entry in entries.flatten().take(1000) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        seen.insert(name.clone());
        let status = match feanorfs_agent_core::live_continuous_status(current_dir, &name) {
            Ok(Some(status)) if status.active => status,
            _ => {
                // Forget stopped/stale owners so reactivation in the same
                // phase emits a fresh lifecycle event.
                last.remove(&name);
                continue;
            }
        };
        let previous = last.insert(name.clone(), status.phase);
        if previous == Some(status.phase) {
            continue;
        }
        let event = match status.phase {
            feanorfs_common::ContinuousPhase::ReconcilingLocal
            | feanorfs_common::ContinuousPhase::RefreshingRemote => {
                Some(FeanorEvent::AgentReconcileStarted {
                    agent: name.clone(),
                    phase: status.phase.as_str().to_string(),
                })
            }
            feanorfs_common::ContinuousPhase::Idle => Some(FeanorEvent::AgentReconciled {
                agent: name.clone(),
                settled_snapshot: status.settled_snapshot.clone(),
            }),
            feanorfs_common::ContinuousPhase::NeedsAttention => {
                Some(FeanorEvent::AgentReconcileAttention {
                    agent: name.clone(),
                    reason: status
                        .attention
                        .as_ref()
                        .map(|attention| attention.reason.clone())
                        .unwrap_or_else(|| "attention".to_string()),
                })
            }
            _ => None,
        };
        if let Some(event) = event {
            emit_record(&event);
        }
        if status.deferred_count > 0 && status.phase != feanorfs_common::ContinuousPhase::Idle {
            emit_record(&FeanorEvent::AgentReconcileDeferred {
                agent: name.clone(),
                deferred_count: status.deferred_count,
            });
        }
    }
    // Drop projections for agents that no longer exist.
    last.retain(|name, _| seen.contains(name));
}

fn emit(event: &'static str, payload: EventPayload) {
    let mirror_str = payload.mirror_state.map(|s| {
        serde_json::to_value(s)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "idle".into())
    });
    let ev = match event {
        "sync_state" => FeanorEvent::SyncState {
            mirror_state: mirror_str.unwrap_or_else(|| "idle".into()),
            snapshot_id: payload.snapshot_id,
        },
        "folder_changed" => FeanorEvent::FolderChanged {
            path: payload.path,
            snapshot_id: payload.snapshot_id,
        },
        "conflict_risk" => FeanorEvent::ConflictRisk {
            path: payload.path.expect("conflict risk requires a path"),
            snapshot_id: payload.snapshot_id,
        },
        "conflict_registered" => FeanorEvent::ConflictRegistered {
            path: payload.path.expect("registered conflict requires a path"),
            snapshot_id: payload.snapshot_id,
        },
        _ => unreachable!("ordinary event names are closed at call sites"),
    };
    emit_record(&ev);
}

fn emit_record(ev: &FeanorEvent) {
    if let Ok(line) = serde_json::to_string(ev) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agent_message_cursor_reset_event, agent_message_event, assignment_state_str,
        integrator_event, project_live_reconciliation, protocol_resolution_event_kind,
        remember_message_id, resolution_event_kind, signal_events, update_snapshot_after_poll,
        work_event, FeanorEvent, ResolutionEvent,
    };
    use feanorfs_agent_core::{ResolutionAssignmentState, ResolutionJobStatus};
    use feanorfs_common::ResolutionOutcome;
    use feanorfs_common::{
        encode_integrator_profile, AgentInboxResult, AgentMessage, AgentMessageKind,
        IntegratorProfile,
    };
    use std::collections::{HashMap, HashSet, VecDeque};

    #[test]
    fn agent_message_event_is_bounded_metadata_without_body() {
        let message = AgentMessage {
            message_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            from: "linux-dev".into(),
            to: "mac-test".into(),
            kind: AgentMessageKind::Request,
            body: "Run iOS simulator tests".into(),
            about_snapshot: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .into(),
            reply_to: None,
            created_at_ms: 1_785_852_000_000,
        };
        let line = serde_json::to_string(&agent_message_event(&message)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["event"], "agent_message");
        assert_eq!(
            value["message_id"],
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(value["from"], "linux-dev");
        assert_eq!(value["to"], "mac-test");
        assert_eq!(value["kind"], "request");
        assert_eq!(
            value["about_snapshot"],
            "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
        );
        assert!(
            value.get("body").is_none(),
            "events must never carry bodies"
        );
        assert!(value.get("reply_to").is_none());
        assert!(value.get("created_at_ms").is_none());
        assert!(value.get("snapshot_id").is_none());
    }

    #[test]
    fn ordinary_events_omit_agent_message_fields() {
        let ev = FeanorEvent::SyncState {
            mirror_state: "syncing".into(),
            snapshot_id: Some("abc".into()),
        };
        let value: serde_json::Value = serde_json::to_value(ev).unwrap();
        assert!(value.get("cursor").is_none());
        assert!(value.get("cursor_reset").is_none());
        assert!(value.get("message_id").is_none());
        assert!(value.get("from").is_none());
        assert!(value.get("to").is_none());
        assert!(value.get("kind").is_none());
        assert!(value.get("about_snapshot").is_none());
        assert_eq!(value["snapshot_id"], "abc");
    }

    #[test]
    fn cursor_reset_event_is_bounded_metadata() {
        let cursor = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let value = serde_json::to_value(agent_message_cursor_reset_event(cursor)).unwrap();
        assert_eq!(value["event"], "agent_message_cursor_reset");
        assert_eq!(value["cursor"], cursor);
        assert_eq!(value["cursor_reset"], true);
        for forbidden in [
            "body",
            "message_id",
            "from",
            "to",
            "kind",
            "about_snapshot",
            "path",
            "snapshot_id",
            "assignment_id",
            "attempt",
            "mirror_state",
        ] {
            assert!(
                value.get(forbidden).is_none(),
                "cursor reset events must omit {forbidden}"
            );
        }
    }

    #[test]
    fn event_variants_preserve_the_shipped_ndjson_shapes() {
        assert_eq!(
            serde_json::to_string(&FeanorEvent::SyncState {
                mirror_state: "idle".into(),
                snapshot_id: Some("abc".into()),
            })
            .unwrap(),
            r#"{"event":"sync_state","mirror_state":"idle","snapshot_id":"abc"}"#
        );
        assert_eq!(
            serde_json::to_string(&FeanorEvent::FolderChanged {
                path: Some("src/lib.rs".into()),
                snapshot_id: None,
            })
            .unwrap(),
            r#"{"event":"folder_changed","path":"src/lib.rs"}"#
        );
        assert_eq!(
            serde_json::to_string(&agent_message_cursor_reset_event("abc")).unwrap(),
            r#"{"event":"agent_message_cursor_reset","cursor":"abc","cursor_reset":true}"#
        );
        assert_eq!(
            serde_json::to_string(&FeanorEvent::AgentReconcileStarted {
                agent: "worker".into(),
                phase: feanorfs_common::ContinuousPhase::RefreshingRemote
                    .as_str()
                    .to_string(),
            })
            .unwrap(),
            r#"{"event":"agent_reconcile_started","agent":"worker","phase":"refreshing_remote"}"#
        );
    }

    #[test]
    fn stopped_live_owner_is_forgotten_before_same_phase_reactivation() {
        let base = tempfile::tempdir().unwrap();
        let state = feanorfs_agent_core::agents_dir(base.path())
            .unwrap()
            .join("worker")
            .join("state");
        std::fs::create_dir_all(&state).unwrap();
        let _owner = feanorfs_agent_core::ContinuousOwnerLock::try_acquire(base.path(), "worker")
            .unwrap()
            .expect("interactive owner");
        let mut status = feanorfs_agent_core::build_status(
            "worker",
            true,
            feanorfs_common::ContinuousPhase::Idle,
            Some("a".repeat(64)),
            Some("b".repeat(64)),
            Some("a".repeat(64)),
            false,
            0,
            None,
            Some(std::process::id()),
            None,
        );
        feanorfs_agent_core::write_continuous_status(base.path(), "worker", &status).unwrap();

        let mut phases = HashMap::new();
        project_live_reconciliation(base.path(), &mut phases);
        assert_eq!(
            phases.get("worker"),
            Some(&feanorfs_common::ContinuousPhase::Idle)
        );

        status.active = false;
        feanorfs_agent_core::write_continuous_status(base.path(), "worker", &status).unwrap();
        project_live_reconciliation(base.path(), &mut phases);
        assert!(!phases.contains_key("worker"));

        status.active = true;
        feanorfs_agent_core::write_continuous_status(base.path(), "worker", &status).unwrap();
        project_live_reconciliation(base.path(), &mut phases);
        assert_eq!(
            phases.get("worker"),
            Some(&feanorfs_common::ContinuousPhase::Idle)
        );
    }

    #[test]
    fn transient_head_failure_preserves_the_last_snapshot() {
        let mut current = Some("previous".to_string());
        assert!(!update_snapshot_after_poll(
            &mut current,
            Err(anyhow::anyhow!("temporary head failure")),
        ));
        assert_eq!(current.as_deref(), Some("previous"));
        assert!(update_snapshot_after_poll(
            &mut current,
            Ok(Some("recovered".to_string())),
        ));
        assert_eq!(current.as_deref(), Some("recovered"));
    }

    #[test]
    fn cursor_reset_is_emitted_before_associated_wakeups() {
        let message = AgentMessage {
            message_id: "a".repeat(64),
            from: "human".into(),
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "work".into(),
            about_snapshot: "b".repeat(64),
            reply_to: None,
            created_at_ms: 1,
        };
        let mut seen = HashSet::new();
        let mut order = VecDeque::new();
        let events = signal_events(
            &AgentInboxResult {
                cursor: "c".repeat(64),
                cursor_reset: true,
                messages: vec![message],
            },
            &mut seen,
            &mut order,
        );
        let values = events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["event"], "agent_message_cursor_reset");
        assert_eq!(values[1]["event"], "agent_message");
    }

    #[test]
    fn agent_message_wakeup_deduplication_is_bounded() {
        let mut seen = HashSet::new();
        let mut order = VecDeque::new();

        assert!(remember_message_id(&mut seen, &mut order, "a", 2));
        assert!(!remember_message_id(&mut seen, &mut order, "a", 2));
        assert!(remember_message_id(&mut seen, &mut order, "b", 2));
        assert!(remember_message_id(&mut seen, &mut order, "c", 2));
        assert_eq!(order, VecDeque::from(["b".to_string(), "c".to_string()]));
        assert_eq!(seen, HashSet::from(["b".to_string(), "c".to_string()]));
        assert!(
            remember_message_id(&mut seen, &mut order, "a", 2),
            "an id may be emitted again only after bounded-cache eviction"
        );
    }

    #[test]
    fn integrator_events_are_typed_metadata_without_bodies() {
        let profile = IntegratorProfile::Assignment {
            assignment_id: "0123456789abcdef0123456789abcdef".into(),
            attempt: 0,
            selected: "agent-b".into(),
            about_snapshot: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .into(),
            roster_fingerprint: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .into(),
            neutral_integrator: true,
            task: "Integrate parser implementation and tests".into(),
        };
        let message = AgentMessage {
            message_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            from: "human".into(),
            to: "agent-b".into(),
            kind: AgentMessageKind::Request,
            body: encode_integrator_profile(&profile).unwrap(),
            about_snapshot: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .into(),
            reply_to: None,
            created_at_ms: 1_785_852_000_000,
        };
        let event = integrator_event(&message).expect("ffint1 assignment must produce an event");
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "integrator_assigned");
        assert_eq!(value["assignment_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(value["attempt"], 0);
        assert_eq!(value["message_id"], message.message_id);
        assert_eq!(value["from"], "human");
        assert_eq!(value["to"], "agent-b");
        assert_eq!(value["kind"], "request");
        assert_eq!(value["about_snapshot"], message.about_snapshot);
        for forbidden in ["body", "task", "selected", "roster_fingerprint", "reply_to"] {
            assert!(
                value.get(forbidden).is_none(),
                "integrator events must omit {forbidden}"
            );
        }

        // Plain ffmsg1 bodies still emit the generic wakeup.
        let mut plain = message;
        plain.body = "ordinary signal".into();
        assert!(integrator_event(&plain).is_none());
        assert!(matches!(
            agent_message_event(&plain),
            FeanorEvent::AgentMessage { .. }
        ));
    }

    #[test]
    fn work_events_are_typed_metadata_without_paths_or_scopes() {
        use feanorfs_common::{WorkIntentProfile, WorkProfile};
        let profile = WorkProfile::WorkIntent(WorkIntentProfile {
            task_id: "parser-impl".into(),
            agent: "linux-dev".into(),
            sequence: 1,
            causal_base: None,
            coordinator: Some("human".into()),
            paths: vec!["src/parser.rs".into(), "tests/parser.rs".into()],
            concerns: vec!["parser behavior".into()],
            dependencies: vec![],
            capabilities: vec!["rust".into()],
        });
        let message = AgentMessage {
            message_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            from: "linux-dev".into(),
            to: "human".into(),
            kind: AgentMessageKind::Request,
            body: feanorfs_common::encode_work_profile(&profile).unwrap(),
            about_snapshot: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .into(),
            reply_to: None,
            created_at_ms: 1_785_852_000_000,
        };
        let event = work_event(&message).expect("ffwork1 intent must produce an event");
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "work_intent");
        assert_eq!(value["task_id"], "parser-impl");
        assert_eq!(value["agent"], "linux-dev");
        assert_eq!(value["sequence"], 1);
        assert_eq!(value["message_id"], message.message_id);
        assert_eq!(value["kind"], "request");
        // Deliberately omits full path sets, scopes, and bodies.
        for forbidden in [
            "body",
            "paths",
            "concerns",
            "dependencies",
            "capabilities",
            "coordinator",
            "reply_to",
        ] {
            assert!(
                value.get(forbidden).is_none(),
                "work events must omit {forbidden}"
            );
        }

        // Plain ffmsg1 bodies produce no work event.
        let mut plain = message;
        plain.body = "ordinary signal".into();
        assert!(work_event(&plain).is_none());
    }

    fn job_status(
        job_id: &str,
        state: ResolutionAssignmentState,
        outcome: Option<ResolutionOutcome>,
    ) -> ResolutionJobStatus {
        ResolutionJobStatus {
            job_id: job_id.to_string(),
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            attempt: 0,
            owner: "agent-b".to_string(),
            conflict_fingerprint: "c".repeat(64),
            assignment_state: state,
            outcome,
            question_generation: 0,
            created_at_ms: 1,
            verified_at_ms: None,
        }
    }

    fn protocol_entry(
        job_id: &str,
        state: feanorfs_agent_core::ProtocolAssignmentState,
    ) -> feanorfs_agent_core::ResolutionProtocolEntryStatus {
        feanorfs_agent_core::ResolutionProtocolEntryStatus {
            conflict_fingerprint: "c".repeat(64),
            job_id: job_id.to_string(),
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            attempt: 0,
            owner: "agent-b".to_string(),
            state,
            question_generation: 0,
            outcome: None,
            question: None,
        }
    }

    #[test]
    fn resolution_event_kind_diffs_prepared_submitted_applied_revoked() {
        let job_id = "fedcba9876543210fedcba9876543210";
        // New job -> prepared.
        assert_eq!(
            resolution_event_kind(
                None,
                &job_status(job_id, ResolutionAssignmentState::Active, None)
            ),
            Some("resolution_prepared")
        );
        // Result recorded -> submitted.
        let active = job_status(job_id, ResolutionAssignmentState::Active, None);
        let submitted = job_status(
            job_id,
            ResolutionAssignmentState::Active,
            Some(ResolutionOutcome::CandidateReady),
        );
        assert_eq!(
            resolution_event_kind(Some(&active), &submitted),
            Some("resolution_submitted")
        );
        // Repeated poll with no change -> no event.
        assert_eq!(resolution_event_kind(Some(&submitted), &submitted), None);
        // Completed after publish -> applied.
        let completed = job_status(
            job_id,
            ResolutionAssignmentState::Completed,
            Some(ResolutionOutcome::CandidateReady),
        );
        assert_eq!(
            resolution_event_kind(Some(&submitted), &completed),
            Some("resolution_applied")
        );
        // Revoked/superseded -> revoked.
        for state in [
            ResolutionAssignmentState::Revoked,
            ResolutionAssignmentState::Superseded,
        ] {
            assert_eq!(
                resolution_event_kind(Some(&active), &job_status(job_id, state, None)),
                Some("resolution_revoked")
            );
        }
    }

    #[test]
    fn resolution_events_are_metadata_only_without_paths_or_bodies() {
        let event = FeanorEvent::ResolutionPrepared(ResolutionEvent {
            job_id: "fedcba9876543210fedcba9876543210".to_string(),
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            attempt: 0,
            owner: "agent-b".to_string(),
            conflict_fingerprint: "c".repeat(64),
            state: assignment_state_str(ResolutionAssignmentState::Active).to_string(),
            outcome: None,
            job_count: 3,
        });
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "resolution_prepared");
        assert_eq!(value["job_id"], "fedcba9876543210fedcba9876543210");
        assert_eq!(value["assignment_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(value["attempt"], 0);
        assert_eq!(value["owner"], "agent-b");
        assert_eq!(value["state"], "active");
        assert_eq!(value["job_count"], 3);
        for forbidden in [
            "path",
            "body",
            "candidate",
            "identity",
            "conflict",
            "artifacts",
            "accepted_intents",
            "causal_refs",
            "prevention",
            "question",
            "diagnostics",
            "verification",
            "snapshot_id",
            "about_snapshot",
        ] {
            assert!(
                value.get(forbidden).is_none(),
                "resolution events must omit {forbidden}"
            );
        }

        let submitted = FeanorEvent::ResolutionSubmitted(ResolutionEvent {
            job_id: "fedcba9876543210fedcba9876543210".to_string(),
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            attempt: 0,
            owner: "agent-b".to_string(),
            conflict_fingerprint: "c".repeat(64),
            state: assignment_state_str(ResolutionAssignmentState::Active).to_string(),
            outcome: Some(ResolutionOutcome::CandidateReady.as_str().to_string()),
            job_count: 1,
        });
        let value = serde_json::to_value(submitted).unwrap();
        assert_eq!(value["event"], "resolution_submitted");
        assert_eq!(value["outcome"], "candidate_ready");
        assert!(value.get("candidate").is_none());
    }

    #[test]
    fn protocol_resolution_event_kind_diffs_assigned_received_answered_revoked() {
        use feanorfs_agent_core::ProtocolAssignmentState;
        let job_id = "fedcba9876543210fedcba9876543210";
        // New entry -> assigned.
        let assigned = protocol_entry(job_id, ProtocolAssignmentState::Assigned);
        assert_eq!(
            protocol_resolution_event_kind(None, &assigned),
            Some("resolution_assigned")
        );
        // Repeated observation with no change -> no event.
        assert_eq!(
            protocol_resolution_event_kind(Some(&assigned), &assigned),
            None
        );
        // Result observed -> result_received.
        let received = protocol_entry(job_id, ProtocolAssignmentState::ResultReceived);
        assert_eq!(
            protocol_resolution_event_kind(Some(&assigned), &received),
            Some("resolution_result_received")
        );
        // Human answer observed -> human_answered.
        let answered = protocol_entry(job_id, ProtocolAssignmentState::HumanAnswered);
        assert_eq!(
            protocol_resolution_event_kind(Some(&received), &answered),
            Some("resolution_human_answered")
        );
        // Revoked -> revoked.
        let revoked = protocol_entry(job_id, ProtocolAssignmentState::Revoked);
        assert_eq!(
            protocol_resolution_event_kind(Some(&assigned), &revoked),
            Some("resolution_revoked")
        );
        // A back-transition to Assigned on the same entry is not a wakeup.
        assert_eq!(
            protocol_resolution_event_kind(Some(&revoked), &assigned),
            None
        );
    }

    #[test]
    fn protocol_resolution_events_are_metadata_only_without_question_text() {
        use feanorfs_agent_core::ProtocolAssignmentState;
        for (variant, expected) in [
            (
                FeanorEvent::ResolutionAssigned(ResolutionEvent {
                    job_id: "fedcba9876543210fedcba9876543210".to_string(),
                    assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
                    attempt: 0,
                    owner: "agent-b".to_string(),
                    conflict_fingerprint: "c".repeat(64),
                    state: ProtocolAssignmentState::Assigned.as_str().to_string(),
                    outcome: None,
                    job_count: 2,
                }),
                "resolution_assigned",
            ),
            (
                FeanorEvent::ResolutionResultReceived(ResolutionEvent {
                    job_id: "fedcba9876543210fedcba9876543210".to_string(),
                    assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
                    attempt: 0,
                    owner: "agent-b".to_string(),
                    conflict_fingerprint: "c".repeat(64),
                    state: ProtocolAssignmentState::ResultReceived.as_str().to_string(),
                    outcome: Some(ResolutionOutcome::RequiresHuman.as_str().to_string()),
                    job_count: 2,
                }),
                "resolution_result_received",
            ),
            (
                FeanorEvent::ResolutionHumanAnswered(ResolutionEvent {
                    job_id: "fedcba9876543210fedcba9876543210".to_string(),
                    assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
                    attempt: 0,
                    owner: "agent-b".to_string(),
                    conflict_fingerprint: "c".repeat(64),
                    state: ProtocolAssignmentState::HumanAnswered.as_str().to_string(),
                    outcome: Some(ResolutionOutcome::RequiresHuman.as_str().to_string()),
                    job_count: 2,
                }),
                "resolution_human_answered",
            ),
        ] {
            let value = serde_json::to_value(variant).unwrap();
            assert_eq!(value["event"], expected);
            assert_eq!(value["conflict_fingerprint"], "c".repeat(64));
            assert_eq!(value["job_count"], 2);
            for forbidden in [
                "path",
                "body",
                "candidate",
                "identity",
                "conflict",
                "artifacts",
                "prevention",
                "question",
                "question_generation",
                "diagnostics",
                "verification",
                "snapshot_id",
                "about_snapshot",
            ] {
                assert!(
                    value.get(forbidden).is_none(),
                    "protocol resolution events must omit {forbidden}"
                );
            }
        }

        // The revoked protocol wakeup reuses the same bounded wire kind as
        // the local-store revocation.
        let revoked = FeanorEvent::ResolutionRevoked(ResolutionEvent {
            job_id: "fedcba9876543210fedcba9876543210".to_string(),
            assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
            attempt: 0,
            owner: "agent-b".to_string(),
            conflict_fingerprint: "c".repeat(64),
            state: ProtocolAssignmentState::Revoked.as_str().to_string(),
            outcome: None,
            job_count: 1,
        });
        assert_eq!(
            serde_json::to_value(revoked).unwrap()["event"],
            "resolution_revoked"
        );
    }
}
