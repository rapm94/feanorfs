use crate::api::ApiClient;
use crate::backoff::{BackoffGrowth, BackoffReset, ExponentialBackoff};
use crate::commands::do_sync;
use crate::local::ClientDb;
use crate::tray_state::{clear_watch_pid, is_paused, write_watch_pid};
use anyhow::Result;
use feanorfs_common::normalize_path;
use notify::{Event, EventKind, Watcher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

fn path_warrants_sync(path: &Path) -> bool {
    let Some(path_str) = path.to_str() else {
        return false;
    };
    let normalized = normalize_path(path_str);
    let ignored_component = normalized.split('/').any(|part| {
        matches!(part, ".feanorfs" | ".feanorfsignore" | ".git" | ".jj")
            || is_feanorfs_temp_component(part)
    });
    !ignored_component
}

pub fn event_paths_warrant_sync(paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| path_warrants_sync(path))
}

/// Applies workspace ignore rules to event paths relative to the watched
/// root. This is required for agent worktrees stored below the private
/// `~/.feanorfs` state directory: the private ancestor is not part of the
/// worktree and must not cause every legitimate child event to be ignored.
pub fn event_paths_warrant_sync_under(root: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| {
        let relative = path.strip_prefix(root).unwrap_or(path);
        path_warrants_sync(relative)
    })
}

pub fn event_warrants_sync(event: &Event) -> bool {
    !matches!(event.kind, EventKind::Access(_)) && event_paths_warrant_sync(&event.paths)
}

pub fn event_warrants_sync_under(event: &Event, root: &Path) -> bool {
    !matches!(event.kind, EventKind::Access(_))
        && event_paths_warrant_sync_under(root, &event.paths)
}

const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(45);
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);

/// Watcher sync-retry backoff: base 5 s doubling from the first failure,
/// 300 s cap. Sequence (failures 0..): 0, 10, 20, 40, 80, 160, 300, 300, ...
const WATCH_BACKOFF: ExponentialBackoff =
    ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(300))
        .with_growth(BackoffGrowth::DoublesFromFirstFailure)
        .with_reset(BackoffReset::Immediate);

async fn drain_event_burst(rx: &mut tokio::sync::mpsc::Receiver<()>, delay: Duration) {
    tokio::time::sleep(delay).await;
    while rx.try_recv().is_ok() {}
}

/// Borrowed per-workspace watcher identity shared by every sync attempt.
pub struct WatchTarget<'a> {
    pub api: &'a ApiClient,
    pub db: &'a ClientDb,
    pub dir: &'a Path,
    pub workspace_id: &'a str,
    pub password: Option<&'a str>,
}

