use crate::api::ApiClient;
use crate::commands::do_sync;
use crate::local::ClientDb;
use crate::tray_state::{clear_watch_pid, is_paused, write_watch_pid};
use anyhow::Result;
use feanorfs_common::normalize_path;
use notify::{Event, EventKind, Watcher};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// True when a path component is a FeanorFS temp file. The atomic-write and
/// large-file temp names are `.feanorfs-tmp-<pid>-<seq>-<attempt>` (numeric
/// segments only), so a real user file merely sharing the prefix is still
/// watched.
fn is_feanorfs_temp_component(part: &str) -> bool {
    part.strip_prefix(".feanorfs-tmp-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('-')
                .all(|segment| !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()))
    })
}

pub fn event_paths_warrant_sync(paths: &[PathBuf]) -> bool {
    for path in paths {
        let Some(path_str) = path.to_str() else {
            continue;
        };
        let normalized = normalize_path(path_str);
        let ignored_component = normalized.split('/').any(|part| {
            matches!(part, ".feanorfs" | ".feanorfsignore" | ".git" | ".jj")
                || is_feanorfs_temp_component(part)
        });
        if !ignored_component {
            return true;
        }
    }
    false
}

pub fn event_warrants_sync(event: &Event) -> bool {
    !matches!(event.kind, EventKind::Access(_)) && event_paths_warrant_sync(&event.paths)
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
                if event_warrants_sync(&event) {
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
            publish_sync_failure(current_dir, db).await;
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
                        publish_sync_failure(current_dir, db).await;
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
                    publish_sync_failure(current_dir, db).await;
                    tracing::error!("Periodic sync failed: {:?}", e);
                }
            }
        }
    }

    Ok(())
}

async fn publish_sync_failure(current_dir: &Path, db: &ClientDb) {
    // Keep routine tray reads truthful without copying error text (which may
    // contain paths or endpoints) into the bounded secret-free snapshot.
    let _ =
        crate::tray::publish_worker_status(current_dir, &crate::commands::MirrorState::Offline, db)
            .await;
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
    // Publish the bounded secret-free tray status snapshot so routine tray
    // refreshes never scan the project or take the sync lock.
    let _ = crate::tray::publish_worker_status(current_dir, &result.mirror_state, db).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        backoff_duration, drain_event_burst, event_paths_warrant_sync, event_warrants_sync,
    };
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    use notify::{Event, EventKind};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn sync_worthy_for_workspace_file() {
        assert!(event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/src/main.rs"
        )]));
    }

    #[test]
    fn ignores_access_events_caused_by_the_scanner_itself() {
        let event = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
            .add_path(PathBuf::from("/workspace/src"));
        assert!(!event_warrants_sync(&event));
    }

    #[test]
    fn accepts_mutating_events_for_workspace_files() {
        let event = Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/workspace/src/main.rs"));
        assert!(event_warrants_sync(&event));
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
    fn ignores_only_numeric_feanorfs_temp_files() {
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.feanorfs-tmp-123-0"
        )]));
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.feanorfs-tmp-84583-2-1"
        )]));
        // A user file merely sharing the prefix must still be watched.
        assert!(event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.feanorfs-tmp-notes.txt"
        )]));
        assert!(event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.feanorfs-tmp-final"
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
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.jj/repo/store"
        )]));
        assert!(!event_paths_warrant_sync(&[PathBuf::from(
            "/workspace/.feanorfsignore"
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
        for _ in 0..100 {
            tx.try_send(()).expect("queue burst event");
        }
        drop(tx);

        let delay = Duration::from_millis(25);
        let started = tokio::time::Instant::now();
        rx.recv().await.expect("receive initial event");
        drain_event_burst(&mut rx, delay).await;

        assert!(started.elapsed() >= delay);
        assert!(
            rx.try_recv().is_err(),
            "entire queued burst must be drained"
        );
    }
}
