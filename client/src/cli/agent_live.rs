//! Continuous reconciliation controller for one active agent worktree.
//!
//! One controller owns one `(workspace, agent)` pair for a process lifetime.
//! It watches the agent worktree with the same bounded/debounced discipline
//! as the normal workspace watcher, waits on the opaque workspace head, and
//! drives the existing land/refresh machinery automatically:
//!
//! ```text
//! fs burst (500 ms quiet) -> bounded dirty generation -> automatic land
//! head change (tree root differs) -> shared sync -> automatic refresh
//! signal-only head (same tree root)  -> wakeup only, zero file work
//! ```
//!
//! The controller never merges content, never chooses conflict winners, and
//! never activates a dormant agent: only `agent run` (interactive) or an
//! enabled configured runner (worker-owned) start it.

use anyhow::{ensure, Context as _, Result};
pub use feanorfs_agent_core::live_reconciliation_health;
use feanorfs_agent_core::{
    build_status, classify_continuous_error, land_agent_continuous, land_agent_continuous_scoped,
    land_agent_runner_owned, land_agent_runner_owned_scoped, partition_agent_scope,
    probe_agent_state, refresh_agent_continuous, refresh_agent_runner_owned, verify_agent_worktree,
    write_continuous_status, AcceptedWorkDescriptor, ContinuousErrorClass, ContinuousOwnerLock,
    HeadObserver, RunnerOwnership, RunnerWorkWait, RunnerWorkWaitKind, ScopeChangePublishState,
};
use feanorfs_common::{
    work_contract::{
        encode_scope_change_request, ScopeChangeOperation, ScopeChangeRequestProfile,
        WORK_MAX_SCOPE_CHANGE_OPERATIONS,
    },
    ContinuousAgentStatus, ContinuousAttention, ContinuousPhase, WorkStatusInput, WorkTaskState,
    WORK_MAX_PATHS,
};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

use super::process_tree;
use feanorfs_client::backoff::{BackoffGrowth, BackoffReset, ExponentialBackoff};
use feanorfs_client::watch::event_warrants_sync_under;
use notify::Watcher;

/// Quiet period after the last filesystem event before reconciliation starts.
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);
/// Bounded event channel; saturation keeps a dirty bit instead of growing.
const EVENT_CHANNEL_BOUND: usize = 100;
/// Longest one controller spend waiting for a remote head change; the
/// periodic pass remains the recovery backstop on unsupported hubs.
const HEAD_WAIT_WINDOW: Duration = Duration::from_secs(30);
/// Retry backoff after retryable transport failures: base 1 s doubling from
/// the first failure, 60 s cap. Sequence (failures 0..): 1, 2, 4, 8, 16, 32,
/// 60, 60, ... Callers always increment before asking, so the zero-failure
/// base delay only matters for direct unit use.
const LIVE_BACKOFF: ExponentialBackoff =
    ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60))
        .with_growth(BackoffGrowth::DoublesFromFirstFailure)
        .with_reset(BackoffReset::Base);
/// Bounded final-reconciliation budget on exit/flush.
const FINAL_FLUSH_BUDGET: Duration = Duration::from_secs(30);

/// Honest terminal outcome of one controller lifecycle (CAD-10): settled,
/// retryably offline, or needs attention.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LiveFinalOutcome {
    /// Every local generation reconciled and the tree is settled.
    pub settled: bool,
    /// Retryably offline: local work remains and nothing was lost.
    pub offline: bool,
    /// Fail-closed attention requiring explicit action.
    pub attention: Option<ContinuousAttention>,
    /// Latest reachable settled snapshot, when known.
    pub settled_snapshot: Option<String>,
    pub deferred_count: u32,
    /// Interactive child exit code, when the controller owned a child.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_exit: Option<i32>,
}

enum Owner {
    Interactive(ContinuousOwnerLock),
    Runner(RunnerOwnership),
}

struct ControllerCore {
    base: PathBuf,
    agent: String,
    owner: Owner,
    api: Arc<feanorfs_agent_core::ApiClient>,
    db: feanorfs_agent_core::ClientDb,
    workspace_id: String,
    format_version: u32,
    password: Option<String>,
    observer: HeadObserver<'static>,
    phase: ContinuousPhase,
    observed_head: Option<String>,
    observed_tree: Option<String>,
    settled_snapshot: Option<String>,
    pending_local: bool,
    pending_remote: bool,
    deferred_count: u32,
    attention: Option<ContinuousAttention>,
    offline_failures: u32,
    retry_at: Option<tokio::time::Instant>,
    owner_pid: u32,
    owner_start_id: Option<String>,
    active: bool,
    /// Agent base snapshot of the most recent authoritative probe; the
    /// scope-change operation derivation compares the worktree against it.
    probe_agent_base: Option<String>,
    /// Dedup key of the last published scope-change request. Interactive
    /// controllers persist it durably (surviving `agent run` restarts);
    /// runner-owned controllers persist the record in runner state instead.
    published_scope_change: Option<PublishedScopeChange>,
}

/// One deduplicated scope-change request key. The pre-publish gate compares
/// the full tuple (task, intent, fingerprint) — never the fingerprint alone —
/// so the same path set under a different task/intent or a different
/// operation set is not deduped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedScopeChange {
    task_id: String,
    intent_message_id: String,
    fingerprint: String,
    /// Message id of the sent request; `None` while publish-pending (the
    /// record is persisted before the send). A restart that finds a
    /// publish-pending record fails closed and never republishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
}

/// Accepted-work guard for one automatic land.
enum LandGuard {
    /// No accepted scope (legacy/advisory or no work projection): land
    /// all-path exactly as before.
    AllPath,
    /// Enforced runner without active accepted work: nothing may publish.
    Skip,
    /// Guard automatic land with this accepted scope.
    Scoped {
        scope: feanorfs_common::WorkScope,
        task_id: String,
        intent_message_id: String,
        coordinator: Option<String>,
    },
    /// Fail-closed refusal: no land, typed attention entered (superseded
    /// generation, incomplete projection, or unknown send outcome).
    Refused { reason: String, detail: String },
}

/// Typed reason a runner-owned land is refused before any mutation.
enum LandRefusal {
    /// The reducer projection is incomplete; acceptance cannot be proven.
    ProjectionIncomplete,
    /// The pinned generation is absent from the projection.
    GenerationMissing,
    /// The pinned generation was superseded or reached a terminal state.
    Superseded,
    /// The projection could not be read at all.
    UnreadableProjection(String),
}

impl LandRefusal {
    fn reason(&self) -> &'static str {
        match self {
            Self::ProjectionIncomplete => "projection_incomplete",
            Self::GenerationMissing | Self::Superseded => "superseded_intent",
            Self::UnreadableProjection(_) => "corrupt_state",
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::ProjectionIncomplete => {
                "the work projection is incomplete; the pinned accepted generation cannot be \
                 proven current"
                    .to_string()
            }
            Self::GenerationMissing => {
                "the pinned accepted-work generation is missing from the work projection"
                    .to_string()
            }
            Self::Superseded => {
                "the pinned accepted-work generation was superseded or reached a terminal state; \
                 refusing the land"
                    .to_string()
            }
            Self::UnreadableProjection(detail) => detail.clone(),
        }
    }
}

/// Pure re-validation of a pinned accepted-work generation against a fresh
/// work projection. The pinned (task, agent, intent, sequence) generation
/// must still be the current accepted generation: an incomplete projection,
/// a missing task or proposal, a terminal state, or a newer accepted
/// generation for the same (task, agent) refuses the land fail-closed.
fn revalidate_pinned_generation(
    projection: &feanorfs_common::WorkStatusResult,
    descriptor: &AcceptedWorkDescriptor,
) -> std::result::Result<(), LandRefusal> {
    if projection.projection_incomplete {
        return Err(LandRefusal::ProjectionIncomplete);
    }
    let Some(task) = projection
        .tasks
        .iter()
        .find(|task| task.task_id == descriptor.task_id)
    else {
        return Err(LandRefusal::GenerationMissing);
    };
    let Some(proposal) = task
        .proposals
        .iter()
        .find(|proposal| proposal.intent_message_id == descriptor.intent_message_id)
    else {
        return Err(LandRefusal::GenerationMissing);
    };
    if proposal.agent != descriptor.agent || proposal.sequence != descriptor.sequence {
        return Err(LandRefusal::Superseded);
    }
    if proposal.state != WorkTaskState::Accepted {
        return Err(LandRefusal::Superseded);
    }
    let superseded = task.proposals.iter().any(|other| {
        other.agent == descriptor.agent
            && other.state == WorkTaskState::Accepted
            && (other.sequence > proposal.sequence
                || (other.sequence == proposal.sequence
                    && other.intent_message_id != proposal.intent_message_id
                    && other.intent_message_id < proposal.intent_message_id))
    });
    if superseded {
        return Err(LandRefusal::Superseded);
    }
    Ok(())
}

