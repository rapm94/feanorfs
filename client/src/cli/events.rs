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
            events.push(integrator_event(message).unwrap_or_else(|| agent_message_event(message)));
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
                if !update_snapshot_after_poll(
                    &mut current_snapshot,
                    api.get_head(&config.workspace_id).await,
                ) {
                    continue;
                }
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
        agent_message_cursor_reset_event, agent_message_event, integrator_event,
        remember_message_id, signal_events, update_snapshot_after_poll, FeanorEvent,
    };
    use feanorfs_common::{
        encode_integrator_profile, AgentInboxResult, AgentMessage, AgentMessageKind,
        IntegratorProfile,
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
}
