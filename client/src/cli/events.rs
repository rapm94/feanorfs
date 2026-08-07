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

#[derive(Serialize)]
struct FeanorEvent {
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mirror_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    about_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<u32>,
}

struct EventPayload {
    path: Option<String>,
    mirror_state: Option<MirrorState>,
    snapshot_id: Option<String>,
}

/// Bounded wakeup record for one new agent signal; deliberately omits the body.
fn agent_message_event(message: &feanorfs_common::AgentMessage) -> FeanorEvent {
    FeanorEvent {
        event: "agent_message",
        mirror_state: None,
        path: None,
        snapshot_id: None,
        message_id: Some(message.message_id.clone()),
        from: Some(message.from.clone()),
        to: Some(message.to.clone()),
        kind: Some(message.kind.as_str().to_string()),
        about_snapshot: Some(message.about_snapshot.clone()),
        assignment_id: None,
        attempt: None,
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
    Some(FeanorEvent {
        event,
        mirror_state: None,
        path: None,
        snapshot_id: None,
        message_id: Some(message.message_id.clone()),
        from: Some(message.from.clone()),
        to: Some(message.to.clone()),
        kind: Some(message.kind.as_str().to_string()),
        about_snapshot: Some(message.about_snapshot.clone()),
        assignment_id: Some(assignment_id),
        attempt: Some(attempt),
    })
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

    let mut poll = tokio::time::interval(Duration::from_secs(30));
    let mut last_emitted_conflicts: HashSet<String> = HashSet::new();
    let mut last_signal_cursor: Option<String> = current_snapshot.clone();
    let mut last_emitted_messages: HashSet<String> = HashSet::new();
    let mut emitted_message_order = VecDeque::new();
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
            _ = poll.tick() => {
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
                current_snapshot = api.get_head(&config.workspace_id).await?;
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
                    for message in &signals.messages {
                        if remember_message_id(
                            &mut last_emitted_messages,
                            &mut emitted_message_order,
                            &message.message_id,
                            MAX_EMITTED_MESSAGE_IDS,
                        ) {
                            if let Some(integrator) = integrator_event(message) {
                                emit_record(&integrator);
                            } else {
                                emit_record(&agent_message_event(message));
                            }
                        }
                    }
                    if signals.cursor_reset {
                        tracing::warn!(
                            "events poll: signal cursor reset or bounded result overflow; older wakeups may have been missed"
                        );
                    }
                    if !signals.cursor.is_empty() {
                        last_signal_cursor = Some(signals.cursor);
                    }
                }
            }
        }
    }
}

fn emit(event: &'static str, payload: EventPayload) {
    let mirror_str = payload.mirror_state.map(|s| {
        serde_json::to_value(s)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "idle".into())
    });
    let ev = FeanorEvent {
        event,
        mirror_state: mirror_str,
        path: payload.path,
        snapshot_id: payload.snapshot_id,
        message_id: None,
        from: None,
        to: None,
        kind: None,
        about_snapshot: None,
        assignment_id: None,
        attempt: None,
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
    use super::{agent_message_event, integrator_event, remember_message_id, FeanorEvent};
    use feanorfs_common::{
        encode_integrator_profile, AgentMessage, AgentMessageKind, IntegratorProfile,
    };
    use std::collections::{HashSet, VecDeque};

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
        let ev = FeanorEvent {
            event: "sync_state",
            mirror_state: Some("syncing".into()),
            path: None,
            snapshot_id: Some("abc".into()),
            message_id: None,
            from: None,
            to: None,
            kind: None,
            about_snapshot: None,
            assignment_id: None,
            attempt: None,
        };
        let value: serde_json::Value = serde_json::to_value(ev).unwrap();
        assert!(value.get("message_id").is_none());
        assert!(value.get("from").is_none());
        assert!(value.get("to").is_none());
        assert!(value.get("kind").is_none());
        assert!(value.get("about_snapshot").is_none());
        assert_eq!(value["snapshot_id"], "abc");
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
        assert_eq!(agent_message_event(&plain).event, "agent_message");
    }
}