impl ControllerCore {
    /// Resolves the accepted-work guard for the next automatic land.
    ///
    /// Runner-owned controllers guard with the accepted-work descriptor
    /// bound to the active request; the pinned generation is re-resolved
    /// against a fresh work projection before every land (see
    /// [`Self::revalidate_runner_land`]) so a superseded or terminal
    /// generation, an incomplete projection, or an unknown send outcome
    /// refuses the land fail-closed. When the runner is enforced but has no
    /// active accepted work, automatic publication is skipped entirely.
    /// Interactive controllers guard with the latest accepted intent for the
    /// agent; without one, legacy all-path behavior is preserved.
    async fn land_guard(&mut self) -> Option<LandGuard> {
        match &self.owner {
            Owner::Runner(ownership) => {
                let enforced = match ownership.scope_mode(&self.base, &self.agent) {
                    Ok(mode) => mode.is_enforced(),
                    Err(error) => {
                        self.enter_attention(ContinuousAttention {
                            reason: "corrupt_state".to_string(),
                            detail: bounded_detail(&error),
                        });
                        return None;
                    }
                };
                if !enforced {
                    return Some(LandGuard::AllPath);
                }
                // Fail closed on a scope-change record that is not confirmed:
                // a previous process persisted the dedup record but the send
                // outcome is unknown. Mark a publish-pending record awaiting
                // confirmation and stop before any new request or land.
                match ownership.scope_change_request_key(&self.base, &self.agent) {
                    Ok(Some(key)) if key.publish_state != ScopeChangePublishState::Confirmed => {
                        if key.publish_state == ScopeChangePublishState::PublishPending {
                            let _ = ownership
                                .mark_scope_change_awaiting_confirmation(&self.base, &self.agent);
                        }
                        return Some(LandGuard::Refused {
                            reason: "scope_change_awaiting_confirmation".to_string(),
                            detail: "a previous scope-change request was persisted but never \
                                     confirmed; refusing automatic land until it is resolved"
                                .to_string(),
                        });
                    }
                    Ok(_) | Err(_) => {}
                }
                let descriptor = match ownership.active_accepted_work(&self.base, &self.agent) {
                    Ok(Some(descriptor)) => descriptor,
                    Ok(None) => return Some(LandGuard::Skip),
                    Err(error) => {
                        self.enter_attention(ContinuousAttention {
                            reason: "corrupt_state".to_string(),
                            detail: bounded_detail(&error),
                        });
                        return None;
                    }
                };
                match self.revalidate_runner_land(&descriptor).await {
                    Ok(()) => Some(LandGuard::Scoped {
                        scope: descriptor.scope,
                        task_id: descriptor.task_id,
                        intent_message_id: descriptor.intent_message_id,
                        coordinator: descriptor.coordinator,
                    }),
                    Err(refusal) => Some(LandGuard::Refused {
                        reason: refusal.reason().to_string(),
                        detail: refusal.detail(),
                    }),
                }
            }
            Owner::Interactive(_) => match self.latest_accepted_guard().await {
                Ok(guard) => Some(guard),
                Err(error) => {
                    self.classify_operation_error(&error);
                    None
                }
            },
        }
    }

    /// Re-opens the work projection before a runner-owned automatic land and
    /// re-resolves the pinned accepted-work descriptor: the pinned
    /// (task, agent, intent, sequence) generation must still be the current
    /// accepted generation. A newer accepted generation, a terminal or
    /// missing generation, or an incomplete projection refuses the land
    /// fail-closed (no land; conflict/worktree untouched).
    async fn revalidate_runner_land(
        &mut self,
        descriptor: &AcceptedWorkDescriptor,
    ) -> std::result::Result<(), LandRefusal> {
        let ctx = self.sync_ctx();
        let projection =
            match feanorfs_agent_core::work::work_status(&ctx, WorkStatusInput::default()).await {
                Ok(projection) => projection,
                Err(error) => {
                    return Err(LandRefusal::UnreadableProjection(bounded_detail(&error)));
                }
            };
        revalidate_pinned_generation(&projection, descriptor)
    }

    /// Latest accepted nonterminal intent for the agent, resolved through the
    /// reducer projection (interactive guard source).
    async fn latest_accepted_guard(&mut self) -> anyhow::Result<LandGuard> {
        let ctx = self.sync_ctx();
        let projection =
            feanorfs_agent_core::work::work_status(&ctx, WorkStatusInput::default()).await?;
        if projection.projection_incomplete {
            // Fail closed: acceptance cannot be proven, so nothing may land.
            return Ok(LandGuard::Refused {
                reason: "projection_incomplete".to_string(),
                detail: "the work projection is incomplete; the latest accepted intent cannot \
                         be proven, so automatic land is refused"
                    .to_string(),
            });
        }
        let mut best: Option<(String, &feanorfs_common::WorkProposalStatus)> = None;
        for task in &projection.tasks {
            for proposal in &task.proposals {
                if proposal.agent != self.agent || proposal.state != WorkTaskState::Accepted {
                    continue;
                }
                let is_better = best.as_ref().is_none_or(|(_, current)| {
                    proposal.sequence > current.sequence
                        || (proposal.sequence == current.sequence
                            && proposal.intent_message_id < current.intent_message_id)
                });
                if is_better {
                    best = Some((task.task_id.clone(), proposal));
                }
            }
        }
        let Some((task_id, proposal)) = best else {
            return Ok(LandGuard::AllPath);
        };
        Ok(LandGuard::Scoped {
            scope: proposal.accepted_scope.clone(),
            task_id,
            intent_message_id: proposal.intent_message_id.clone(),
            coordinator: proposal.coordinator.clone(),
        })
    }

