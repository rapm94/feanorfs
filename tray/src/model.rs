//! Tray state and derived view model.
//!
//! Pure state container plus the derived projections the menu and dialogs
//! render from it. No native windows, no subprocesses (besides the owned
//! `sync` watch child, which is the state's own responsibility).

use crate::feanorfs::{
    background_service_managed, feanorfs_bin, graceful_stop_child, tray_recent, tray_status,
    workspace_has_config,
};
use feanorfs_common::tray_contract::{RecentWorkspacesResult, TrayStatusResult};
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::ffi::{OsStr, OsString};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const REFRESH_SECS: u64 = 10;
const RECENT_CACHE_SECS: u64 = 30;
const MAX_WATCH_FAILURES: u32 = 3;
const FAST_EXIT_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SetupKind {
    AddFolder,
    JoinFolder,
    Repair,
}

pub(crate) struct AppState {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) managed_launch: bool,
    watch_child: Option<Child>,
    owns_watch: bool,
    watch_failures: u32,
    last_spawn_at: Option<Instant>,
    respawn_disabled: bool,
    pub(crate) status_inflight: bool,
    pub(crate) status_pending: bool,
    pub(crate) task_generation: u64,
    pub(crate) last_status: Option<TrayStatusResult>,
    pub(crate) status_failed: bool,
    pub(crate) error_message: Option<String>,
    pub(crate) recent: Option<RecentWorkspacesResult>,
    pub(crate) recent_fetched_at: Option<Instant>,
    managed_service: Option<bool>,
    pub(crate) setup_inflight: bool,
    pub(crate) setup_kind: Option<SetupKind>,
    pub(crate) stop_inflight: bool,
    pub(crate) switch_inflight: bool,
    pub(crate) pair_inflight: bool,
    pub(crate) recovery_inflight: bool,
    pub(crate) health_inflight: bool,
    pub(crate) update_inflight: bool,
    pub(crate) pair_cancel: Option<std::sync::mpsc::Sender<()>>,
    pub(crate) quit_pending: bool,
    pub(crate) last_menu_revision: Cell<Option<u64>>,
}