pub async fn run_watch(target: WatchTarget<'_>, lazy: bool) -> Result<()> {
    let WatchTarget {
        api,
        db,
        dir: current_dir,
        workspace_id,
        password,
    } = target;
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
    let mut retry = SyncRetryGate::new();
    // Reusable bounded head observer: a healthy hub wakes the watcher as soon
    // as the opaque head changes; the periodic window remains the recovery
    // backstop for unsupported hubs and transient transport failures.
    let mut head_observer = feanorfs_agent_core::HeadObserver::new(api, workspace_id);

    println!("Performing initial sync...");
    if !is_paused(current_dir) {
        match sync_once(
            api,
            db,
            current_dir,
            workspace_id,
            password,
            "initial sync",
            false,
            lazy,
        )
        .await
        {
            Ok(()) => {
                retry.noted_success();
                acknowledge_current_head(&mut head_observer, api, workspace_id).await;
            }
            Err(e) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                retry.noted_failure(consecutive_errors);
                publish_sync_failure(current_dir, db).await;
                tracing::error!("Initial sync failed: {:?}", e);
                eprintln!("Initial sync failed: {e:?}");
                eprintln!("Offline — changes will sync when the server is reachable.");
            }
        }
    } else {
        println!("Sync paused — skipping initial sync.");
    }

    // Backoff is a real wait before the next attempt, not a permanent skip:
    // the old `if backoff > 0 { continue }` gate meant one failed sync (a
    // transient hub restart, an offline laptop) permanently silenced the
    // watcher — it kept refreshing `watch.pid` every poll so the tray still
    // reported "watching", but no sync ever ran again.
    loop {
        tokio::select! {
            maybe = rx.recv() => {
                if maybe.is_none() {
                    break;
                }
                drain_event_burst(&mut rx, DEBOUNCE_INTERVAL).await;

                if !retry.ready() {
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
                    lazy,
                )
                .await
                {
                    Ok(()) => {
                        consecutive_errors = 0;
                        retry.noted_success();
                        acknowledge_current_head(&mut head_observer, api, workspace_id).await;
                    }
                    Err(e) => {
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        retry.noted_failure(consecutive_errors);
                        publish_sync_failure(current_dir, db).await;
                        tracing::error!("Auto-sync failed: {:?}", e);
                        eprintln!("Auto-sync failed: {e:?}");
                    }
                }
            }
            observation = head_observer.observe(IDLE_POLL_INTERVAL) => {
                // Refresh the pid file so `is_watching` doesn't treat a
                // long-running watcher as stale (24h age cutoff).
                write_watch_pid(current_dir);
                if !retry.ready() {
                    continue;
                }
                if is_paused(current_dir) {
                    continue;
                }
                // Skip only when a wait-supported hub authoritatively
                // observed the whole window with an unchanged head; changed
                // heads, unsupported hubs, and errors keep the backstop.
                let unchanged_on_supported_hub = matches!(
                    observation.as_ref(),
                    Ok(observed) if !observed.changed && !observed.unsupported
                );
                if unchanged_on_supported_hub {
                    continue;
                }
                let label = if observation
                    .as_ref()
                    .map(|observed| observed.changed)
                    .unwrap_or(false)
                {
                    "Head change detected! Syncing"
                } else {
                    "Periodic sync"
                };
                match sync_once(api, db, current_dir, workspace_id, password, label, true, lazy)
                    .await
                {
                    Ok(()) => {
                        consecutive_errors = 0;
                        retry.noted_success();
                        acknowledge_current_head(&mut head_observer, api, workspace_id).await;
                    }
                    Err(e) => {
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        retry.noted_failure(consecutive_errors);
                        publish_sync_failure(current_dir, db).await;
                        tracing::error!("Periodic sync failed: {:?}", e);
                    }
                }
            }
            _ = wait_for_retry(retry.deadline()) => {
                if is_paused(current_dir) {
                    retry.postpone(IDLE_POLL_INTERVAL);
                    continue;
                }
                match sync_once(
                    api,
                    db,
                    current_dir,
                    workspace_id,
                    password,
                    "Retrying sync",
                    true,
                    lazy,
                )
                .await
                {
                    Ok(()) => {
                        consecutive_errors = 0;
                        retry.noted_success();
                        acknowledge_current_head(&mut head_observer, api, workspace_id).await;
                    }
                    Err(e) => {
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        retry.noted_failure(consecutive_errors);
                        publish_sync_failure(current_dir, db).await;
                        tracing::error!("Retry sync failed: {:?}", e);
                        eprintln!("Retry sync failed: {e:?}");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Marks the current head as observed after a successful sync so the
/// watcher's own publication never re-triggers a redundant pass.
async fn acknowledge_current_head(
    observer: &mut feanorfs_agent_core::HeadObserver<'_>,
    api: &ApiClient,
    workspace_id: &str,
) {
    if let Ok(head) = api.get_head(workspace_id).await {
        observer.acknowledge(head);
    }
}

/// Gates sync attempts after failures with a real wait, so a transient error
/// delays the next attempt instead of silencing the watcher forever.
struct SyncRetryGate {
    next_attempt: Option<Instant>,
    now: Box<dyn Fn() -> Instant>,
}

impl SyncRetryGate {
    fn new() -> Self {
        Self {
            next_attempt: None,
            now: Box::new(Instant::now),
        }
    }

    #[cfg(test)]
    fn with_clock(now: impl Fn() -> Instant + 'static) -> Self {
        Self {
            next_attempt: None,
            now: Box::new(now),
        }
    }

    fn ready(&self) -> bool {
        self.next_attempt
            .is_none_or(|deadline| (self.now)() >= deadline)
    }

    fn deadline(&self) -> Option<Instant> {
        self.next_attempt
    }

    fn noted_success(&mut self) {
        self.next_attempt = None;
    }

    fn noted_failure(&mut self, consecutive_errors: u32) {
        self.next_attempt = Some((self.now)() + WATCH_BACKOFF.delay(consecutive_errors));
    }

    fn postpone(&mut self, delay: Duration) {
        self.next_attempt = Some((self.now)() + delay);
    }
}

/// Sleeps until an armed retry deadline. An unarmed gate remains pending, so
/// it cannot create an immediate-select loop after a successful sync.
async fn wait_for_retry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

async fn publish_sync_failure(current_dir: &Path, db: &ClientDb) {
    // Keep routine tray reads truthful without copying error text (which may
    // contain paths or endpoints) into the bounded secret-free snapshot.
    let _ =
        crate::tray::publish_worker_status(current_dir, &crate::commands::MirrorState::Offline, db)
            .await;
}

#[allow(clippy::too_many_arguments)]
async fn sync_once(
    api: &ApiClient,
    db: &ClientDb,
    current_dir: &Path,
    workspace_id: &str,
    password: Option<&str>,
    label: &str,
    announce: bool,
    lazy: bool,
) -> Result<()> {
    tracing::info!("{label}");
    if announce {
        println!("{label}...");
    }
    let result = do_sync(api, db, current_dir, workspace_id, password, lazy).await?;
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
        drain_event_burst, event_paths_warrant_sync, event_paths_warrant_sync_under,
        event_warrants_sync, event_warrants_sync_under, wait_for_retry, SyncRetryGate,
        WATCH_BACKOFF,
    };
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    use notify::{Event, EventKind};
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::Instant;

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
    fn agent_worktree_events_ignore_private_ancestor_but_not_relative_metadata() {
        let root =
            PathBuf::from("/home/test/.feanorfs/workspaces/opaque/agents/mac-opencode/worktree");
        assert!(event_paths_warrant_sync_under(
            &root,
            &[root.join("src/App.jsx")]
        ));
        assert!(!event_paths_warrant_sync_under(
            &root,
            &[root.join("src/.git/index")]
        ));

        let mutation =
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(root.join("src/App.jsx"));
        assert!(event_warrants_sync_under(&mutation, &root));
        let access = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
            .add_path(root.join("src/App.jsx"));
        assert!(!event_warrants_sync_under(&access, &root));
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
        assert_eq!(WATCH_BACKOFF.delay(0), Duration::ZERO);
        assert_eq!(WATCH_BACKOFF.delay(1), Duration::from_secs(10));
        assert!(WATCH_BACKOFF.delay(10) <= Duration::from_secs(300));
    }

    #[test]
    fn retry_gate_waits_out_backoff_then_recovers() {
        // One failed sync must delay the next attempt, not silence the
        // watcher forever: the pre-fix loop skipped every sync while the
        // error count stayed positive.
        let clock = std::rc::Rc::new(std::cell::Cell::new(Instant::now()));
        let test_clock = std::rc::Rc::clone(&clock);
        let mut gate = SyncRetryGate::with_clock(move || test_clock.get());
        assert!(gate.ready());
        assert!(gate.deadline().is_none());
        gate.noted_failure(1);
        assert!(!gate.ready(), "attempt must wait out the backoff");
        assert!(gate.deadline().is_some(), "failure must arm a retry timer");
        clock.set(clock.get() + Duration::from_secs(9));
        assert!(!gate.ready());
        clock.set(clock.get() + Duration::from_secs(2));
        assert!(gate.ready(), "backoff elapsed; the next attempt may run");
        // A success clears the failure state so the next attempt is immediate.
        gate.noted_success();
        assert!(gate.ready());
        assert!(gate.deadline().is_none());
        // Failures compound up to the bounded maximum.
        gate.noted_failure(10);
        assert!(!gate.ready());
        clock.set(clock.get() + Duration::from_secs(300));
        assert!(gate.ready());
    }

    #[tokio::test]
    async fn retry_wait_is_pending_until_an_armed_deadline() {
        assert!(
            tokio::time::timeout(Duration::from_millis(20), wait_for_retry(None))
                .await
                .is_err(),
            "an unarmed retry must not create an immediate-select loop"
        );

        tokio::time::timeout(
            Duration::from_millis(250),
            wait_for_retry(Some(Instant::now() + Duration::from_millis(20))),
        )
        .await
        .expect("an armed retry deadline must wake the watcher");
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