    /// Publishes one deduplicated bounded scope-change request for the
    /// waiting task, then records the typed scope-amendment wait.
    ///
    /// The dedup record is durably persisted BEFORE the message is sent, so
    /// a crash/restart between persist and publish never republishes an
    /// already-persisted request.
    async fn handle_out_of_scope(
        &mut self,
        guard: &LandGuard,
        out_of_scope: &[String],
        out_of_scope_changes: &[feanorfs_common::FileState],
        out_of_scope_count: u32,
    ) {
        let LandGuard::Scoped {
            scope,
            task_id,
            intent_message_id,
            coordinator,
        } = guard
        else {
            return;
        };
        let mut paths = out_of_scope.to_vec();
        paths.truncate(WORK_MAX_PATHS);
        paths.sort();
        paths.dedup();
        // Canonical dedup fingerprint: sorted accepted-scope entries, sorted
        // concerns, and the sorted operation set (wire strings) of the
        // blocked diff. Task and intent are separate tuple components.
        let mut scope_entries = scope.paths.clone();
        scope_entries.sort();
        scope_entries.dedup();
        let mut concerns = scope.concerns.clone();
        concerns.sort();
        concerns.dedup();
        let operations = self
            .derive_scope_change_operations(out_of_scope, out_of_scope_changes)
            .await;
        let fingerprint = blake3::hash(
            format!(
                "{}\u{0}{}\u{0}{}",
                scope_entries.join("\u{0}"),
                concerns.join("\u{0}"),
                operations
                    .iter()
                    .map(|operation| operation.as_str())
                    .collect::<Vec<_>>()
                    .join("\u{0}")
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        // Canonical scope component (paths + concerns, no operations): the
        // admission gate releases the scope-amendment wait only when this
        // changes (an amendment), never for operation-set differences.
        let scope_fingerprint = blake3::hash(
            format!(
                "{}\u{0}{}",
                scope_entries.join("\u{0}"),
                concerns.join("\u{0}")
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        // The pre-publish gate compares the full tuple (task, intent,
        // fingerprint) — never the fingerprint alone.
        let already_published = match &self.owner {
            Owner::Runner(ownership) => ownership
                .scope_change_request_key(&self.base, &self.agent)
                .ok()
                .flatten()
                .is_some_and(|key| {
                    key.task_id == *task_id
                        && key.intent_message_id == *intent_message_id
                        && key.paths_fingerprint == fingerprint
                }),
            Owner::Interactive(_) => self.published_scope_change.as_ref().is_some_and(|record| {
                record.task_id == *task_id
                    && record.intent_message_id == *intent_message_id
                    && record.fingerprint == fingerprint
            }),
        };
        if already_published {
            return;
        }
        let profile = ScopeChangeRequestProfile {
            task_id: task_id.clone(),
            intent_message_id: intent_message_id.clone(),
            operations,
            paths,
            concerns: scope.concerns.clone(),
            reason: format!("{out_of_scope_count} path(s) outside the accepted scope"),
        };
        let body = match encode_scope_change_request(&profile) {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(?error, "encode scope change request");
                return;
            }
        };
        // Persist-before-publish: land the dedup record durably first so a
        // restart between this point and the send never republishes.
        match &self.owner {
            Owner::Runner(ownership) => {
                if let Err(error) = ownership.begin_scope_change_request(
                    &self.base,
                    &self.agent,
                    task_id,
                    intent_message_id,
                    &fingerprint,
                    &scope_fingerprint,
                ) {
                    tracing::warn!(?error, "persist scope change request");
                    return;
                }
            }
            Owner::Interactive(_) => {
                let record = PublishedScopeChange {
                    task_id: task_id.clone(),
                    intent_message_id: intent_message_id.clone(),
                    fingerprint: fingerprint.clone(),
                    message_id: None,
                };
                if let Err(error) = self.persist_scope_change_record(&record).await {
                    tracing::warn!(?error, "persist scope change request");
                    return;
                }
                self.published_scope_change = Some(record);
            }
        }
        let ctx = self.sync_ctx();
        let sent = match feanorfs_agent_core::send_message(
            &ctx,
            feanorfs_common::AgentMessageInput {
                to: coordinator
                    .clone()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "human".to_string()),
                kind: feanorfs_common::AgentMessageKind::Request,
                body,
                about_snapshot: self.observed_head.clone(),
                reply_to: None,
                from: Some(self.agent.clone()),
            },
        )
        .await
        {
            Ok(sent) => sent,
            Err(error) => {
                tracing::warn!(?error, "publish scope change request");
                if let Owner::Runner(ownership) = &self.owner {
                    let _ = ownership.abandon_scope_change_request(&self.base, &self.agent);
                }
                return;
            }
        };
        // Fill the returned message id into the persisted record, then
        // record the typed scope-amendment wait. While a child is still
        // active the store refuses the wait (expected): the admission gate
        // blocks on the confirmed record itself, never on the wait.
        match &self.owner {
            Owner::Runner(ownership) => {
                if let Err(error) = ownership.complete_scope_change_request(
                    &self.base,
                    &self.agent,
                    &sent.message_id,
                ) {
                    tracing::warn!(?error, "complete scope change request");
                }
                let wait = RunnerWorkWait {
                    kind: RunnerWorkWaitKind::ScopeAmendmentRequested,
                    message_id: intent_message_id.clone(),
                    reason: None,
                    out_of_scope_count,
                    observed_at_ms: chrono::Utc::now().timestamp_millis(),
                };
                if let Err(error) = ownership.record_work_wait(&self.base, &self.agent, &wait) {
                    tracing::warn!(?error, "record scope amendment wait");
                }
            }
            Owner::Interactive(_) => {
                let mut record = PublishedScopeChange {
                    task_id: task_id.clone(),
                    intent_message_id: intent_message_id.clone(),
                    fingerprint: fingerprint.clone(),
                    message_id: None,
                };
                record.message_id = Some(sent.message_id.clone());
                if let Err(error) = self.persist_scope_change_record(&record).await {
                    tracing::warn!(?error, "complete scope change request");
                }
                self.published_scope_change = Some(record);
            }
        }
    }

    /// Derives the bounded operation set (Add/Modify/Delete/ModeChange) of
    /// the out-of-scope blocked diff from the agent base snapshot and the
    /// live worktree scan. Deterministic and sorted; any read error degrades
    /// to an empty set so the request profile still encodes and deduplicates
    /// (the operations only widen the dedup key).
    async fn derive_scope_change_operations(
        &self,
        out_of_scope: &[String],
        out_of_scope_changes: &[feanorfs_common::FileState],
    ) -> Vec<ScopeChangeOperation> {
        // Derive from the same authoritative diff that produced the
        // partition. A worktree re-scan would race the runner's refresh
        // (which can resurrect deleted base paths), so it is never used.
        let base_files = match self.load_base_files().await {
            Some(files) => files,
            None => return Vec::new(),
        };
        let changes = out_of_scope_changes
            .iter()
            .filter(|change| out_of_scope.contains(&change.path))
            .collect::<Vec<_>>();
        let mut operations = std::collections::BTreeSet::new();
        for change in changes {
            let operation = match base_files.get(&change.path) {
                _ if change.deleted => ScopeChangeOperation::Delete,
                None => ScopeChangeOperation::Add,
                Some(base) if base.hash != change.hash => ScopeChangeOperation::Modify,
                Some(base) if base.mode != change.mode => ScopeChangeOperation::ModeChange,
                // Unchanged against the base (e.g. a resurrection race): the
                // diff entry contributes no operation.
                Some(_) => continue,
            };
            operations.insert(operation);
        }
        let operations = operations.into_iter().collect::<Vec<_>>();
        debug_assert!(operations.len() <= WORK_MAX_SCOPE_CHANGE_OPERATIONS);
        operations
    }

    /// Loads the agent base snapshot file set once (bounded, best-effort).
    async fn load_base_files(
        &self,
    ) -> Option<std::collections::HashMap<String, feanorfs_common::FileState>> {
        let base_id = self.probe_agent_base.clone()?;
        let ctx = self.sync_ctx();
        let snapshots = feanorfs_agent_core::SnapshotEngine::new(&ctx);
        match snapshots.load_files(&base_id).await {
            Ok(files) => Some(files),
            Err(error) => {
                tracing::warn!(?error, "derive scope-change operations: read agent base");
                None
            }
        }
    }

    fn current_status(&self) -> ContinuousAgentStatus {
        build_status(
            &self.agent,
            self.active,
            self.phase,
            self.observed_head.clone(),
            self.observed_tree.clone(),
            self.settled_snapshot.clone(),
            self.pending_local,
            self.deferred_count,
            self.attention.clone(),
            Some(self.owner_pid),
            self.owner_start_id.clone(),
        )
    }

    fn publish_status(&self) {
        let status = self.current_status();
        if let Err(error) = write_continuous_status(&self.base, &self.agent, &status) {
            tracing::warn!(
                ?error,
                agent = self.agent,
                "publish continuous status failed"
            );
        }
    }

    fn set_phase(&mut self, phase: ContinuousPhase) {
        self.phase = phase;
        self.publish_status();
    }

    fn has_pending_work(&self) -> bool {
        self.pending_local || self.pending_remote
    }

    fn is_settled(&self) -> bool {
        self.attention.is_none() && !self.has_pending_work() && self.settled_snapshot.is_some()
    }

    fn update_settlement(&mut self) {
        if self.attention.is_some() || self.has_pending_work() {
            self.settled_snapshot = None;
        } else if self.settled_snapshot.is_none() {
            // Keep an existing settled snapshot stable across signal-only
            // heads. A result publication itself advances the opaque head;
            // replacing this ID with that message-only snapshot would make a
            // correctly correlated result appear stale after it was sent.
            self.settled_snapshot = self.observed_head.clone();
        }
    }

    fn pending_phase(&self) -> ContinuousPhase {
        if self.attention.is_some() {
            ContinuousPhase::NeedsAttention
        } else if self.retry_at.is_some() && self.has_pending_work() {
            ContinuousPhase::Offline
        } else if self.pending_local {
            ContinuousPhase::LocalDirty
        } else if self.pending_remote {
            ContinuousPhase::RefreshingRemote
        } else {
            ContinuousPhase::Idle
        }
    }

    fn mark_local_dirty(&mut self) {
        self.pending_local = true;
        self.settled_snapshot = None;
        if self.attention.is_none() {
            self.set_phase(self.pending_phase());
        }
    }

    fn apply_probe(&mut self, probe: feanorfs_agent_core::ContinuousProbe) {
        self.offline_failures = 0;
        self.retry_at = None;
        if self.observed_tree != probe.head_tree {
            self.settled_snapshot = None;
        }
        self.observed_head = probe.current_head.clone();
        self.observed_tree = probe.head_tree;
        self.probe_agent_base = probe.agent_base;
        self.observer.acknowledge(probe.current_head);
        self.pending_local = probe.local_changes > 0 || probe.conflicts > 0;
        self.pending_remote = !probe.base_is_current;
        self.update_settlement();
    }

    /// Durable interactive scope-change dedup record file (one per agent)
    /// under the agent's continuous state directory. Persisted with the
    /// work-store atomic-durable write policy (crash-durable replacement),
    /// so a restart never republishes an already-persisted request.
    fn scope_change_record_rel(&self) -> String {
        format!("{}/state/scope-change-request.json", self.agent)
    }

    fn scope_change_record_path(&self) -> Result<PathBuf> {
        Ok(feanorfs_agent_core::agents_dir(&self.base)?.join(self.scope_change_record_rel()))
    }

    /// Loads the durable interactive dedup record, when one exists and is
    /// well-formed (bounded, validated).
    fn load_persisted_scope_change_record(&self) -> Option<PublishedScopeChange> {
        let path = self.scope_change_record_path().ok()?;
        let bytes = std::fs::read(&path).ok()?;
        if bytes.len() > 16 * 1024 {
            return None;
        }
        let record: PublishedScopeChange = serde_json::from_slice(&bytes).ok()?;
        if record.task_id.is_empty()
            || record.intent_message_id.len() != 64
            || record.fingerprint.is_empty()
            || record.fingerprint.len() > 128
            || record
                .message_id
                .as_deref()
                .is_some_and(|message_id| message_id.len() != 64)
        {
            return None;
        }
        Some(record)
    }

    /// Persists the durable interactive dedup record with the work-store
    /// atomic-durable write policy (crash-durable replacement) plus a private
    /// 0o600 mode on Unix, matching the runner-state policy.
    async fn persist_scope_change_record(&self, record: &PublishedScopeChange) -> Result<()> {
        let bytes = serde_json::to_vec(record).context("serialize scope change record")?;
        ensure!(
            bytes.len() <= 16 * 1024,
            "scope change record exceeds its size bound"
        );
        let path = self.scope_change_record_path()?;
        let base = feanorfs_agent_core::agents_dir(&self.base)?;
        feanorfs_agent_core::fs_util::atomic_write_durable(
            &base,
            &self.scope_change_record_rel(),
            &bytes,
        )
        .await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Ok(metadata) = std::fs::metadata(&path) {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o600);
                let _ = std::fs::set_permissions(&path, permissions);
            }
        }
        Ok(())
    }