impl AppState {
    pub(crate) fn new(workspace: Option<PathBuf>) -> Self {
        Self {
            workspace,
            managed_launch: false,
            watch_child: None,
            owns_watch: false,
            watch_failures: 0,
            last_spawn_at: None,
            respawn_disabled: false,
            status_inflight: false,
            status_pending: false,
            task_generation: 0,
            last_status: None,
            status_failed: false,
            error_message: None,
            recent: None,
            recent_fetched_at: None,
            managed_service: None,
            setup_inflight: false,
            setup_kind: None,
            stop_inflight: false,
            switch_inflight: false,
            pair_inflight: false,
            recovery_inflight: false,
            health_inflight: false,
            update_inflight: false,
            pair_cancel: None,
            quit_pending: false,
            last_menu_revision: Cell::new(None),
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.last_status.as_ref().is_some_and(|s| s.paused)
    }

    pub(crate) fn external_watcher_active(&self) -> bool {
        self.watch_child.is_none() && self.last_status.as_ref().is_some_and(|s| s.watching)
    }

    pub(crate) fn has_managed_service(&mut self) -> bool {
        if let Some(managed) = self.managed_service {
            return managed;
        }
        let managed = self
            .workspace
            .as_deref()
            .is_some_and(background_service_managed);
        self.managed_service = Some(managed);
        managed
    }

    pub(crate) fn start_watch(&mut self) {
        if self.is_paused() || self.respawn_disabled || self.has_managed_service() {
            return;
        }
        if self.watch_child.is_some() {
            return;
        }
        if self.external_watcher_active() {
            return;
        }
        let Some(workspace) = self.workspace.clone() else {
            return;
        };

        match Command::new(feanorfs_bin())
            .args(["sync"])
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.watch_child = Some(child);
                self.owns_watch = true;
                self.last_spawn_at = Some(Instant::now());
            }
            Err(e) => {
                self.respawn_disabled = true;
                self.error_message = Some(format!(
                    "Automatic syncing could not start because the FeanorFS command is unavailable. Your files were not changed. Reinstall FeanorFS and try again. Details: {e}"
                ));
            }
        }
    }

    pub(crate) fn check_watch_alive(&mut self) {
        if self.respawn_disabled || self.is_paused() {
            return;
        }

        if let Some(child) = &mut self.watch_child {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.watch_child = None;
                    self.owns_watch = false;
                    let fast_exit = self
                        .last_spawn_at
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(FAST_EXIT_SECS));
                    if fast_exit {
                        self.watch_failures = self.watch_failures.saturating_add(1);
                    } else {
                        self.watch_failures = 0;
                    }
                    if self.watch_failures >= MAX_WATCH_FAILURES {
                        self.respawn_disabled = true;
                        self.error_message = Some(
                            "Automatic syncing stopped after repeated failures. Your files were not changed. Quit and reopen FeanorFS; if this happens again, choose Check System Health… from the tray.".into(),
                        );
                        return;
                    }
                    self.start_watch();
                }
                Ok(None) => {
                    if self
                        .last_spawn_at
                        .is_some_and(|t| t.elapsed() >= Duration::from_secs(FAST_EXIT_SECS))
                    {
                        self.watch_failures = 0;
                    }
                }
                Err(_) => {
                    self.watch_child = None;
                    self.owns_watch = false;
                    self.watch_failures = self.watch_failures.saturating_add(1);
                    if self.watch_failures >= MAX_WATCH_FAILURES {
                        self.respawn_disabled = true;
                        self.error_message = Some(
                            "Automatic syncing stopped after repeated failures. Your files were not changed. Quit and reopen FeanorFS; if this happens again, choose Check System Health… from the tray.".into(),
                        );
                        return;
                    }
                    self.start_watch();
                }
            }
        } else if self.external_watcher_active() {
            // Distinguish the normal OS-managed watcher from a sync command
            // the user really started in a terminal. The menu should never
            // describe automatic background syncing as a terminal process.
            let _ = self.has_managed_service();
        } else {
            self.start_watch();
        }
    }

    pub(crate) fn stop_watch(&mut self) {
        if let Some(mut child) = self.watch_child.take() {
            graceful_stop_child(&mut child);
            self.owns_watch = false;
        }
    }

    pub(crate) fn cached_recent(&mut self) {
        let stale = self
            .recent_fetched_at
            .map(|t| t.elapsed().as_secs() >= RECENT_CACHE_SECS)
            .unwrap_or(true);
        if stale {
            self.recent = tray_recent();
            self.recent_fetched_at = Some(Instant::now());
        }
    }

    pub(crate) fn invalidate_recent(&mut self) {
        self.recent = None;
        self.recent_fetched_at = None;
    }

    pub(crate) fn reset_watch_policy(&mut self) {
        self.watch_failures = 0;
        self.respawn_disabled = false;
        self.status_failed = false;
        self.error_message = None;
        self.managed_service = None;
    }

    pub(crate) fn adopt_recent_if_unconfigured(&mut self) -> bool {
        if self.workspace.is_some()
            || self.setup_inflight
            || self.stop_inflight
            || self.switch_inflight
            || self.pair_inflight
        {
            return false;
        }
        self.cached_recent();
        let Some(recent) = self.recent.as_ref() else {
            return false;
        };
        let candidate = configured_recent_workspace(recent);
        let Some(candidate) = candidate else {
            return false;
        };
        self.workspace = Some(candidate);
        self.reset_watch_policy();
        true
    }

    pub(crate) fn cancel_pairing(&mut self) {
        if let Some(cancel) = self.pair_cancel.take() {
            let _ = cancel.send(());
        }
    }
}

pub(crate) fn configured_recent_workspace(recent: &RecentWorkspacesResult) -> Option<PathBuf> {
    configured_recent_workspace_with(recent, workspace_has_config)
}

pub(crate) fn configured_recent_workspace_with(
    recent: &RecentWorkspacesResult,
    has_config: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    recent
        .active
        .iter()
        .chain(recent.workspaces.iter().map(|workspace| &workspace.path))
        .map(PathBuf::from)
        .find(|path| has_config(path))
}

