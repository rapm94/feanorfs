use feanorfs_client::conflicts::load_last_synced;
use feanorfs_client::lock::try_acquire_sync_lock;
use feanorfs_client::watch::event_paths_warrant_sync;
use feanorfs_client::{do_status, load_config, ApiClient, ClientDb};
use notify::Watcher;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

#[derive(Serialize)]
struct FeanorEvent {
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mirror_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

pub async fn run_events(current_dir: &Path) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
    let api = ApiClient::from_config(current_dir, &config).await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<std::path::PathBuf>>(100);
    let tx_clone = tx.clone();
    let watch_root = current_dir.to_path_buf();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if event_paths_warrant_sync(&event.paths) {
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
        emit("sync_state", None, Some(status.mirror_state.to_string()));
    }

    let mut poll = tokio::time::interval(Duration::from_secs(30));
    let mut last_emitted_conflicts: HashSet<String> = HashSet::new();
    loop {
        tokio::select! {
            paths = rx.recv() => {
                if let Some(paths) = paths {
                    for p in paths {
                        if let Ok(rel) = p.strip_prefix(current_dir) {
                            emit("folder_changed", rel.to_str().map(str::to_string), None);
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
                emit("sync_state", None, Some(status.mirror_state.to_string()));

                let last = load_last_synced(&db).await.unwrap_or_default();
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
                        emit("conflict_risk", Some(remote.path.clone()), None);
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
                    emit("conflict_registered", Some(p), None);
                }
                last_emitted_conflicts.retain(|p| status.pending_conflicts.contains(p));
            }
        }
    }
}

fn emit(event: &'static str, path: Option<String>, mirror_state: Option<String>) {
    let ev = FeanorEvent {
        event,
        mirror_state,
        path,
    };
    if let Ok(line) = serde_json::to_string(&ev) {
        println!("{line}");
    }
}