    async fn probe_authoritative_state(&mut self) -> Result<feanorfs_agent_core::ContinuousProbe> {
        probe_agent_state(&self.base, &self.db, &self.api, &self.agent).await
    }
    fn sync_ctx(&self) -> feanorfs_agent_core::SyncCtx<'_> {
        feanorfs_agent_core::SyncCtx::with_format_version(
            &self.api,
            &self.db,
            &self.base,
            &self.workspace_id,
            self.password.as_deref(),
            feanorfs_common::LegacyPolicy::Reject,
            self.format_version,
        )
    }

    /// Outbound: publish one quiet local generation through the existing land
    /// engine with `clean=false` and `propose=false`.
    async fn do_land(&mut self) {
        if self.attention.is_some() {
            return;
        }
        if let Err(error) = verify_agent_worktree(&self.base, &self.agent) {
            self.enter_attention(ContinuousAttention {
                reason: "unsafe_path".to_string(),
                detail: bounded_detail(&error),
            });
            return;
        }
        // Filesystem notifications are only hints. Refresh writes and editor
        // temp files can produce events without changing the agent tree, so
        // establish an authoritative diff before publishing a snapshot.
        let probe = match self.probe_authoritative_state().await {
            Ok(probe) => probe,
            Err(error) => {
                self.settled_snapshot = None;
                self.classify_operation_error(&error);
                return;
            }
        };
        self.apply_probe(probe);
        if !self.pending_local {
            self.set_phase(self.pending_phase());
            return;
        }

        // Accepted-work guard: enforced scopes partition the outbound diff;
        // out-of-scope changes stay local and unlanded.
        let Some(guard) = self.land_guard().await else {
            return;
        };
        match guard {
            LandGuard::AllPath => {
                self.land_all_path().await;
            }
            LandGuard::Skip => {
                // Enforced runner with no active accepted work: nothing may
                // publish. Keep the local generation queued; a later admitted
                // request binds a scope that guards it.
                self.set_phase(self.pending_phase());
            }
            LandGuard::Refused { reason, detail } => {
                // Fail-closed: no land; the conflict/worktree stays
                // untouched and automatic mutation pauses until explicit
                // action.
                self.enter_attention(ContinuousAttention { reason, detail });
            }
            LandGuard::Scoped {
                scope,
                task_id,
                intent_message_id,
                coordinator,
            } => {
                let ctx = self.sync_ctx();
                let partition = match partition_agent_scope(&ctx, &self.agent, &scope).await {
                    Ok(partition) => partition,
                    Err(error) => {
                        self.classify_operation_error(&error);
                        return;
                    }
                };
                if !partition.out_of_scope.is_empty() {
                    let count = partition.out_of_scope.len().min(WORK_MAX_PATHS) as u32;
                    self.handle_out_of_scope(
                        &LandGuard::Scoped {
                            scope: scope.clone(),
                            task_id,
                            intent_message_id,
                            coordinator,
                        },
                        &partition.out_of_scope,
                        &partition.out_of_scope_changes,
                        count,
                    )
                    .await;
                    // Keep the local generation queued and stay visible until
                    // an accepted amendment brings the paths in scope.
                    self.set_phase(self.pending_phase());
                    return;
                }
                if partition.in_scope_changes == 0 {
                    self.set_phase(self.pending_phase());
                    return;
                }
                self.land_scoped(&scope).await;
            }
        }
    }

    /// Outbound land without a scope guard (legacy/advisory behavior).
    async fn land_all_path(&mut self) {
        self.set_phase(ContinuousPhase::ReconcilingLocal);
        let result = match &self.owner {
            Owner::Interactive(lock) => {
                land_agent_continuous(
                    &self.base,
                    &self.db,
                    &self.api,
                    &self.workspace_id,
                    &self.agent,
                    self.password.as_deref(),
                    lock,
                )
                .await
            }
            Owner::Runner(ownership) => {
                land_agent_runner_owned(
                    &self.base,
                    &self.db,
                    &self.api,
                    &self.workspace_id,
                    &self.agent,
                    self.password.as_deref(),
                    ownership,
                )
                .await
            }
        };
        self.settle_land_result(result).await;
    }

    /// Outbound land guarded by one accepted scope.
    async fn land_scoped(&mut self, scope: &feanorfs_common::WorkScope) {
        self.set_phase(ContinuousPhase::ReconcilingLocal);
        let result = match &self.owner {
            Owner::Interactive(lock) => {
                land_agent_continuous_scoped(
                    &self.base,
                    &self.db,
                    &self.api,
                    &self.workspace_id,
                    &self.agent,
                    self.password.as_deref(),
                    lock,
                    scope,
                )
                .await
            }
            Owner::Runner(ownership) => {
                land_agent_runner_owned_scoped(
                    &self.base,
                    &self.db,
                    &self.api,
                    &self.workspace_id,
                    &self.agent,
                    self.password.as_deref(),
                    ownership,
                    Some(scope),
                )
                .await
            }
        };
        self.settle_land_result(result).await;
    }

    async fn settle_land_result(
        &mut self,
        result: anyhow::Result<feanorfs_common::AgentLandResult>,
    ) {
        match result {
            Ok(landed) => {
                self.offline_failures = 0;
                self.retry_at = None;
                let conflicts = landed.conflicts.len();
                if conflicts > 0 {
                    // The land engine already registered the encrypted
                    // conflicts and materialized the legs; pause automatic
                    // mutation until explicit resolution.
                    self.enter_attention(ContinuousAttention {
                        reason: "pending_conflicts".to_string(),
                        detail: format!("{conflicts} path(s) overlap and need explicit resolution"),
                    });
                    return;
                }
                self.pending_local = false;
                self.deferred_count = 0;
                if let Some(id) = landed.snapshot_id.clone() {
                    // Advance the observed head/tree to our own publication;
                    // a later observation of the same head is a no-op.
                    self.observed_head = Some(id.clone());
                    let ctx = self.sync_ctx();
                    let engine = feanorfs_agent_core::SnapshotEngine::new(&ctx);
                    match engine.load_snapshot(&id).await {
                        Ok(snapshot) => self.observed_tree = Some(snapshot.root),
                        Err(error) => tracing::warn!(?error, "read landed snapshot tree"),
                    }
                    self.observer.acknowledge(self.observed_head.clone());
                }
                // Even a successful land may have incorporated untouched
                // remote paths into the shared head. Do not call that head
                // settled until an inbound refresh and authoritative probe
                // prove the agent base carries the same tree.
                self.pending_remote = true;
                self.settled_snapshot = None;
                // Land publishes into the shared folder and advances the agent
                // base, but never writes remote paths into the agent worktree.
                // Follow with an inbound pass so untouched remote paths reach
                // the agent; it is a no-op when nothing is pending.
                self.do_refresh().await;
            }
            Err(error) => self.classify_operation_error(&error),
        }
    }

    /// Inbound: bring the shared worktree and the agent worktree current.
    async fn do_refresh(&mut self) {
        if self.attention.is_some() {
            return;
        }
        if let Err(error) = verify_agent_worktree(&self.base, &self.agent) {
            self.enter_attention(ContinuousAttention {
                reason: "unsafe_path".to_string(),
                detail: bounded_detail(&error),
            });
            return;
        }
        // This flag is deliberately cleared only by the verified post-refresh
        // probe. A retryable sync/refresh error must leave inbound work queued.
        self.pending_remote = true;
        self.settled_snapshot = None;
        self.set_phase(ContinuousPhase::RefreshingRemote);
        // 1. Shared main worktree current through the existing sync engine.
        if let Err(error) = feanorfs_client::commands::do_sync(
            &self.api,
            &self.db,
            &self.base,
            &self.workspace_id,
            self.password.as_deref(),
            false,
        )
        .await
        {
            self.classify_operation_error(&error);
            return;
        }
        // 2. Agent worktree refresh with safe defaults; never --replace.
        let result = match &self.owner {
            Owner::Interactive(lock) => {
                refresh_agent_continuous(
                    &self.base,
                    &self.db,
                    &self.api,
                    &self.workspace_id,
                    &self.agent,
                    self.password.as_deref(),
                    lock,
                )
                .await
            }
            Owner::Runner(ownership) => {
                refresh_agent_runner_owned(
                    &self.base,
                    &self.db,
                    &self.api,
                    &self.workspace_id,
                    &self.agent,
                    self.password.as_deref(),
                    ownership,
                )
                .await
            }
        };
        match result {
            Ok(refreshed) => {
                self.offline_failures = 0;
                self.retry_at = None;
                self.deferred_count = refreshed.deferred.len() as u32;
                match self.probe_authoritative_state().await {
                    Ok(probe) => self.apply_probe(probe),
                    Err(error) => {
                        self.classify_operation_error(&error);
                        return;
                    }
                }
                // Deferred paths are agent-local overlaps. The authoritative
                // probe normally sees them as local/remote work too, but keep
                // the explicit result as a conservative lower bound.
                self.pending_local |= !refreshed.deferred.is_empty();
                self.update_settlement();
                self.set_phase(self.pending_phase());
            }
            Err(error) => self.classify_operation_error(&error),
        }
    }

    fn classify_operation_error(&mut self, error: &anyhow::Error) {
        match classify_continuous_error(error) {
            ContinuousErrorClass::Retryable => {
                self.offline_failures = self.offline_failures.saturating_add(1);
                let backoff = LIVE_BACKOFF.delay(self.offline_failures);
                self.retry_at = Some(tokio::time::Instant::now() + backoff);
                self.set_phase(ContinuousPhase::Offline);
                tracing::warn!(
                    ?error,
                    agent = self.agent,
                    retry_after_ms = backoff.as_millis(),
                    "continuous reconciliation offline; will retry"
                );
            }
            ContinuousErrorClass::Attention(attention) => {
                self.enter_attention(attention);
                tracing::error!(
                    ?error,
                    agent = self.agent,
                    "continuous reconciliation paused"
                );
            }
        }
    }

    fn enter_attention(&mut self, attention: ContinuousAttention) {
        self.attention = Some(attention);
        self.settled_snapshot = None;
        self.phase = ContinuousPhase::NeedsAttention;
        self.publish_status();
    }

    /// One bounded attempt to reconcile remaining local work.
    ///
    /// `deactivate` marks the controller stopped (final outcome); runner
    /// flushes keep the controller live for the worker lifetime.
    async fn flush_local(&mut self, budget: Duration, deactivate: bool) -> LiveFinalOutcome {
        if deactivate {
            self.set_phase(ContinuousPhase::Stopping);
        }

        let deadline = tokio::time::Instant::now() + budget;
        if self.attention.is_none() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, self.probe_authoritative_state()).await {
                Ok(Ok(probe)) => self.apply_probe(probe),
                Ok(Err(error)) => {
                    // A failed final probe means the current remote state is
                    // unknown. Never report the previously observed snapshot
                    // as settled merely because no local event was queued.
                    self.pending_remote = true;
                    self.settled_snapshot = None;
                    self.classify_operation_error(&error);
                }
                Err(error) => {
                    tracing::warn!(?error, "final reconciliation probe exceeded its budget");
                    self.pending_remote = true;
                    self.settled_snapshot = None;
                    self.offline_failures = self.offline_failures.saturating_add(1);
                    self.retry_at = Some(tokio::time::Instant::now());
                    self.set_phase(ContinuousPhase::Offline);
                }
            }
        }

        // Bound retries even if a concurrently advancing head keeps producing
        // new work. A successful land already performs its own inbound pass.
        for _ in 0..4 {
            if self.attention.is_some() || !self.has_pending_work() || self.retry_at.is_some() {
                break;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let operation = async {
                if self.pending_local {
                    self.do_land().await;
                } else {
                    self.do_refresh().await;
                }
            };
            if let Err(error) = tokio::time::timeout(remaining, operation).await {
                tracing::warn!(?error, "reconciliation exceeded its budget");
                self.pending_remote = true;
                self.settled_snapshot = None;
                self.offline_failures = self.offline_failures.saturating_add(1);
                self.retry_at = Some(tokio::time::Instant::now());
                self.set_phase(ContinuousPhase::Offline);
                break;
            }
        }

        self.update_settlement();
        let outcome = LiveFinalOutcome {
            settled: self.is_settled(),
            offline: self.attention.is_none() && !self.is_settled(),
            attention: self.attention.clone(),
            settled_snapshot: self.settled_snapshot.clone(),
            deferred_count: self.deferred_count,
            child_exit: None,
        };
        if deactivate {
            self.active = false;
            self.phase = ContinuousPhase::Stopping;
            self.publish_status();
        } else {
            self.set_phase(self.pending_phase());
        }
        outcome
    }
}