pub(crate) fn unavailable_workspace_count(recent: &RecentWorkspacesResult) -> usize {
    unavailable_workspace_count_with(recent, workspace_has_config)
}

pub(crate) fn unavailable_workspace_count_with(
    recent: &RecentWorkspacesResult,
    has_config: impl Fn(&Path) -> bool,
) -> usize {
    recent
        .workspaces
        .iter()
        .filter(|workspace| !has_config(Path::new(&workspace.path)))
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MirroredFolderMenuItem {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) available: bool,
    pub(crate) selected: bool,
}

/// Exact canonical UTF-8 workspace identity for selection matching. Returns
/// `None` for paths that are not valid UTF-8: an unencodable workspace must
/// never alias another workspace through a lossy comparison.
fn canonical_path_string(path: &Path) -> Option<String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical.to_str().map(str::to_string)
}

fn same_workspace_path(left: &str, right: &str) -> bool {
    canonical_path_string(Path::new(left)) == canonical_path_string(Path::new(right))
        && canonical_path_string(Path::new(left)).is_some()
}

fn compact_workspace_path(path: &Path) -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from);
    if let Some(relative) = home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        if relative.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

pub(crate) fn workspace_switch_item_with(
    label: &str,
    path: &str,
    active: Option<&str>,
    has_config: impl Fn(&Path) -> bool,
) -> MirroredFolderMenuItem {
    let available = has_config(Path::new(path));
    let selected = active.is_some_and(|active| same_workspace_path(active, path));
    let mut menu_label = format!("{label} — {}", compact_workspace_path(Path::new(path)));
    if !available {
        menu_label.push_str(" — unavailable");
    }
    MirroredFolderMenuItem {
        id: format!("switch:{path}"),
        label: menu_label,
        available,
        selected,
    }
}

pub(crate) fn mirrored_folder_menu_items(state: &AppState) -> Vec<MirroredFolderMenuItem> {
    mirrored_folder_menu_items_with(state, workspace_has_config)
}

