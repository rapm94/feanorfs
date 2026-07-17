use crate::api::ApiClient;
use crate::commands::do_sync;
use crate::local::ClientDb;
use crate::tray_state::{clear_watch_pid, is_paused, write_watch_pid};
use anyhow::Result;
use feanorfs_common::normalize_path;
use notify::Watcher;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn event_paths_warrant_sync(paths: &[PathBuf]) -> bool {
    for path in paths {
        let Some(path_str) = path.to_str() else {
            continue;
        };
        let normalized = normalize_path(path_str);
        if !normalized.contains("/.feanorfs/")
            && !normalized.contains("/.git/")
            && !normalized.ends_with(".feanorfs")
            && !normalized.ends_with(".git")
        {
            return true;
        }
    }
    false
}

const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(45);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);

async fn drain_event_burst(rx: &mut tokio::sync::mpsc::Receiver<()>, delay: Duration) {
    tokio::time::sleep(delay).await;
    while rx.try_recv().is_ok() {}
}

fn backoff_duration(consecutive_errors: u32) -> Duration {
    if consecutive_errors == 0 {
        return Duration::ZERO;
    }
    let secs = 5u64.saturating_mul(1u64 << consecutive_errors.min(6));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

pub async fn run_watch(
    api: &ApiClient,
    db: &ClientDb,
    current_dir: &Path,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<()> {
    tracing::info!("Starting watcher on {}", current_dir.display());
    println!("Starting change watcher on {}...", current_dir.display());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(100);

    let tx_clone = tx.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if event_paths_warrant_sync(&event.paths) {
                    tracing::debug!("FS event: {:?}", event);
                    let _ = tx_clone.try_send(());
                }
            }
        })?;

    watcher.watch(current_dir, notify::RecursiveMode::Recursive)?;
    write_watch_pid(current_dir);
    struct WatchPidGuard<'a>(&'a Path);
    impl Drop for WatchPidGuard<'_> {
        fn drop(&mut self) {
            clear_watch_pid(self.0);
        }
    }
    let _watch_guard = WatchPidGuard(current_dir);
    println!("Watching for changes... (Press Ctrl+C to stop)");

    let mut consecutive_errors = 0u32;
    let mut poll = tokio::time::interval(IDLE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    println!("Performing initial sync...");
    if !is_paused(current_dir) {
        if let Err(e) = sync_once(
            api,
            db,
            current_dir,
            workspace_id,
            password,
            "initial sync",
            false,
        )
        .await
        {
            consecutive_errors = consecutive_errors.saturating_add(1);
            tracing::error!("Initial sync failed: {:?}", e);
            eprintln!("Initial sync failed: {e:?}");
            eprintln!("Offline — changes will sync when the server is reachable.");
        }
    } else {
        println!("Sync paused — skipping initial sync.");
    }

    loop {
        let backoff = backoff_duration(consecutive_errors);
        tokio::select! {
            maybe = rx.recv() => {
                if maybe.is_none() {
                    break;
                }
                drain_event_burst(&mut rx, DEBOUNCE_INTERVAL).await;

                if backoff > Duration::ZERO {
                    continue;
                }
                if is_paused(current_dir) {
                    continue;
                }

                match sync_once(
                    api,
                    db,
                    current_dir,
                    workspace_id,
                    password,
                    "Changes detected! Syncing",
                    true,
                )
                .await
                {
                    Ok(()) => consecutive_errors = 0,
                    Err(e) => {
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        tracing::error!("Auto-sync failed: {:?}", e);
                        eprintln!("Auto-sync failed: {e:?}");
                    }
                }
            }
            _ = poll.tick() => {
                // Refresh the pid file so `is_watching` doesn't treat a
                // long-running watcher as stale (24h age cutoff).
                write_watch_pid(current_dir);
                if backoff > Duration::ZERO {
                    continue;
                }
                if is_paused(current_dir) {
                    continue;
                }
                if let Err(e) = sync_once(
                    api,
                    db,
                    current_dir,
                    workspace_id,
                    password,
                    "Periodic sync",
                    true,
                )
                .await
                {
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    tracing::error!("Periodic sync failed: {:?}", e);
                }
            }
        }
    }

    Ok(())
}

async fn sync_once(
    api: &ApiClient,
    db: &ClientDb,
    current_dir: &Path,
    workspace_id: &str,
    password: Option<&str>,
    label: &str,
    announce: bool,
) -> Result<()> {
    tracing::info!("{label}");
    if announce {
        println!("{label}...");
    }
    let result = do_sync(api, db, current_dir, workspace_id, password, false).await?;
    println!(
        "Sync complete. Uploaded {}, Downloaded {} (lazy: {}), Local Deletes {}, Remote Deletes {}.",
        result.uploads,
        result.downloads,
        result.placeholders,
        result.deletes_local,
        result.deletes_remote
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{backoff_duration, drain_event_burst, event_paths_warrant_sync};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn sync_worthy_for_workspace_file() {
        assert!(event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/src/main.rs"
        )]));
    }

    #[test]
    fn ignores_feanorfs_metadata_paths() {
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.feanorfs/local_cache.db"
        )]));
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.feanorfs/agents/ci1/foo.txt"
        )]));
    }

    #[test]
    fn ignores_git_paths() {
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.git/index"
        )]));
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/src/.git/config"
        )]));
    }

    #[test]
    fn backoff_grows_with_errors() {
        assert_eq!(backoff_duration(0), Duration::ZERO);
        assert_eq!(backoff_duration(1), Duration::from_secs(10));
        assert!(backoff_duration(10) <= Duration::from_secs(300));
    }

    #[tokio::test]
    async fn bulk_event_burst_runs_one_debounce_pass() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        tx.try_send(()).expect("queue initial event");
        let delayed_tx = tx.clone();
        let delayed_events = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            for _ in 0..99 {
                delayed_tx
                    .try_send(())
                    .expect("queue event during debounce");
            }
        });

        let delay = Duration::from_millis(25);
        let started = tokio::time::Instant::now();
        rx.recv().await.expect("receive initial event");
        drain_event_burst(&mut rx, delay).await;
        delayed_events.await.expect("send delayed events");
        drop(tx);

        assert!(started.elapsed() >= delay);
        assert!(rx.try_recv().is_err(), "entire timed burst must be drained");
    }
}