fn bounded_detail(error: &anyhow::Error) -> String {
    let mut out = String::new();
    for character in format!("{error:#}").chars().take(1024) {
        if character.is_control() {
            out.extend(character.escape_default());
        } else {
            out.push(character);
        }
    }
    out
}

/// Control messages sent to a spawned controller task.
enum Control {
    /// Drain the current burst and attempt one bounded reconciliation.
    FlushFinal(oneshot::Sender<LiveFinalOutcome>),
}

/// Handle to a spawned runner-owned controller task.
pub struct RunnerControllerHandle {
    /// Bumped on every observed opaque head change (files or signals).
    pub head_generation: watch::Receiver<u64>,
    control: mpsc::Sender<Control>,
    terminal: watch::Receiver<ControllerTaskState>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Default)]
struct ControllerTaskState {
    finished: bool,
    error: Option<String>,
}

impl RunnerControllerHandle {
    /// Blocks until the controller has attempted to reconcile remaining local
    /// work and reports the honest outcome. The controller keeps running for
    /// the worker lifetime afterwards.
    pub async fn flush_final(&self) -> Result<LiveFinalOutcome> {
        let (tx, rx) = oneshot::channel();
        if self.control.send(Control::FlushFinal(tx)).await.is_err() {
            return Err(self
                .wait_for_stopped_error("continuous controller is not running")
                .await);
        }
        match rx.await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(self
                .wait_for_stopped_error("continuous controller stopped before final reconciliation")
                .await),
        }
    }

    pub(super) fn stopped_error(&self, fallback: &str) -> anyhow::Error {
        let state = self.terminal.borrow().clone();
        anyhow::anyhow!(state.error.unwrap_or_else(|| {
            if state.finished {
                "continuous controller stopped".to_string()
            } else {
                fallback.to_string()
            }
        }))
    }

    async fn wait_for_stopped_error(&self, fallback: &str) -> anyhow::Error {
        let mut terminal = self.terminal.clone();
        if !terminal.borrow().finished {
            let _ = tokio::time::timeout(Duration::from_secs(1), terminal.changed()).await;
        }
        self.stopped_error(fallback)
    }

    #[cfg(test)]
    pub(super) fn stopped_for_test(error: &str) -> Self {
        let (generation_tx, generation_rx) = watch::channel(0u64);
        drop(generation_tx);
        let (control, control_rx) = mpsc::channel(1);
        drop(control_rx);
        let (_terminal_tx, terminal) = watch::channel(ControllerTaskState {
            finished: true,
            error: Some(error.to_string()),
        });
        Self {
            head_generation: generation_rx,
            control,
            terminal,
            task: tokio::spawn(async {}),
        }
    }
}

impl Drop for RunnerControllerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn spawn_runner_controller(
    workspace_root: &Path,
    agent: &str,
    ownership: RunnerOwnership,
    shutdown: watch::Receiver<bool>,
) -> Result<RunnerControllerHandle> {
    // Preparation is the readiness handshake: config, API, authoritative
    // probe, and watcher installation all complete before the runner can
    // launch a configured child.
    let runtime = prepare_controller(
        workspace_root.to_path_buf(),
        agent.to_string(),
        Owner::Runner(ownership),
    )
    .await?;
    let (control_tx, control_rx) = mpsc::channel::<Control>(4);
    let (generation_tx, generation_rx) = watch::channel(1u64);
    let (terminal_tx, terminal_rx) = watch::channel(ControllerTaskState::default());
    let task = tokio::spawn(async move {
        // Keep the generation channel live until terminal task state has been
        // published, so a runner waking on channel closure can report the
        // actual controller error deterministically.
        let generation_liveness = generation_tx.clone();
        let result = run_controller_loop(runtime, shutdown, control_rx, generation_tx, None).await;
        let state = ControllerTaskState {
            finished: true,
            error: result.err().map(|error| bounded_detail(&error)),
        };
        let _ = terminal_tx.send(state);
        drop(generation_liveness);
    });
    Ok(RunnerControllerHandle {
        head_generation: generation_rx,
        control: control_tx,
        terminal: terminal_rx,
        task,
    })
}