pub(crate) fn mirrored_folder_menu_items_with(
    state: &AppState,
    has_config: impl Fn(&Path) -> bool + Copy,
) -> Vec<MirroredFolderMenuItem> {
    let mut workspaces = state
        .recent
        .as_ref()
        .map(|recent| {
            recent
                .workspaces
                .iter()
                .map(|workspace| (workspace.path.clone(), workspace.label.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(workspace) = state.workspace.as_deref() {
        // A workspace whose exact path is not UTF-8 cannot be identified for
        // selection; skip it rather than aliasing it through a lossy string.
        if let Some(path) = canonical_path_string(workspace) {
            if !workspaces
                .iter()
                .any(|(candidate, _)| same_workspace_path(candidate, &path))
            {
                let label = workspace
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("folder")
                    .to_string();
                workspaces.insert(0, (path, label));
            }
        }
    }

    let active = state
        .workspace
        .as_deref()
        .and_then(canonical_path_string)
        .or_else(|| {
            state
                .recent
                .as_ref()
                .and_then(|recent| recent.active.clone())
        });
    workspaces
        .iter()
        .map(|(path, label)| workspace_switch_item_with(label, path, active.as_deref(), has_config))
        .collect()
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

pub(crate) fn is_paused_on_disk(workspace: &Path) -> bool {
    tray_status(workspace).is_ok_and(|status| status.paused)
}

pub(crate) fn resolve_initial_workspace() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FEANORFS_WORKSPACE") {
        let path = expand_tilde(&p);
        return workspace_has_config(&path).then_some(path);
    }
    let recent = tray_recent()?;
    recent
        .active
        .into_iter()
        .chain(recent.workspaces.into_iter().map(|w| w.path))
        .map(PathBuf::from)
        .find(|p| workspace_has_config(p))
}

pub(crate) fn first_run_requested(args: &[OsString]) -> bool {
    args.iter()
        .any(|argument| argument == OsStr::new("--first-run"))
}

pub(crate) fn should_prompt_first_run(requested: bool, workspace: Option<&Path>) -> bool {
    requested && workspace.is_none()
}

pub(crate) fn menu_actions_enabled(state: &AppState) -> bool {
    !state.setup_inflight
        && !state.stop_inflight
        && !state.switch_inflight
        && !state.pair_inflight
        && !state.recovery_inflight
}

pub(crate) fn unmanaged_terminal_watcher_active(
    state: &AppState,
    status: &TrayStatusResult,
) -> bool {
    status.watching && !state.owns_watch && state.managed_service == Some(false)
}

pub(crate) fn activity_header(state: &AppState) -> Option<&'static str> {
    if state.setup_inflight {
        return Some(match state.setup_kind {
            Some(SetupKind::AddFolder) => "FeanorFS — adding folder…",
            Some(SetupKind::JoinFolder) => "FeanorFS — joining shared folder…",
            Some(SetupKind::Repair) => "FeanorFS — repairing mirroring…",
            None => "FeanorFS — setting up folder…",
        });
    }
    if state.stop_inflight {
        return Some("FeanorFS — stopping mirroring…");
    }
    if state.switch_inflight {
        return Some("FeanorFS — switching folders…");
    }
    if state.pair_inflight {
        return Some("FeanorFS — sharing securely…");
    }
    if state.recovery_inflight {
        return Some("FeanorFS — recovery in progress…");
    }
    None
}

pub(crate) fn menu_revision(state: &AppState) -> u64 {
    let mut hasher = DefaultHasher::new();
    state.workspace.hash(&mut hasher);
    state.owns_watch.hash(&mut hasher);
    state.error_message.hash(&mut hasher);
    state.setup_inflight.hash(&mut hasher);
    state.setup_kind.hash(&mut hasher);
    state.stop_inflight.hash(&mut hasher);
    state.switch_inflight.hash(&mut hasher);
    state.pair_inflight.hash(&mut hasher);
    state.recovery_inflight.hash(&mut hasher);
    state.health_inflight.hash(&mut hasher);
    state.update_inflight.hash(&mut hasher);
    if let Some(status) = state.last_status.as_ref() {
        serde_json::to_vec(status)
            .expect("tray status is serializable")
            .hash(&mut hasher);
    }
    if let Some(recent) = state.recent.as_ref() {
        serde_json::to_vec(recent)
            .expect("recent workspace state is serializable")
            .hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::tray_contract::{RecentWorkspaceEntry, TrayAgentsSummary};

    fn make_status(mirror_state: &str, paused: bool) -> TrayStatusResult {
        TrayStatusResult {
            mirror_state: mirror_state.into(),
            paused,
            watching: true,
            workspace_path: "/tmp/test".into(),
            workspace_id: "test-workspace".into(),
            workspace_label: "test".into(),
            pending_conflict_count: 0,
            pending_conflicts: vec![],
            agents: TrayAgentsSummary {
                working: 0,
                need_attention: 0,
                entries: vec![],
            },
        }
    }

    #[test]
    fn empty_state_is_safe_before_setup() {
        let mut state = AppState::new(None);
        assert!(state.workspace.is_none());
        assert!(state.watch_child.is_none());
        assert!(!state.setup_inflight);
        assert_eq!(state.setup_kind, None);
        assert!(!state.stop_inflight);
        assert!(!state.switch_inflight);
        assert!(!state.pair_inflight);
        assert!(!state.recovery_inflight);
        assert!(!state.health_inflight);
        assert!(!state.update_inflight);
        assert!(state.pair_cancel.is_none());
        assert_eq!(state.last_menu_revision.get(), None);
        assert!(!state.has_managed_service());
    }

    #[test]
    fn normal_background_sync_is_never_labeled_as_a_terminal_process() {
        let status = make_status("idle", false);
        let mut state = AppState::new(Some(PathBuf::from("/tmp/test")));
        state.managed_service = Some(true);
        assert!(!unmanaged_terminal_watcher_active(&state, &status));

        state.managed_service = Some(false);
        assert!(unmanaged_terminal_watcher_active(&state, &status));
    }

    #[test]
    fn unchanged_refresh_does_not_replace_the_native_menu() {
        let mut state = AppState::new(Some(PathBuf::from("/tmp/test")));
        state.last_status = Some(make_status("idle", false));
        let initial = menu_revision(&state);

        // Cache bookkeeping changes every refresh but has no visible menu
        // effect, so it must not close an open macOS status menu.
        state.recent_fetched_at = Some(Instant::now());
        assert_eq!(menu_revision(&state), initial);

        state.last_status.as_mut().unwrap().paused = true;
        assert_ne!(menu_revision(&state), initial);
    }

    #[test]
    fn first_run_hint_prompts_only_for_an_unconfigured_tray() {
        assert!(first_run_requested(&[OsString::from("--first-run")]));
        assert!(!first_run_requested(&[OsString::from("--not-first-run")]));
        assert!(should_prompt_first_run(true, None));
        assert!(!should_prompt_first_run(
            true,
            Some(Path::new("/configured"))
        ));
        assert!(!should_prompt_first_run(false, None));
    }

    #[test]
    fn configured_recent_workspace_skips_stale_entries() {
        let root = std::env::temp_dir().join(format!(
            "feanorfs-tray-recent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let stale = root.join("stale");
        let configured = root.join("configured");
        std::fs::create_dir_all(&configured).unwrap();

        let recent = RecentWorkspacesResult {
            active: Some(stale.to_string_lossy().into_owned()),
            workspaces: vec![RecentWorkspaceEntry {
                path: configured.to_string_lossy().into_owned(),
                workspace_id: "fsw1-test".into(),
                label: "configured".into(),
            }],
        };
        assert_eq!(
            configured_recent_workspace_with(&recent, |path| path == configured),
            Some(configured)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_workspace_is_labeled_disabled_and_counted() {
        let root = std::env::temp_dir().join(format!(
            "feanorfs-tray-unavailable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let available = root.join("available");
        let unavailable = root.join("unavailable");
        std::fs::create_dir_all(&available).unwrap();
        let recent = RecentWorkspacesResult {
            active: Some(unavailable.to_string_lossy().into_owned()),
            workspaces: vec![
                RecentWorkspaceEntry {
                    path: unavailable.to_string_lossy().into_owned(),
                    workspace_id: "fsw1-unavailable".into(),
                    label: "offline drive".into(),
                },
                RecentWorkspaceEntry {
                    path: available.to_string_lossy().into_owned(),
                    workspace_id: "fsw1-available".into(),
                    label: "available".into(),
                },
            ],
        };

        let has_config = |path: &Path| path == available;
        assert_eq!(unavailable_workspace_count_with(&recent, has_config), 1);
        let unavailable_item = workspace_switch_item_with(
            "offline drive",
            &unavailable.to_string_lossy(),
            recent.active.as_deref(),
            has_config,
        );
        assert!(!unavailable_item.available);
        assert!(unavailable_item.selected);
        assert!(unavailable_item.label.contains("offline drive"));
        assert!(unavailable_item.label.ends_with("— unavailable"));

        let available_item =
            workspace_switch_item_with("available", &available.to_string_lossy(), None, has_config);
        assert!(available_item.available);
        assert!(!available_item.selected);
        assert!(available_item
            .label
            .contains(&available.display().to_string()));

        // The tray's in-memory selection is authoritative for Open Folder and
        // every other folder-scoped action, even before a cached registry is
        // refreshed. Both followed folders remain present in the selector.
        let mut state = AppState::new(Some(available.clone()));
        state.recent = Some(recent);
        let items = mirrored_folder_menu_items_with(&state, has_config);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items.iter().filter(|item| item.selected).count(),
            1,
            "exactly one folder must be visibly selected"
        );
        let selected = items.iter().find(|item| item.selected).unwrap();
        assert_eq!(selected.id, format!("switch:{}", available.display()));
        assert!(selected.available);

        std::fs::remove_dir_all(root).unwrap();
    }
}