/// Runs the full interactive lifecycle for `agent run`: acquire the lease,
/// launch the child, reconcile continuously, flush on exit, and report.
pub async fn run_agent_interactive(
    workspace_root: &Path,
    name: &str,
    command: &[String],
) -> Result<LiveFinalOutcome> {
    let config = feanorfs_client::load_config(workspace_root)?;
    ensure!(
        config.format_version >= 3,
        "continuous agent reconciliation requires a format-v3 workspace; run `feanorfs migrate` first"
    );
    ensure!(!command.is_empty(), "agent run requires a command");
    let owner = Owner::Interactive(ContinuousOwnerLock::acquire_interactive(
        workspace_root,
        name,
    )?);
    // Complete controller setup before child code can execute. This closes
    // the startup window where an immediate edit-and-exit could be missed and
    // ensures setup failures never leave an unowned child behind.
    let mut runtime =
        prepare_controller(workspace_root.to_path_buf(), name.to_string(), owner).await?;
    let agent_path = feanorfs_agent_core::agent_dir(workspace_root, name)?;
    let agent_dir_abs = agent_path
        .canonicalize()
        .unwrap_or_else(|_| agent_path.clone());
    let workspace_abs = workspace_root
        .canonicalize()
        .context("resolve shared workspace root")?;
    let mut command_line = tokio::process::Command::new(&command[0]);
    command_line
        .args(&command[1..])
        .current_dir(&agent_path)
        .env("FEANORFS_AGENT", name)
        .env("FEANORFS_AGENT_DIR", agent_dir_abs)
        .env("FEANORFS_WORKSPACE_ROOT", workspace_abs)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = match InteractiveChild::spawn(&mut command_line) {
        Ok(child) => child,
        Err(error) => {
            runtime.core.active = false;
            runtime.core.publish_status();
            return Err(error).with_context(|| format!("launch agent command '{}'", command[0]));
        }
    };

    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let (_control_tx, control_rx) = mpsc::channel::<Control>(4);
    let (generation_tx, _generation_rx) = watch::channel(1u64);
    match run_controller_loop(
        runtime,
        shutdown_rx,
        control_rx,
        generation_tx,
        Some(&mut child),
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            child.terminate().await;
            Err(error)
        }
    }
}

/// An interactive child plus an OS-owned process tree. Dropping this value is
/// fail-closed: the direct child and every admitted descendant are signalled.
struct InteractiveChild {
    child: Option<tokio::process::Child>,
    process_tree: Option<process_tree::ProcessTree>,
}

impl InteractiveChild {
    fn spawn(command: &mut tokio::process::Command) -> std::io::Result<Self> {
        process_tree::configure_process_group(command)?;
        let mut child = command.spawn()?;
        let tree = match process_tree::ProcessTree::adopt_child(&child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        #[cfg(windows)]
        if let Err(error) = tree.release_child(&child) {
            let _ = tree.force_termination();
            let _ = child.start_kill();
            return Err(error);
        }
        Ok(Self {
            child: Some(child),
            process_tree: Some(tree),
        })
    }

    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let result = self
            .child
            .as_mut()
            .expect("interactive child is present")
            .wait()
            .await;
        self.child.take();
        self.terminate_descendants();
        result
    }

    async fn terminate(&mut self) {
        if let Some(tree) = self.process_tree.as_ref() {
            let _ = tree.request_termination();
        }
        if let Some(child) = self.child.as_mut() {
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_err()
            {
                if let Some(tree) = self.process_tree.as_ref() {
                    let _ = tree.force_termination();
                }
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            }
        }
        self.child.take();
        self.terminate_descendants();
    }

    fn terminate_descendants(&mut self) {
        if let Some(tree) = self.process_tree.take() {
            let _ = tree.force_termination();
        }
    }
}

impl Drop for InteractiveChild {
    fn drop(&mut self) {
        self.terminate_descendants();
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// Waits for the interactive child; returns `None` when no child is owned.
async fn await_child(
    child: &mut Option<&mut InteractiveChild>,
) -> Option<std::io::Result<ExitStatus>> {
    match child.as_deref_mut() {
        Some(child) => Some(child.wait().await),
        None => None,
    }
}

fn exit_status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().map_or(1, |signal| 128 + signal)
    }
    #[cfg(not(unix))]
    1
}

struct ControllerRuntime {
    core: ControllerCore,
    fs_rx: mpsc::Receiver<()>,
    burst_dirty: Arc<AtomicBool>,
    _watcher: notify::RecommendedWatcher,
}

async fn prepare_controller(
    base: PathBuf,
    agent: String,
    owner: Owner,
) -> Result<ControllerRuntime> {
    let config = feanorfs_client::load_config(&base)?;
    ensure!(
        config.format_version >= 3,
        "continuous agent reconciliation requires a format-v3 workspace"
    );
    let api = Arc::new(feanorfs_client::open_api_client(&base, &config).await?);
    let db = feanorfs_client::open_client_db(&base).await?;
    let workspace_id = config.workspace_id.clone();
    let password = config.encryption_password.clone();
    let owner_pid = std::process::id();
    let owner_start_id = process_tree::process_start_identifier(owner_pid, "continuous-controller");

    let mut core = ControllerCore {
        base: base.clone(),
        agent: agent.clone(),
        owner,
        api: Arc::clone(&api),
        db,
        workspace_id: workspace_id.clone(),
        format_version: config.format_version,
        password,
        observer: HeadObserver::new_owned(api, &workspace_id),
        phase: ContinuousPhase::Starting,
        observed_head: None,
        observed_tree: None,
        settled_snapshot: None,
        pending_local: false,
        pending_remote: true,
        deferred_count: 0,
        attention: None,
        offline_failures: 0,
        retry_at: None,
        owner_pid,
        owner_start_id: Some(owner_start_id),
        active: true,
        probe_agent_base: None,
        published_scope_change: None,
    };
    // Interactive dedup survives `agent run` restarts through the durable
    // record: an already-persisted request is never republished.
    if matches!(core.owner, Owner::Interactive(_)) {
        core.published_scope_change = core.load_persisted_scope_change_record();
    }
    core.publish_status();
    // Fail-closed restart recovery: a publish-pending scope-change record
    // means a previous process persisted the dedup record but the send
    // outcome is unknown. Never republish: mark it awaiting confirmation and
    // stop automatic mutation until explicit action.
    match &core.owner {
        Owner::Runner(ownership) => match ownership.scope_change_request_key(&base, &agent) {
            Ok(Some(key)) if key.publish_state == ScopeChangePublishState::PublishPending => {
                let _ = ownership.mark_scope_change_awaiting_confirmation(&base, &agent);
                core.enter_attention(ContinuousAttention {
                    reason: "scope_change_awaiting_confirmation".to_string(),
                    detail: "a previous scope-change request was persisted but never \
                                 confirmed; refusing automatic land until it is resolved"
                        .to_string(),
                });
            }
            Ok(_) | Err(_) => {}
        },
        Owner::Interactive(_) => {
            if core
                .published_scope_change
                .as_ref()
                .is_some_and(|record| record.message_id.is_none())
            {
                core.enter_attention(ContinuousAttention {
                    reason: "scope_change_awaiting_confirmation".to_string(),
                    detail: "a previous scope-change request was persisted but never confirmed; \
                             refusing automatic land until it is resolved"
                        .to_string(),
                });
            }
        }
    }

    // Install the watcher before the initial probe. Existing runner-owned
    // worktrees can change during a slow probe, and those events must survive
    // into the first reconciliation generation.
    let (fs_tx, fs_rx) = mpsc::channel::<()>(EVENT_CHANNEL_BOUND);
    let burst_dirty = Arc::new(AtomicBool::new(false));
    let burst_dirty_watch = Arc::clone(&burst_dirty);
    let watched_dir = feanorfs_agent_core::agent_dir(&base, &agent)?;
    let event_root = watched_dir.clone();
    let mut watcher =
        notify::recommended_watcher(move |result: std::result::Result<notify::Event, _>| {
            if let Ok(event) = result {
                if event_warrants_sync_under(&event, &event_root) && fs_tx.try_send(()).is_err() {
                    burst_dirty_watch.store(true, Ordering::Release);
                }
            }
        })
        .context("start agent worktree watcher")?;
    watcher
        .watch(&watched_dir, notify::RecursiveMode::Recursive)
        .context("watch agent worktree")?;

    let startup = match core.probe_authoritative_state().await {
        Ok(probe) => probe,
        Err(error) => {
            core.classify_operation_error(&error);
            core.active = false;
            core.publish_status();
            return Err(error).context("probe continuous agent state before launch");
        }
    };
    let startup_conflicts = startup.conflicts;
    core.apply_probe(startup);
    if startup_conflicts > 0 {
        core.enter_attention(ContinuousAttention {
            reason: "pending_conflicts".to_string(),
            detail: format!("{startup_conflicts} pre-existing overlap(s) need explicit resolution"),
        });
    } else {
        core.set_phase(core.pending_phase());
    }

    Ok(ControllerRuntime {
        core,
        fs_rx,
        burst_dirty,
        _watcher: watcher,
    })
}

/// The single controller loop shared by interactive and runner owners.
async fn run_controller_loop(
    runtime: ControllerRuntime,
    mut shutdown: watch::Receiver<bool>,
    mut control_rx: mpsc::Receiver<Control>,
    generation_tx: watch::Sender<u64>,
    mut child: Option<&mut InteractiveChild>,
) -> Result<LiveFinalOutcome> {
    let ControllerRuntime {
        mut core,
        mut fs_rx,
        burst_dirty,
        _watcher,
    } = runtime;

    let mut pending_flush: Option<oneshot::Sender<LiveFinalOutcome>> = None;
    let mut generation: u64 = 1;
    // Register before entering the loop so a service-manager/user SIGTERM
    // cannot take the default process-exit path and orphan the already
    // admitted interactive process group.
    #[cfg(unix)]
    let mut terminate_signal: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        if child.is_some() {
            let mut signal =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .context("install interactive SIGTERM handler")?;
            Box::pin(async move {
                let _ = signal.recv().await;
            })
        } else {
            Box::pin(std::future::pending())
        };
    #[cfg(not(unix))]
    let mut terminate_signal: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(std::future::pending());

    loop {
        if let Some(tx) = pending_flush.take() {
            // Runner flush: include every queued filesystem generation before
            // the authoritative probe and bounded reconcile.
            capture_event_burst(&mut core, &mut fs_rx, &burst_dirty, false).await;
            let outcome = core.flush_local(FINAL_FLUSH_BUDGET, false).await;
            let _ = tx.send(outcome);
        }
        if burst_dirty.swap(false, Ordering::AcqRel) && core.attention.is_none() {
            core.mark_local_dirty();
        }
        if *shutdown.borrow() {
            break;
        }
        if core.has_pending_work() && core.attention.is_none() {
            if let Some(retry_at) = core.retry_at {
                if tokio::time::Instant::now() < retry_at {
                    // Offline: retain both local and inbound generations while
                    // waiting for the retry window, shutdown, new events, or
                    // an explicit final flush.
                    tokio::select! {
                        _ = tokio::time::sleep_until(retry_at) => {}
                        _ = shutdown.changed() => break,
                        Some(Control::FlushFinal(tx)) = control_rx.recv() => {
                            pending_flush = Some(tx);
                        }
                        Some(()) = fs_rx.recv() => {
                            capture_event_burst(
                                &mut core,
                                &mut fs_rx,
                                &burst_dirty,
                                true,
                            ).await;
                        }
                    }
                    continue;
                }
                core.retry_at = None;
            }
            if core.pending_local {
                core.do_land().await;
            } else {
                core.do_refresh().await;
            }
            continue;
        }

        tokio::select! {
            Some(()) = fs_rx.recv() => {
                capture_event_burst(&mut core, &mut fs_rx, &burst_dirty, true).await;
            }
            observation = core.observer.observe(HEAD_WAIT_WINDOW) => {
                match observation {
                    Ok(observed) => {
                        if observed.changed {
                            generation = generation.saturating_add(1);
                            let _ = generation_tx.send(generation);
                            let Some(head) = observed.head else {
                                core.observed_head = None;
                                core.observed_tree = None;
                                core.pending_remote = true;
                                core.settled_snapshot = None;
                                continue;
                            };
                            let previous_head = core.observed_head.clone();
                            match load_tree_root(&core, &head).await {
                                Ok(tree) => {
                                    let tree_changed = core.observed_tree.as_deref() != Some(&tree);
                                    core.observed_head = Some(head.clone());
                                    core.observed_tree = Some(tree);
                                    if tree_changed {
                                        core.pending_remote = true;
                                        core.settled_snapshot = None;
                                    } else {
                                        // Signal-only snapshot: no file work.
                                        // Preserve the prior settled snapshot
                                        // so a terminal signal does not make
                                        // its own `about_snapshot` stale.
                                        core.update_settlement();
                                        core.publish_status();
                                    }
                                }
                                Err(error) => {
                                    // HeadObserver records a change before the
                                    // encrypted snapshot is loaded. Restore its
                                    // acknowledgement so a retry can observe
                                    // the same head again.
                                    core.observer.acknowledge(previous_head);
                                    core.pending_remote = true;
                                    core.settled_snapshot = None;
                                    core.classify_operation_error(&error);
                                }
                            }
                        }
                    }
                    Err(error)
                        if feanorfs_agent_core::api::is_retryable_transport_error(&error) =>
                    {
                        core.pending_remote = true;
                        core.settled_snapshot = None;
                        core.offline_failures = core.offline_failures.saturating_add(1);
                        core.retry_at = Some(
                            tokio::time::Instant::now() + LIVE_BACKOFF.delay(core.offline_failures),
                        );
                        core.set_phase(ContinuousPhase::Offline);
                    }
                    Err(error) => core.classify_operation_error(&error),
                }
            }
            _ = shutdown.changed() => break,
            Some(Control::FlushFinal(tx)) = control_rx.recv() => {
                pending_flush = Some(tx);
            }
            exit = await_child(&mut child), if child.is_some() => {
                // Child exited: drain the final burst and make one bounded
                // final reconciliation attempt.
                capture_event_burst(&mut core, &mut fs_rx, &burst_dirty, false).await;
                let mut outcome = core.flush_local(FINAL_FLUSH_BUDGET, true).await;
                outcome.child_exit = Some(match exit {
                    Some(Ok(status)) => exit_status_code(status),
                    Some(Err(error)) => {
                        tracing::warn!(?error, "wait for interactive agent child failed");
                        1
                    }
                    None => 1,
                });
                return Ok(outcome);
            }
            _ = &mut terminate_signal, if child.is_some() => {
                // SIGTERM must follow the same bounded teardown as Ctrl+C,
                // including descendants and one final reconciliation pass.
                if let Some(child) = child.as_deref_mut() {
                    child.terminate().await;
                }
                capture_event_burst(&mut core, &mut fs_rx, &burst_dirty, false).await;
                let mut outcome = core.flush_local(FINAL_FLUSH_BUDGET, true).await;
                outcome.child_exit = Some(143);
                return Ok(outcome);
            }
            _ = tokio::signal::ctrl_c(), if child.is_some() => {
                // The interactive child owns a separate process group/job, so
                // explicitly terminate the whole tree before final probing.
                if let Some(child) = child.as_deref_mut() {
                    child.terminate().await;
                }
                capture_event_burst(&mut core, &mut fs_rx, &burst_dirty, false).await;
                let mut outcome = core.flush_local(FINAL_FLUSH_BUDGET, true).await;
                outcome.child_exit = Some(130);
                return Ok(outcome);
            }
        }
    }

    // Shutdown: drain and make one bounded final reconcile attempt.
    capture_event_burst(&mut core, &mut fs_rx, &burst_dirty, false).await;
    if let Some(tx) = pending_flush.take() {
        let outcome = core.flush_local(FINAL_FLUSH_BUDGET, false).await;
        let _ = tx.send(outcome.clone());
        core.active = false;
        core.publish_status();
        return Ok(outcome);
    }
    Ok(core.flush_local(FINAL_FLUSH_BUDGET, true).await)
}

async fn load_tree_root(core: &ControllerCore, head: &str) -> Result<String> {
    let ctx = core.sync_ctx();
    let engine = feanorfs_agent_core::SnapshotEngine::new(&ctx);
    Ok(engine.load_snapshot(head).await?.root)
}

async fn capture_event_burst(
    core: &mut ControllerCore,
    rx: &mut mpsc::Receiver<()>,
    overflow: &AtomicBool,
    first_consumed: bool,
) -> bool {
    let consumed = drain_event_burst(rx, DEBOUNCE_INTERVAL, first_consumed).await;
    let dirty = consumed || overflow.swap(false, Ordering::AcqRel);
    if dirty && core.attention.is_none() {
        core.mark_local_dirty();
    }
    dirty
}

/// Drains a burst only after a full quiet period since its final event.
/// Returns whether at least one event was consumed, including the event that
/// selected the caller's branch.
async fn drain_event_burst(
    rx: &mut mpsc::Receiver<()>,
    delay: Duration,
    mut consumed: bool,
) -> bool {
    if !consumed {
        match rx.try_recv() {
            Ok(()) => consumed = true,
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                return false;
            }
        }
    }
    loop {
        match tokio::time::timeout(delay, rx.recv()).await {
            Ok(Some(())) => consumed = true,
            Ok(None) | Err(_) => return consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn drain_reports_events_and_waits_for_the_final_quiet_period() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(()).await.unwrap();
        let sender = tokio::spawn(async move {
            // The second event lands inside the original 10 ms quiet window
            // and the third one after its UNRESTARTED deadline (t=12 > 10):
            // consuming it is only possible if the timer restarted at the
            // previous event. Virtual time makes this ordering exact.
            tokio::time::sleep(Duration::from_millis(5)).await;
            tx.send(()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(7)).await;
            tx.send(()).await.unwrap();
        });
        assert!(drain_event_burst(&mut rx, Duration::from_millis(10), false).await);
        sender.await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "quiet timer must restart after each burst event"
        );
    }

    #[tokio::test]
    async fn retryable_refresh_failure_retains_pending_remote_work() {
        let base = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable = listener.local_addr().unwrap();
        drop(listener);
        let config = feanorfs_client::Config {
            server_url: format!("http://{unavailable}"),
            workspace_id: "continuous-refresh-test".to_string(),
            encryption_password: Some("e".repeat(64)),
            server_password: None,
            tls_ca_pem: None,
            format_version: 3,
            hub_local: false,
            relay: None,
        };
        feanorfs_client::save_config(base.path(), &config).unwrap();
        let worktree = feanorfs_agent_core::agent_dir(base.path(), "worker").unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(worktree.parent().unwrap().join("state")).unwrap();
        let owner = ContinuousOwnerLock::try_acquire(base.path(), "worker")
            .unwrap()
            .expect("interactive owner");
        let db = feanorfs_client::open_client_db(base.path()).await.unwrap();
        let api = Arc::new(feanorfs_agent_core::ApiClient::new(
            &config.server_url,
            None,
        ));
        let mut core = ControllerCore {
            base: base.path().to_path_buf(),
            agent: "worker".to_string(),
            owner: Owner::Interactive(owner),
            api: Arc::clone(&api),
            db,
            workspace_id: config.workspace_id,
            format_version: config.format_version,
            password: config.encryption_password,
            observer: HeadObserver::new_owned(api, "continuous-refresh-test"),
            phase: ContinuousPhase::Idle,
            observed_head: Some("a".repeat(64)),
            observed_tree: Some("b".repeat(64)),
            settled_snapshot: Some("a".repeat(64)),
            pending_local: false,
            pending_remote: false,
            deferred_count: 0,
            attention: None,
            offline_failures: 0,
            retry_at: None,
            owner_pid: std::process::id(),
            owner_start_id: None,
            active: true,
            probe_agent_base: None,
            published_scope_change: None,
        };

        core.do_refresh().await;

        assert!(
            core.pending_remote,
            "failed inbound work must remain queued"
        );
        assert!(core.settled_snapshot.is_none());
        assert_eq!(core.phase, ContinuousPhase::Offline);
        assert!(core.retry_at.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interactive_child_exit_terminates_its_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("descendant.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30 & echo $! > \"$1\"")
            .arg("interactive-child-test")
            .arg(&pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = InteractiveChild::spawn(&mut command).unwrap();
        let descendant_pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(value) = tokio::fs::read_to_string(&pid_file).await {
                    if let Ok(pid) = value.trim().parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child published descendant pid");
        let identity = process_tree::ProcessIdentity::capture(descendant_pid)
            .expect("descendant is live before direct child cleanup");

        assert!(child.wait().await.unwrap().success());
        tokio::time::timeout(Duration::from_secs(2), async {
            while identity.is_current() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("direct child exit must terminate descendants");
    }
}

#[cfg(test)]
mod pinned_generation_tests {
    use super::revalidate_pinned_generation;
    use feanorfs_agent_core::AcceptedWorkDescriptor;
    use feanorfs_common::{
        WorkProposalStatus, WorkScope, WorkStatusResult, WorkTaskState, WorkTaskStatus,
    };

    fn hex_id(byte: u8) -> String {
        std::iter::repeat_n(byte as char, 64).collect()
    }

    fn proposal(state: WorkTaskState, sequence: u64, intent: u8) -> WorkProposalStatus {
        WorkProposalStatus {
            agent: "worker".to_string(),
            state,
            sequence,
            intent_message_id: hex_id(intent),
            coordinator: None,
            accepted_scope: WorkScope::default(),
            capabilities: vec![],
            decision: None,
            accepted_overlap: vec![],
            amendments: vec![],
            causal_refs: vec![],
            inspected_snapshot: None,
            verification: None,
            outcome: None,
            reason: None,
            source_message_id: hex_id(b's'),
            updated_at_ms: 0,
        }
    }

    fn projection(tasks: Vec<WorkTaskStatus>, incomplete: bool) -> WorkStatusResult {
        WorkStatusResult {
            cursor: hex_id(b'c'),
            cursor_reset: false,
            projection_incomplete: incomplete,
            messages_processed: 0,
            tasks,
            evidence_count: 0,
            dropped_count: 0,
            updated_at_ms: 0,
            applied_message_ids: vec![],
        }
    }

    fn descriptor(intent: u8, sequence: u64) -> AcceptedWorkDescriptor {
        AcceptedWorkDescriptor {
            schema_version: feanorfs_agent_core::ACCEPTED_WORK_SCHEMA_VERSION,
            task_id: "parser-impl".to_string(),
            agent: "worker".to_string(),
            intent_message_id: hex_id(intent),
            sequence,
            scope: WorkScope::default(),
            capabilities: vec![],
            coordinator: None,
            causal_base: None,
            base_snapshot: hex_id(b'b'),
            message_fingerprint: hex_id(b'f'),
            source_message_id: hex_id(b's'),
            updated_at_ms: 0,
        }
    }

    fn task(proposals: Vec<WorkProposalStatus>) -> WorkTaskStatus {
        WorkTaskStatus {
            task_id: "parser-impl".to_string(),
            state: WorkTaskState::Accepted,
            proposals,
        }
    }

    #[test]
    fn current_accepted_generation_passes() {
        let projection = projection(
            vec![task(vec![proposal(WorkTaskState::Accepted, 1, b'a')])],
            false,
        );
        assert!(revalidate_pinned_generation(&projection, &descriptor(b'a', 1)).is_ok());
    }

    #[test]
    fn incomplete_projection_refuses() {
        let projection = projection(
            vec![task(vec![proposal(WorkTaskState::Accepted, 1, b'a')])],
            true,
        );
        assert!(matches!(
            revalidate_pinned_generation(&projection, &descriptor(b'a', 1)),
            Err(super::LandRefusal::ProjectionIncomplete)
        ));
    }

    #[test]
    fn missing_task_or_proposal_refuses() {
        let empty = projection(vec![], false);
        assert!(matches!(
            revalidate_pinned_generation(&empty, &descriptor(b'a', 1)),
            Err(super::LandRefusal::GenerationMissing)
        ));
        let other_task = projection(
            vec![WorkTaskStatus {
                task_id: "other-task".to_string(),
                state: WorkTaskState::Accepted,
                proposals: vec![proposal(WorkTaskState::Accepted, 1, b'a')],
            }],
            false,
        );
        assert!(matches!(
            revalidate_pinned_generation(&other_task, &descriptor(b'a', 1)),
            Err(super::LandRefusal::GenerationMissing)
        ));
    }

    #[test]
    fn terminal_state_refuses() {
        let projection = projection(
            vec![task(vec![proposal(WorkTaskState::Completed, 1, b'a')])],
            false,
        );
        assert!(matches!(
            revalidate_pinned_generation(&projection, &descriptor(b'a', 1)),
            Err(super::LandRefusal::Superseded)
        ));
    }

    #[test]
    fn higher_sequence_supersedes() {
        let projection = projection(
            vec![task(vec![
                proposal(WorkTaskState::Accepted, 1, b'a'),
                proposal(WorkTaskState::Accepted, 2, b'b'),
            ])],
            false,
        );
        assert!(matches!(
            revalidate_pinned_generation(&projection, &descriptor(b'a', 1)),
            Err(super::LandRefusal::Superseded)
        ));
        // The newest generation itself still passes.
        assert!(revalidate_pinned_generation(&projection, &descriptor(b'b', 2)).is_ok());
    }

    #[test]
    fn equal_sequence_tie_breaks_toward_smallest_intent_id() {
        // hex_id(b'a') < hex_id(b'b'): the smaller id is the current
        // generation at equal sequence; the larger is superseded.
        let projection = projection(
            vec![task(vec![
                proposal(WorkTaskState::Accepted, 3, b'b'),
                proposal(WorkTaskState::Accepted, 3, b'a'),
            ])],
            false,
        );
        assert!(revalidate_pinned_generation(&projection, &descriptor(b'a', 3)).is_ok());
        assert!(matches!(
            revalidate_pinned_generation(&projection, &descriptor(b'b', 3)),
            Err(super::LandRefusal::Superseded)
        ));
    }
}
