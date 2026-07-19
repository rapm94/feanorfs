mod feanorfs;
mod icons;
mod password_dialog;

use feanorfs::feanorfs_bin;
use feanorfs::{
    agent_land, background_service_managed, background_service_start, background_service_stop,
    check_for_updates, clear_pairing_clipboard, conflicts_keep, copy_pairing_clipboard,
    export_recovery_kit, forget_unavailable_workspaces, graceful_stop_child, import_recovery_kit,
    join_workspace, run_pairing_session, start_workspace, stop_workspace, sync_once, system_health,
    tray_activate, tray_pause, tray_recent, tray_status, workspace_has_config, HealthReport,
    HealthStatus, PairSessionEvent, UpdateCheckResult, UpdateStatus,
};
use feanorfs_common::tray_contract::{RecentWorkspacesResult, TrayStatusResult};
use icons::{icon_for, visual_from_state, TrayVisual};
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::ffi::{OsStr, OsString};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{TrayIcon, TrayIconBuilder};

const REFRESH_SECS: u64 = 10;
const RECENT_CACHE_SECS: u64 = 30;
const MAX_WATCH_FAILURES: u32 = 3;
const FAST_EXIT_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SetupKind {
    AddFolder,
    JoinFolder,
    Repair,
}

#[derive(Clone)]
enum Action {
    Refresh,
    FirstRun,
    StatusReady {
        generation: u64,
        workspace: PathBuf,
        status: Result<TrayStatusResult, String>,
    },
    HealthReady {
        workspace: PathBuf,
        report: Result<HealthReport, String>,
    },
    UpdateReady(Result<UpdateCheckResult, String>),
    MenuClick(String),
    TaskDone {
        error: Option<String>,
        restart_watch: bool,
        /// `Some` only for pause/resume tasks — applied on success only.
        set_paused: Option<bool>,
        generation: u64,
    },
    SwitchDone {
        generation: u64,
        path: PathBuf,
        error: Option<String>,
    },
    ForgetUnavailableDone {
        generation: u64,
        before: usize,
        result: Result<RecentWorkspacesResult, String>,
    },
    SetupDone {
        generation: u64,
        path: PathBuf,
        kind: SetupKind,
        error: Option<String>,
    },
    SetupCanceled {
        generation: u64,
    },
    StopDone {
        generation: u64,
        path: PathBuf,
        error: Option<String>,
    },
    PairReady {
        generation: u64,
        code: String,
        expires_in_seconds: u64,
    },
    PairDone {
        generation: u64,
        paired: bool,
        canceled: bool,
        error: Option<String>,
    },
    RecoveryDone {
        generation: u64,
        restored_folder: Option<PathBuf>,
        error: Option<String>,
    },
}

struct AppState {
    workspace: Option<PathBuf>,
    watch_child: Option<Child>,
    owns_watch: bool,
    watch_failures: u32,
    last_spawn_at: Option<Instant>,
    respawn_disabled: bool,
    status_inflight: bool,
    status_pending: bool,
    task_generation: u64,
    last_status: Option<TrayStatusResult>,
    status_failed: bool,
    error_message: Option<String>,
    recent: Option<RecentWorkspacesResult>,
    recent_fetched_at: Option<Instant>,
    managed_service: Option<bool>,
    setup_inflight: bool,
    setup_kind: Option<SetupKind>,
    stop_inflight: bool,
    switch_inflight: bool,
    pair_inflight: bool,
    recovery_inflight: bool,
    health_inflight: bool,
    update_inflight: bool,
    pair_cancel: Option<std::sync::mpsc::Sender<()>>,
    quit_pending: bool,
    last_menu_revision: Cell<Option<u64>>,
}

impl AppState {
    fn new(workspace: Option<PathBuf>) -> Self {
        Self {
            workspace,
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

    fn is_paused(&self) -> bool {
        self.last_status.as_ref().is_some_and(|s| s.paused)
    }

    fn external_watcher_active(&self) -> bool {
        self.watch_child.is_none() && self.last_status.as_ref().is_some_and(|s| s.watching)
    }

    fn has_managed_service(&mut self) -> bool {
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

    fn start_watch(&mut self) {
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

    fn check_watch_alive(&mut self) {
        if self.respawn_disabled || self.is_paused() {
            return;
        }

        if let Some(ref mut child) = self.watch_child {
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

    fn stop_watch(&mut self) {
        if let Some(mut child) = self.watch_child.take() {
            graceful_stop_child(&mut child);
            self.owns_watch = false;
        }
    }

    fn cached_recent(&mut self) {
        let stale = self
            .recent_fetched_at
            .map(|t| t.elapsed().as_secs() >= RECENT_CACHE_SECS)
            .unwrap_or(true);
        if stale {
            self.recent = tray_recent();
            self.recent_fetched_at = Some(Instant::now());
        }
    }

    fn invalidate_recent(&mut self) {
        self.recent = None;
        self.recent_fetched_at = None;
    }

    fn reset_watch_policy(&mut self) {
        self.watch_failures = 0;
        self.respawn_disabled = false;
        self.status_failed = false;
        self.error_message = None;
        self.managed_service = None;
    }

    fn adopt_recent_if_unconfigured(&mut self) -> bool {
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

    fn cancel_pairing(&mut self) {
        if let Some(cancel) = self.pair_cancel.take() {
            let _ = cancel.send(());
        }
    }
}

fn configured_recent_workspace(recent: &RecentWorkspacesResult) -> Option<PathBuf> {
    recent
        .active
        .iter()
        .chain(recent.workspaces.iter().map(|workspace| &workspace.path))
        .map(PathBuf::from)
        .find(|path| workspace_has_config(path))
}

fn unavailable_workspace_count(recent: &RecentWorkspacesResult) -> usize {
    recent
        .workspaces
        .iter()
        .filter(|workspace| !workspace_has_config(Path::new(&workspace.path)))
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MirroredFolderMenuItem {
    id: String,
    label: String,
    available: bool,
    selected: bool,
}

fn canonical_path_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn same_workspace_path(left: &str, right: &str) -> bool {
    canonical_path_string(Path::new(left)) == canonical_path_string(Path::new(right))
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

fn workspace_switch_item(label: &str, path: &str, active: Option<&str>) -> MirroredFolderMenuItem {
    let available = workspace_has_config(Path::new(path));
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

fn mirrored_folder_menu_items(state: &AppState) -> Vec<MirroredFolderMenuItem> {
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
        let path = canonical_path_string(workspace);
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

    let active = state
        .workspace
        .as_deref()
        .map(canonical_path_string)
        .or_else(|| {
            state
                .recent
                .as_ref()
                .and_then(|recent| recent.active.clone())
        });
    workspaces
        .iter()
        .map(|(path, label)| workspace_switch_item(label, path, active.as_deref()))
        .collect()
}

fn append_mirrored_folders(menu: &Menu, state: &AppState, actions_enabled: bool) {
    let entries = mirrored_folder_menu_items(state);
    if entries.is_empty() {
        return;
    }
    let folders = Submenu::with_id(
        muda::MenuId::new("mirrored-folders"),
        "Mirrored Folders",
        true,
    );
    for entry in entries {
        let _ = folders.append(&CheckMenuItem::with_id(
            muda::MenuId::new(entry.id),
            entry.label,
            actions_enabled && entry.available,
            entry.selected,
            None,
        ));
    }
    if state
        .recent
        .as_ref()
        .is_some_and(|recent| unavailable_workspace_count(recent) > 0)
    {
        let _ = folders.append(&PredefinedMenuItem::separator());
        let _ = folders.append(&MenuItem::with_id(
            muda::MenuId::new("forget-unavailable"),
            "Remove Unavailable Folders…",
            actions_enabled,
            None,
        ));
    }
    let _ = menu.append(&folders);
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

fn is_paused_on_disk(workspace: &Path) -> bool {
    workspace.join(".feanorfs/paused").is_file()
}

fn resolve_initial_workspace() -> Option<PathBuf> {
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

fn first_run_requested(args: &[OsString]) -> bool {
    args.iter()
        .any(|argument| argument == OsStr::new("--first-run"))
}

fn should_prompt_first_run(requested: bool, workspace: Option<&Path>) -> bool {
    requested && workspace.is_none()
}

const FIRST_RUN_START: &str = "Start Mirroring a Folder…";
const FIRST_RUN_JOIN: &str = "Join a Shared Folder…";
const FIRST_RUN_LATER: &str = "Not Now";
const HEALTH_REPAIR: &str = "Repair Mirroring";
const HEALTH_CLOSE: &str = "Close";
const UPDATE_OPEN: &str = "Open Release Page";
const UPDATE_LATER: &str = "Later";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRunChoice {
    Start,
    Join,
    Later,
}

fn first_run_choice(result: rfd::MessageDialogResult) -> FirstRunChoice {
    match result {
        rfd::MessageDialogResult::Custom(choice) if choice == FIRST_RUN_START => {
            FirstRunChoice::Start
        }
        rfd::MessageDialogResult::Custom(choice) if choice == FIRST_RUN_JOIN => {
            FirstRunChoice::Join
        }
        _ => FirstRunChoice::Later,
    }
}

fn show_first_run_choice() -> FirstRunChoice {
    activate_for_native_dialog();
    first_run_choice(
        rfd::MessageDialog::new()
            .set_title("Welcome to FeanorFS")
            .set_description(
                "Add a folder from this computer, or securely join one shared from another computer. FeanorFS will keep it synced automatically.",
            )
            .set_level(rfd::MessageLevel::Info)
            .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
                FIRST_RUN_START.into(),
                FIRST_RUN_JOIN.into(),
                FIRST_RUN_LATER.into(),
            ))
            .show(),
    )
}

fn health_check_label(name: &str) -> &str {
    match name {
        "global_config" => "Saved connection",
        "workspace_config" => "Workspace setup",
        "e2ee" => "End-to-end encryption",
        "workspace_format" => "Encrypted snapshot format",
        "automatic_sync" => "Automatic syncing",
        "tray_registration" => "System tray startup",
        "private_hub" => "Private hub",
        "relay" => "Off-LAN connection",
        "server" => "Mirror connection",
        "remote_workspace" => "Remote workspace",
        "local_state" => "Local sync state",
        _ => "FeanorFS component",
    }
}

fn health_report_needs_repair(report: &HealthReport) -> bool {
    !report.ok
        || report
            .checks
            .iter()
            .any(|check| check.status == HealthStatus::Failure)
}

fn health_choice_requests_repair(choice: &rfd::MessageDialogResult) -> bool {
    matches!(
        choice,
        rfd::MessageDialogResult::Custom(value) if value == HEALTH_REPAIR
    )
}

fn health_report_description(report: &HealthReport) -> String {
    let failures = report
        .checks
        .iter()
        .filter(|check| check.status == HealthStatus::Failure)
        .map(|check| health_check_label(&check.name))
        .collect::<Vec<_>>();
    let warnings = report
        .checks
        .iter()
        .filter(|check| check.status == HealthStatus::Warning)
        .map(|check| health_check_label(&check.name))
        .collect::<Vec<_>>();
    if failures.is_empty() && warnings.is_empty() && report.ok {
        return "FeanorFS is healthy. Encryption, the mirror connection, background syncing, and local state passed their checks."
            .into();
    }

    let mut description = if failures.is_empty() && !report.ok {
        "FeanorFS could not confirm all required checks. The health check did not change your files."
            .to_string()
    } else if failures.is_empty() {
        "FeanorFS is working, with items worth checking.".to_string()
    } else {
        format!(
            "FeanorFS found {} issue{}. The health check did not change your files.",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" }
        )
    };
    if !failures.is_empty() {
        description.push_str("\n\nNeeds repair:");
        for label in failures {
            description.push_str("\n• ");
            description.push_str(label);
        }
    }
    if !warnings.is_empty() {
        description.push_str("\n\nCheck when convenient:");
        for label in warnings {
            description.push_str("\n• ");
            description.push_str(label);
        }
    }
    description
}

fn update_description(result: &UpdateCheckResult) -> String {
    match result.status {
        UpdateStatus::UpToDate => format!(
            "FeanorFS {} is up to date with the latest stable release.",
            result.current_version
        ),
        UpdateStatus::UpdateAvailable => format!(
            "FeanorFS {} is available. This computer has {}.\n\nFeanorFS will not download or execute anything automatically. Open the official release page to review the signed or checksummed installer for your platform.",
            result.latest_version, result.current_version
        ),
        UpdateStatus::DevelopmentBuild => format!(
            "This FeanorFS build ({}) is newer than the latest stable release ({}). No update is needed.",
            result.current_version, result.latest_version
        ),
    }
}

fn update_choice_opens_release(choice: &rfd::MessageDialogResult) -> bool {
    matches!(
        choice,
        rfd::MessageDialogResult::Custom(value) if value == UPDATE_OPEN
    )
}

#[cfg(target_os = "macos")]
fn activate_for_native_dialog() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    if let Some(main_thread) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(main_thread);
        // First-run onboarding is explicitly user-initiated by the installer.
        // Cooperative activation may decline while Terminal or Finder is active.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

#[cfg(not(target_os = "macos"))]
fn activate_for_native_dialog() {}

fn header_label(status: &TrayStatusResult) -> String {
    if status.paused {
        return format!("FeanorFS — {} (paused)", status.workspace_label);
    }
    let state = match status.mirror_state.as_str() {
        "idle" => "up to date",
        "out_of_sync" => "has changes",
        "offline" => "offline",
        "conflict" => "needs attention",
        "syncing" => "syncing",
        "error" => "error",
        other => other,
    };
    format!("FeanorFS — {} ({state})", status.workspace_label)
}

fn choice_label(choice: &str) -> String {
    match choice {
        "local" => "Keep my version".into(),
        "cloud" => "Keep cloud version".into(),
        "both" => "Keep both".into(),
        other => other.into(),
    }
}

fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} seconds")
    } else {
        let minutes = seconds / 60;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    }
}

fn pairing_dialog_description(code: &str, expires_in_seconds: u64) -> String {
    let expiry = format_duration(expires_in_seconds);
    if code.starts_with("fnp2-") {
        return format!(
            "A secure one-time sharing code was copied to your clipboard.\n\n\
             On the other computer, open FeanorFS, choose Join a Shared Folder…, and paste it.\n\n\
             The code expires in {expiry} and works once. Keep this window open while the other computer connects."
        );
    }
    format!(
        "On the other computer, open FeanorFS, choose Join a Shared Folder…, and paste this one-time code:\n\n{code}\n\n\
         The code was copied to your clipboard and expires in {expiry}. \
         Keep this window open while the other computer connects."
    )
}

fn prompt_recovery_passphrase() -> Option<zeroize::Zeroizing<String>> {
    native_password_input("FeanorFS recovery", "Recovery kit passphrase")
}

fn prompt_new_recovery_passphrase() -> Option<zeroize::Zeroizing<String>> {
    let passphrase = native_password_input(
        "Protect FeanorFS recovery kit",
        "New recovery passphrase (12+ characters)",
    )?;
    let confirmation = native_password_input(
        "Protect FeanorFS recovery kit",
        "Confirm recovery passphrase",
    )?;
    if passphrase.as_str() != confirmation.as_str() {
        let _ = rfd::MessageDialog::new()
            .set_title("Passphrases do not match")
            .set_description(
                "The recovery kit was not created. Try again with matching passphrases.",
            )
            .set_level(rfd::MessageLevel::Error)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
        return None;
    }
    Some(passphrase)
}

fn native_password_input(title: &str, message: &str) -> Option<zeroize::Zeroizing<String>> {
    match password_dialog::prompt(title, message) {
        Ok(passphrase) => passphrase,
        Err(error) => {
            let _ = rfd::MessageDialog::new()
                .set_title("Could not open secure password dialog")
                .set_description(error)
                .set_level(rfd::MessageLevel::Error)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            None
        }
    }
}

fn menu_actions_enabled(state: &AppState) -> bool {
    !state.setup_inflight
        && !state.stop_inflight
        && !state.switch_inflight
        && !state.pair_inflight
        && !state.recovery_inflight
}

fn unmanaged_terminal_watcher_active(state: &AppState, status: &TrayStatusResult) -> bool {
    status.watching && !state.owns_watch && state.managed_service == Some(false)
}

fn append_other_computers(menu: &Menu, state: &AppState, actions_enabled: bool) {
    let computers = Submenu::with_id(
        muda::MenuId::new("other-computers"),
        "Other Computers",
        true,
    );
    let _ = computers.append(&MenuItem::with_id(
        muda::MenuId::new("pair"),
        if state.pair_inflight {
            "Preparing Secure Share…"
        } else {
            "Share Selected Folder…"
        },
        actions_enabled && state.workspace.is_some(),
        None,
    ));
    let _ = computers.append(&MenuItem::with_id(
        muda::MenuId::new("join-computer"),
        "Join a Shared Folder…",
        actions_enabled,
        None,
    ));
    let _ = menu.append(&computers);
}

fn append_recovery_menu(menu: &Menu, state: &AppState, actions_enabled: bool) {
    let recovery = Submenu::with_id(
        muda::MenuId::new("recovery"),
        if state.recovery_inflight {
            "Recovery in progress…"
        } else {
            "Recovery"
        },
        true,
    );
    let _ = recovery.append(&MenuItem::with_id(
        muda::MenuId::new("recovery-export"),
        "Export Encrypted Recovery Kit…",
        actions_enabled && state.workspace.is_some(),
        None,
    ));
    let _ = recovery.append(&MenuItem::with_id(
        muda::MenuId::new("recovery-import"),
        "Restore From Recovery Kit…",
        actions_enabled,
        None,
    ));
    let _ = menu.append(&recovery);
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("the selected folder")
        .to_string()
}

fn setup_success_copy(kind: SetupKind, path: &Path) -> (&'static str, String) {
    let name = folder_name(path);
    match kind {
        SetupKind::AddFolder => (
            "Folder ready",
            format!(
                "FeanorFS is now mirroring “{name}”. It will sync automatically, including after you log in again."
            ),
        ),
        SetupKind::JoinFolder => (
            "Shared folder ready",
            format!(
                "“{name}” is connected securely and will sync automatically, including after you log in again."
            ),
        ),
        SetupKind::Repair => (
            "Mirroring repaired",
            format!("FeanorFS repaired automatic syncing for “{name}”."),
        ),
    }
}

fn setup_failure_copy(
    kind: SetupKind,
    path: &Path,
    configured: bool,
    error: &str,
) -> (&'static str, String) {
    let name = folder_name(path);
    let (title, outcome) = match kind {
        SetupKind::AddFolder => ("Folder wasn’t added", format!("“{name}” was not added.")),
        SetupKind::JoinFolder => (
            "Shared folder wasn’t joined",
            format!("“{name}” was not connected."),
        ),
        SetupKind::Repair => (
            "Mirroring wasn’t repaired",
            format!("Automatic syncing for “{name}” was not repaired."),
        ),
    };
    let cause = if configured && kind == SetupKind::AddFolder {
        "This folder already has FeanorFS setup, but its saved mirror could not be reached."
    } else {
        "FeanorFS could not finish the secure connection and initial sync."
    };
    let next_step = if configured && kind == SetupKind::AddFolder {
        "Make sure the computer or service that hosts its existing mirror is available, then choose Add Folder again."
    } else {
        "Check the connection and try again. If it keeps failing, reopen FeanorFS and retry."
    };
    let detail = error.trim().strip_prefix("Error:").unwrap_or(error.trim());
    (
        title,
        format!(
            "{outcome} {cause}\n\nYour files and encrypted setup were not changed. {next_step}\n\nDetails: {detail}"
        ),
    )
}

fn show_setup_result_dialog(title: &str, description: String, success: bool) {
    activate_for_native_dialog();
    let _ = rfd::MessageDialog::new()
        .set_title(title)
        .set_description(description)
        .set_level(if success {
            rfd::MessageLevel::Info
        } else {
            rfd::MessageLevel::Error
        })
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

fn activity_header(state: &AppState) -> Option<&'static str> {
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

fn build_menu(state: &AppState) -> Menu {
    let menu = Menu::new();
    let status = state.last_status.as_ref();
    let actions_enabled = menu_actions_enabled(state);

    if state.health_inflight || state.update_inflight {
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("header"),
            if state.health_inflight {
                "FeanorFS — checking system health…"
            } else {
                "FeanorFS — checking for updates…"
            },
            false,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        append_mirrored_folders(&menu, state, false);
        if state.workspace.is_some() {
            let _ = menu.append(&MenuItem::with_id(
                muda::MenuId::new("open"),
                "Open Selected Folder",
                true,
                None,
            ));
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("quit"),
            "Quit FeanorFS Tray",
            true,
            None,
        ));
        return menu;
    }

    if let Some(s) = status {
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("header"),
            activity_header(state)
                .map(str::to_string)
                .unwrap_or_else(|| header_label(s)),
            false,
            None,
        ));
        if unmanaged_terminal_watcher_active(state, s) {
            let _ = menu.append(&MenuItem::with_id(
                muda::MenuId::new("external-watch"),
                "Syncing in another terminal",
                false,
                None,
            ));
        }
        if let Some(ref msg) = state.error_message {
            let _ = menu.append(&MenuItem::with_id(
                muda::MenuId::new("error"),
                msg,
                false,
                None,
            ));
        }
        let _ = menu.append(&PredefinedMenuItem::separator());

        append_mirrored_folders(&menu, state, actions_enabled);
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("open"),
            "Open Selected Folder",
            true,
            None,
        ));

        let add_label = if state.setup_inflight {
            "Adding Folder…"
        } else {
            "Add Folder…"
        };
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("add-folder"),
            add_label,
            actions_enabled,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());

        let pause_label = if s.paused {
            "Resume Syncing"
        } else {
            "Pause Syncing"
        };
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("pause"),
            pause_label,
            actions_enabled,
            None,
        ));

        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("sync-now"),
            "Sync Now",
            actions_enabled,
            None,
        ));

        if !s.pending_conflicts.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
            let title = format!("Needs attention ({})", s.pending_conflicts.len());
            let conflict_menu = Submenu::with_id(muda::MenuId::new("conflicts"), title, true);
            for c in &s.pending_conflicts {
                let _ = conflict_menu.append(&MenuItem::with_id(
                    muda::MenuId::new(format!("conflict-hdr:{}", c.path)),
                    format!("{} — {}", c.path, c.label),
                    false,
                    None,
                ));
                for choice in &c.choices {
                    let _ = conflict_menu.append(&MenuItem::with_id(
                        muda::MenuId::new(format!("keep-{choice}:{}", c.path)),
                        format!("  {}", choice_label(choice)),
                        actions_enabled,
                        None,
                    ));
                }
                let _ = conflict_menu.append(&PredefinedMenuItem::separator());
            }
            let _ = menu.append(&conflict_menu);
        }

        if !s.agents.entries.is_empty() {
            if s.pending_conflicts.is_empty() {
                let _ = menu.append(&PredefinedMenuItem::separator());
            }
            let title = if s.agents.working > 0 {
                format!(
                    "Agents — {} working · {} need attention",
                    s.agents.working, s.agents.need_attention
                )
            } else {
                "Agents".into()
            };
            let agent_menu = Submenu::with_id(muda::MenuId::new("agents"), title, true);
            for a in &s.agents.entries {
                let label = match a.state.as_str() {
                    "changes" => format!("{} — {} change(s)", a.name, a.change_count),
                    "conflicts" => format!("{} — {} conflict(s)", a.name, a.conflict_count),
                    "offline" => format!("{} — offline", a.name),
                    _ => format!("{} — clean", a.name),
                };
                if a.state == "changes" || a.state == "conflicts" {
                    let _ = agent_menu.append(&MenuItem::with_id(
                        muda::MenuId::new(format!("land:{}", a.name)),
                        format!("  Land {label}"),
                        actions_enabled,
                        None,
                    ));
                } else {
                    let _ = agent_menu.append(&MenuItem::with_id(
                        muda::MenuId::new(format!("agent-hdr:{}", a.name)),
                        &label,
                        false,
                        None,
                    ));
                }
            }
            let _ = menu.append(&agent_menu);
        }

        let _ = menu.append(&PredefinedMenuItem::separator());
        append_other_computers(&menu, state, actions_enabled);
        append_recovery_menu(&menu, state, actions_enabled);
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("stop-mirroring"),
            if state.stop_inflight {
                "Stopping Mirroring…"
            } else {
                "Stop Mirroring This Folder…"
            },
            actions_enabled,
            None,
        ));
    } else {
        let header = activity_header(state).unwrap_or(if state.workspace.is_some() {
            "FeanorFS — checking folder…"
        } else {
            "FeanorFS — no folders yet"
        });
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("header"),
            header,
            false,
            None,
        ));
        if let Some(ref msg) = state.error_message {
            let _ = menu.append(&MenuItem::with_id(
                muda::MenuId::new("error"),
                msg,
                false,
                None,
            ));
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        append_mirrored_folders(&menu, state, actions_enabled);
        if state.workspace.is_some() {
            let _ = menu.append(&MenuItem::with_id(
                muda::MenuId::new("open"),
                "Open Selected Folder",
                true,
                None,
            ));
        }
        let add_label = if state.setup_inflight {
            "Adding Folder…"
        } else {
            "Add Folder…"
        };
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("add-folder"),
            add_label,
            actions_enabled,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        append_other_computers(&menu, state, actions_enabled);
        append_recovery_menu(&menu, state, actions_enabled);
        if state.workspace.is_some() {
            let stop_label = if state.stop_inflight {
                "Stopping Mirroring…"
            } else {
                "Stop Mirroring This Folder…"
            };
            let _ = menu.append(&MenuItem::with_id(
                muda::MenuId::new("stop-mirroring"),
                stop_label,
                actions_enabled,
                None,
            ));
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    if state.workspace.is_some() {
        let label = if state.health_inflight {
            "Checking System Health…"
        } else {
            "Check System Health…"
        };
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("health"),
            label,
            actions_enabled && !state.health_inflight,
            None,
        ));
    }

    let update_label = if state.update_inflight {
        "Checking for Updates…"
    } else {
        "Check for Updates…"
    };
    let _ = menu.append(&MenuItem::with_id(
        muda::MenuId::new("update"),
        update_label,
        actions_enabled && !state.update_inflight,
        None,
    ));

    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(
        muda::MenuId::new("quit"),
        "Quit FeanorFS Tray",
        !state.setup_inflight
            && !state.stop_inflight
            && !state.switch_inflight
            && !state.recovery_inflight,
        None,
    ));
    menu
}

#[derive(Debug, Clone)]
enum MenuAction {
    AddFolder,
    JoinComputer,
    StopMirroring,
    OpenFolder,
    Pair,
    ExportRecovery,
    ImportRecovery,
    TogglePause,
    SyncNow,
    Keep { path: String, choice: String },
    Land { agent: String },
    SwitchWorkspace(PathBuf),
    ForgetUnavailable,
    CheckHealth,
    CheckUpdates,
    Quit,
}

fn parse_menu_action(id: &str) -> Option<MenuAction> {
    if id == "add-folder" {
        return Some(MenuAction::AddFolder);
    }
    if id == "join-computer" {
        return Some(MenuAction::JoinComputer);
    }
    if id == "stop-mirroring" {
        return Some(MenuAction::StopMirroring);
    }
    if id == "open" {
        return Some(MenuAction::OpenFolder);
    }
    if id == "pair" {
        return Some(MenuAction::Pair);
    }
    if id == "recovery-export" {
        return Some(MenuAction::ExportRecovery);
    }
    if id == "recovery-import" {
        return Some(MenuAction::ImportRecovery);
    }
    if id == "pause" {
        return Some(MenuAction::TogglePause);
    }
    if id == "sync-now" {
        return Some(MenuAction::SyncNow);
    }
    if id == "forget-unavailable" {
        return Some(MenuAction::ForgetUnavailable);
    }
    if id == "health" {
        return Some(MenuAction::CheckHealth);
    }
    if id == "update" {
        return Some(MenuAction::CheckUpdates);
    }
    if id == "quit" {
        return Some(MenuAction::Quit);
    }
    if let Some(rest) = id.strip_prefix("keep-") {
        if let Some((choice, path)) = rest.split_once(':') {
            return Some(MenuAction::Keep {
                path: path.into(),
                choice: choice.into(),
            });
        }
    }
    if let Some(agent) = id.strip_prefix("land:") {
        return Some(MenuAction::Land {
            agent: agent.into(),
        });
    }
    if let Some(path) = id.strip_prefix("switch:") {
        return Some(MenuAction::SwitchWorkspace(PathBuf::from(path)));
    }
    None
}

fn action_allowed_while_background_check_runs(action: &MenuAction) -> bool {
    matches!(action, MenuAction::OpenFolder | MenuAction::Quit)
}

fn menu_revision(state: &AppState) -> u64 {
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

fn apply_ui(state: &AppState, tray: &TrayIcon, visual: &mut TrayVisual) {
    let v = if state.setup_inflight || state.switch_inflight {
        TrayVisual::Syncing
    } else if state.workspace.is_none() {
        TrayVisual::Idle
    } else if state.last_status.is_none() || state.status_failed {
        TrayVisual::Error
    } else {
        match &state.last_status {
            Some(s) => visual_from_state(&s.mirror_state, s.paused),
            None => TrayVisual::Error,
        }
    };
    if v != *visual {
        let _ = tray.set_icon(Some(icon_for(v)));
        *visual = v;
    }
    let revision = menu_revision(state);
    if state.last_menu_revision.get() == Some(revision) {
        return;
    }
    let menu = build_menu(state);
    tray.set_menu(Some(Box::new(menu)));
    state.last_menu_revision.set(Some(revision));
}

fn request_status_fetch(state: &mut AppState, proxy: &tao::event_loop::EventLoopProxy<Action>) {
    if state.setup_inflight || state.stop_inflight || state.recovery_inflight {
        return;
    }
    if state.status_inflight {
        state.status_pending = true;
        return;
    }
    state.status_inflight = true;
    state.status_pending = false;
    let generation = state.task_generation;
    let Some(workspace) = state.workspace.clone() else {
        state.status_inflight = false;
        return;
    };
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let status = tray_status(&workspace);
        let _ = proxy.send_event(Action::StatusReady {
            generation,
            workspace,
            status,
        });
    });
}

fn run_exclusive_service_action(
    workspace: &Path,
    external_watcher: bool,
    action: impl FnOnce() -> Result<(), String>,
) -> Option<String> {
    let managed_service = external_watcher && background_service_managed(workspace);
    if external_watcher && !managed_service {
        return Some(
            "Sync is running in a terminal. Stop it before using this tray action.".into(),
        );
    }
    if managed_service {
        if let Err(error) = background_service_stop(workspace) {
            return Some(error);
        }
    }
    let action_error = action().err();
    let restart_error = managed_service
        .then(|| background_service_start(workspace).err())
        .flatten();
    action_error.or(restart_error)
}

fn begin_workspace_repair(
    state: &mut AppState,
    workspace: PathBuf,
    proxy: &tao::event_loop::EventLoopProxy<Action>,
) {
    state.task_generation = state.task_generation.saturating_add(1);
    let generation = state.task_generation;
    state.setup_inflight = true;
    state.setup_kind = Some(SetupKind::Repair);
    state.error_message = Some("Repairing encrypted mirroring…".into());
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let error = start_workspace(&workspace).err();
        let _ = proxy.send_event(Action::SetupDone {
            generation,
            path: workspace,
            kind: SetupKind::Repair,
            error,
        });
    });
}

fn handle_menu_action(
    state: &mut AppState,
    action: MenuAction,
    proxy: &tao::event_loop::EventLoopProxy<Action>,
) {
    if (state.setup_inflight || state.switch_inflight) && !matches!(&action, MenuAction::OpenFolder)
    {
        return;
    }
    if (state.health_inflight || state.update_inflight)
        && !action_allowed_while_background_check_runs(&action)
    {
        return;
    }
    if state.stop_inflight && !matches!(&action, MenuAction::OpenFolder) {
        return;
    }
    if state.pair_inflight && !matches!(&action, MenuAction::OpenFolder | MenuAction::Quit) {
        return;
    }
    if state.recovery_inflight && !matches!(&action, MenuAction::OpenFolder) {
        return;
    }
    if matches!(
        &action,
        MenuAction::ExportRecovery | MenuAction::ImportRecovery
    ) && (state.setup_inflight
        || state.stop_inflight
        || state.switch_inflight
        || state.pair_inflight)
    {
        return;
    }
    match action {
        MenuAction::AddFolder => {
            if state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
            {
                return;
            }
            activate_for_native_dialog();
            let mut dialog = rfd::FileDialog::new().set_title("Choose a folder to mirror");
            if let Some(directory) = state.workspace.as_deref().and_then(Path::parent) {
                dialog = dialog.set_directory(directory);
            }
            let Some(path) = dialog.pick_folder() else {
                return;
            };
            state.task_generation = state.task_generation.saturating_add(1);
            let generation = state.task_generation;
            state.setup_inflight = true;
            state.setup_kind = Some(SetupKind::AddFolder);
            state.error_message = Some("Setting up encrypted mirroring…".into());
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = start_workspace(&path).err();
                let _ = proxy.send_event(Action::SetupDone {
                    generation,
                    path,
                    kind: SetupKind::AddFolder,
                    error,
                });
            });
        }
        MenuAction::JoinComputer => {
            if state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
                || state.recovery_inflight
            {
                return;
            }
            let Some(pairing_code) = native_password_input(
                "Join a shared folder",
                "Paste the one-time code from the other computer",
            ) else {
                return;
            };
            let mut dialog =
                rfd::FileDialog::new().set_title("Choose where to keep the shared folder");
            if let Some(directory) = state.workspace.as_deref().and_then(Path::parent) {
                dialog = dialog.set_directory(directory);
            }
            let Some(path) = dialog.pick_folder() else {
                return;
            };
            state.task_generation = state.task_generation.saturating_add(1);
            let generation = state.task_generation;
            state.setup_inflight = true;
            state.setup_kind = Some(SetupKind::JoinFolder);
            state.error_message = Some("Connecting shared folder securely…".into());
            let proxy = proxy.clone();
            std::thread::spawn(move || match join_workspace(&path, pairing_code) {
                Err(error) if error == "__FEANORFS_JOIN_CANCELED__" => {
                    let _ = proxy.send_event(Action::SetupCanceled { generation });
                }
                result => {
                    let _ = proxy.send_event(Action::SetupDone {
                        generation,
                        path,
                        kind: SetupKind::JoinFolder,
                        error: result.err(),
                    });
                }
            });
        }
        MenuAction::StopMirroring => {
            if state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
            {
                return;
            }
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            let confirmed = rfd::MessageDialog::new()
                .set_title("Stop mirroring this folder?")
                .set_description(
                    "Automatic sync will stop and this folder will be removed from the FeanorFS tray.\n\nYour files and encrypted setup will be kept, so you can start mirroring it again later.",
                )
                .set_level(rfd::MessageLevel::Warning)
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if !matches!(confirmed, rfd::MessageDialogResult::Ok) {
                return;
            }
            state.stop_watch();
            state.task_generation = state.task_generation.saturating_add(1);
            let generation = state.task_generation;
            state.stop_inflight = true;
            state.error_message = Some("Stopping automatic mirroring…".into());
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = stop_workspace(&workspace).err();
                let _ = proxy.send_event(Action::StopDone {
                    generation,
                    path: workspace,
                    error,
                });
            });
        }
        MenuAction::OpenFolder => {
            if let Some(workspace) = state.workspace.as_ref() {
                let _ = open::that(workspace);
            }
        }
        MenuAction::Pair => {
            if state.pair_inflight
                || state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
            {
                return;
            }
            let Some(workspace) = state.workspace.clone() else {
                state.error_message = Some("Select a folder before sharing it.".into());
                return;
            };
            let generation = state.task_generation;
            let (cancel, cancel_rx) = std::sync::mpsc::channel();
            state.pair_inflight = true;
            state.pair_cancel = Some(cancel);
            state.error_message = Some("Preparing a secure one-time sharing code…".into());
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                run_pairing_session(&workspace, cancel_rx, |event| match event {
                    PairSessionEvent::Ready(ready) => {
                        let _ = proxy.send_event(Action::PairReady {
                            generation,
                            code: ready.code,
                            expires_in_seconds: ready.expires_in_seconds,
                        });
                    }
                    PairSessionEvent::Done {
                        paired,
                        canceled,
                        error,
                    } => {
                        let _ = proxy.send_event(Action::PairDone {
                            generation,
                            paired,
                            canceled,
                            error,
                        });
                    }
                });
            });
        }
        MenuAction::ExportRecovery => {
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            let Some(destination) = rfd::FileDialog::new()
                .set_title("Save encrypted FeanorFS recovery kit")
                .set_file_name("FeanorFS-recovery.fnrk")
                .add_filter("FeanorFS recovery kit", &["fnrk"])
                .save_file()
            else {
                return;
            };
            let Some(passphrase) = prompt_new_recovery_passphrase() else {
                return;
            };
            state.task_generation = state.task_generation.saturating_add(1);
            let generation = state.task_generation;
            state.recovery_inflight = true;
            state.error_message = Some("Encrypting recovery kit…".into());
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = export_recovery_kit(&workspace, &destination, passphrase).err();
                let _ = proxy.send_event(Action::RecoveryDone {
                    generation,
                    restored_folder: None,
                    error,
                });
            });
        }
        MenuAction::ImportRecovery => {
            let Some(source) = rfd::FileDialog::new()
                .set_title("Choose an encrypted FeanorFS recovery kit")
                .add_filter("FeanorFS recovery kit", &["fnrk"])
                .pick_file()
            else {
                return;
            };
            let mut dialog =
                rfd::FileDialog::new().set_title("Choose a folder for the restored workspace");
            if let Some(parent) = state.workspace.as_deref().and_then(Path::parent) {
                dialog = dialog.set_directory(parent);
            }
            let Some(destination) = dialog.pick_folder() else {
                return;
            };
            let Some(passphrase) = prompt_recovery_passphrase() else {
                return;
            };
            state.task_generation = state.task_generation.saturating_add(1);
            let generation = state.task_generation;
            state.recovery_inflight = true;
            state.error_message = Some("Authenticating recovery kit…".into());
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = import_recovery_kit(&source, &destination, passphrase).err();
                let _ = proxy.send_event(Action::RecoveryDone {
                    generation,
                    restored_folder: Some(destination),
                    error,
                });
            });
        }
        MenuAction::CheckHealth => {
            if state.health_inflight
                || state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
                || state.recovery_inflight
            {
                return;
            }
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            state.health_inflight = true;
            state.error_message = Some("Checking system health…".into());
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let report = system_health(&workspace);
                let _ = proxy.send_event(Action::HealthReady { workspace, report });
            });
        }
        MenuAction::CheckUpdates => {
            if state.update_inflight
                || state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
                || state.recovery_inflight
            {
                return;
            }
            state.update_inflight = true;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let _ = proxy.send_event(Action::UpdateReady(check_for_updates()));
            });
        }
        MenuAction::Quit => {
            if state.pair_inflight {
                state.quit_pending = true;
                state.error_message = Some("Closing secure pairing…".into());
                state.cancel_pairing();
                return;
            }
            state.stop_watch();
            std::process::exit(0);
        }
        MenuAction::TogglePause => {
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            let pause = !state.is_paused();
            if pause {
                state.stop_watch();
            }
            let generation = state.task_generation;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = tray_pause(&workspace, pause).err();
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: !pause,
                    set_paused: Some(pause),
                    generation,
                });
            });
        }
        MenuAction::SyncNow => {
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            let external_watcher = state.external_watcher_active();
            state.stop_watch();
            let generation = state.task_generation;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = run_exclusive_service_action(&workspace, external_watcher, || {
                    sync_once(&workspace)
                });
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: !external_watcher,
                    set_paused: None,
                    generation,
                });
            });
        }
        MenuAction::ForgetUnavailable => {
            if state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
                || state.recovery_inflight
            {
                return;
            }
            let before = state
                .recent
                .as_ref()
                .map(unavailable_workspace_count)
                .unwrap_or(0);
            if before == 0 {
                return;
            }
            let noun = if before == 1 { "folder" } else { "folders" };
            let confirmed = rfd::MessageDialog::new()
                .set_title("Remove unavailable folders from this list?")
                .set_description(format!(
                    "{before} {noun} cannot be opened right now. This can happen when a folder was moved or deleted, or when an external drive is disconnected.\n\nFeanorFS will remove only these entries from the tray. It will not delete files, encrypted setup, credentials, services, hub data, or remote snapshots. Reconnect external drives and cancel if you want to keep them listed."
                ))
                .set_level(rfd::MessageLevel::Warning)
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if !matches!(confirmed, rfd::MessageDialogResult::Ok) {
                return;
            }
            state.task_generation = state.task_generation.saturating_add(1);
            let generation = state.task_generation;
            state.switch_inflight = true;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let result = forget_unavailable_workspaces();
                let _ = proxy.send_event(Action::ForgetUnavailableDone {
                    generation,
                    before,
                    result,
                });
            });
        }
        MenuAction::Keep { path, choice } => {
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            let external_watcher = state.external_watcher_active();
            state.stop_watch();
            let generation = state.task_generation;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = run_exclusive_service_action(&workspace, external_watcher, || {
                    conflicts_keep(&workspace, &path, &choice).and_then(|()| sync_once(&workspace))
                });
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: !external_watcher,
                    set_paused: None,
                    generation,
                });
            });
        }
        MenuAction::Land { agent } => {
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            let external_watcher = state.external_watcher_active();
            state.stop_watch();
            let generation = state.task_generation;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = run_exclusive_service_action(&workspace, external_watcher, || {
                    agent_land(&workspace, &agent).and_then(|()| sync_once(&workspace))
                });
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: !external_watcher,
                    set_paused: None,
                    generation,
                });
            });
        }
        MenuAction::SwitchWorkspace(path) => {
            if state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
            {
                return;
            }
            if !workspace_has_config(&path) {
                state.error_message = Some(format!(
                    "This folder is no longer available to FeanorFS: {}",
                    path.display()
                ));
                return;
            }
            state.task_generation = state.task_generation.saturating_add(1);
            let generation = state.task_generation;
            state.switch_inflight = true;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = tray_activate(&path).err();
                let _ = proxy.send_event(Action::SwitchDone {
                    generation,
                    path,
                    error,
                });
            });
        }
    }
}

fn main() {
    let workspace = resolve_initial_workspace();
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let prompt_first_run =
        should_prompt_first_run(first_run_requested(&arguments), workspace.as_deref());

    #[cfg(target_os = "macos")]
    let event_loop = {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        let mut el = EventLoopBuilder::<Action>::with_user_event().build();
        el.set_activation_policy(ActivationPolicy::Accessory);
        el
    };
    #[cfg(not(target_os = "macos"))]
    let event_loop = EventLoopBuilder::<Action>::with_user_event().build();

    let proxy = event_loop.create_proxy();

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: muda::MenuEvent| {
        let _ = menu_proxy.send_event(Action::MenuClick(event.id().0.clone()));
    }));

    let mut state = AppState::new(workspace);
    state.cached_recent();

    let initial_visual = TrayVisual::Idle;
    let tray = TrayIconBuilder::new()
        .with_tooltip("FeanorFS")
        .with_icon(icon_for(initial_visual))
        .with_menu(Box::new(build_menu(&state)))
        .build()
        .expect("tray icon");

    let tray = Rc::new(tray);

    let refresh_proxy = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(REFRESH_SECS));
        let _ = refresh_proxy.send_event(Action::Refresh);
    });

    let shared = Rc::new(Mutex::new(state));
    let mut visual = initial_visual;

    {
        let mut st = shared.lock().unwrap();
        request_status_fetch(&mut st, &proxy);
    }
    let mut prompt_first_run = prompt_first_run;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if matches!(
            event,
            tao::event::Event::NewEvents(tao::event::StartCause::Init)
        ) {
            if prompt_first_run {
                prompt_first_run = false;
                let first_run_proxy = proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    let _ = first_run_proxy.send_event(Action::FirstRun);
                });
            }
            return;
        }
        let tao::event::Event::UserEvent(action) = event else {
            return;
        };

        let mut st = shared.lock().unwrap();

        match action {
            Action::FirstRun => {
                let menu_action = match show_first_run_choice() {
                    FirstRunChoice::Start => Some(MenuAction::AddFolder),
                    FirstRunChoice::Join => Some(MenuAction::JoinComputer),
                    FirstRunChoice::Later => None,
                };
                if let Some(menu_action) = menu_action {
                    handle_menu_action(&mut st, menu_action, &proxy);
                }
                apply_ui(&st, &tray, &mut visual);
            }
            Action::Refresh => {
                // Other CLI processes can add or stop folders while the tray is
                // open. Refresh the shared registry on every UI refresh so a
                // new mirrored folder appears within one polling interval.
                st.invalidate_recent();
                st.cached_recent();
                if st.adopt_recent_if_unconfigured() {
                    st.last_status = None;
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::StatusReady {
                generation,
                workspace,
                status,
            } => {
                let stale =
                    generation != st.task_generation || st.workspace.as_ref() != Some(&workspace);
                if stale {
                    if st.status_inflight {
                        st.status_inflight = false;
                        if st.status_pending {
                            st.status_pending = false;
                            request_status_fetch(&mut st, &proxy);
                        }
                    }
                    return;
                }
                st.status_inflight = false;
                match status {
                    Ok(s) => {
                        st.last_status = Some(s);
                        st.status_failed = false;
                        st.error_message = None;
                    }
                    // Keep the last good status on a transient CLI failure.
                    Err(error) => {
                        st.status_failed = true;
                        st.error_message = Some(error);
                    }
                }
                st.check_watch_alive();
                st.cached_recent();
                apply_ui(&st, &tray, &mut visual);
                if st.status_pending {
                    st.status_pending = false;
                    request_status_fetch(&mut st, &proxy);
                }
            }
            Action::HealthReady { workspace, report } => {
                st.health_inflight = false;
                if st.workspace.as_ref() != Some(&workspace) {
                    apply_ui(&st, &tray, &mut visual);
                    return;
                }
                match report {
                    Err(error) => {
                        st.error_message = Some(error.clone());
                        activate_for_native_dialog();
                        let _ = rfd::MessageDialog::new()
                            .set_title("System health check unavailable")
                            .set_description(error)
                            .set_level(rfd::MessageLevel::Error)
                            .set_buttons(rfd::MessageButtons::Ok)
                            .show();
                    }
                    Ok(report) => {
                        let needs_repair = health_report_needs_repair(&report);
                        let has_warning = report
                            .checks
                            .iter()
                            .any(|check| check.status == HealthStatus::Warning);
                        let mut description = health_report_description(&report);
                        if needs_repair {
                            description.push_str(
                                "\n\nRepair Mirroring reuses this workspace's existing encryption and setup, retries normal synchronization, and reinstalls its background services. Conflicts are never resolved automatically.",
                            );
                        }
                        activate_for_native_dialog();
                        let mut dialog = rfd::MessageDialog::new()
                            .set_title(if needs_repair {
                                "FeanorFS needs attention"
                            } else {
                                "FeanorFS system health"
                            })
                            .set_description(description)
                            .set_level(if needs_repair {
                                rfd::MessageLevel::Error
                            } else if has_warning {
                                rfd::MessageLevel::Warning
                            } else {
                                rfd::MessageLevel::Info
                            });
                        if needs_repair {
                            dialog = dialog.set_buttons(rfd::MessageButtons::OkCancelCustom(
                                HEALTH_REPAIR.into(),
                                HEALTH_CLOSE.into(),
                            ));
                        } else {
                            dialog = dialog.set_buttons(rfd::MessageButtons::Ok);
                        }
                        let choice = dialog.show();
                        if needs_repair && health_choice_requests_repair(&choice) {
                            begin_workspace_repair(&mut st, workspace, &proxy);
                        } else {
                            st.error_message = needs_repair
                                .then(|| "System health found issues that need attention.".into());
                        }
                    }
                }
                apply_ui(&st, &tray, &mut visual);
            }
            Action::UpdateReady(result) => {
                st.update_inflight = false;
                match result {
                    Err(error) => {
                        st.error_message = Some(error.clone());
                        activate_for_native_dialog();
                        let _ = rfd::MessageDialog::new()
                            .set_title("Could not check for updates")
                            .set_description(error)
                            .set_level(rfd::MessageLevel::Error)
                            .set_buttons(rfd::MessageButtons::Ok)
                            .show();
                    }
                    Ok(result) => {
                        let available = result.status == UpdateStatus::UpdateAvailable;
                        activate_for_native_dialog();
                        let mut dialog = rfd::MessageDialog::new()
                            .set_title(if available {
                                "FeanorFS update available"
                            } else {
                                "FeanorFS updates"
                            })
                            .set_description(update_description(&result))
                            .set_level(rfd::MessageLevel::Info);
                        if available {
                            dialog = dialog.set_buttons(rfd::MessageButtons::OkCancelCustom(
                                UPDATE_OPEN.into(),
                                UPDATE_LATER.into(),
                            ));
                        } else {
                            dialog = dialog.set_buttons(rfd::MessageButtons::Ok);
                        }
                        let choice = dialog.show();
                        if available
                            && update_choice_opens_release(&choice)
                            && open::that(&result.release_url).is_err()
                        {
                            st.error_message = Some(
                                "The official release page could not be opened. The installed app was not changed. Try Check for Updates again."
                                    .into(),
                            );
                        }
                    }
                }
                apply_ui(&st, &tray, &mut visual);
            }
            Action::MenuClick(id) => {
                if let Some(menu_action) = parse_menu_action(&id) {
                    let needs_ui = matches!(
                        menu_action,
                        MenuAction::AddFolder
                            | MenuAction::StopMirroring
                            | MenuAction::OpenFolder
                            | MenuAction::Pair
                            | MenuAction::ExportRecovery
                            | MenuAction::ImportRecovery
                            | MenuAction::CheckHealth
                            | MenuAction::CheckUpdates
                            | MenuAction::TogglePause
                            | MenuAction::ForgetUnavailable
                            | MenuAction::SwitchWorkspace(_)
                    );
                    handle_menu_action(&mut st, menu_action, &proxy);
                    if needs_ui {
                        apply_ui(&st, &tray, &mut visual);
                    }
                }
            }
            Action::TaskDone {
                error,
                restart_watch,
                set_paused,
                generation,
            } => {
                if generation != st.task_generation {
                    return;
                }
                if let Some(e) = error {
                    st.error_message = Some(e);
                    if let Some(wanted_paused) = set_paused {
                        let workspace = st.workspace.clone();
                        let paused_on_disk = workspace.as_deref().is_some_and(is_paused_on_disk);
                        if let Some(ref mut s) = st.last_status {
                            s.paused = paused_on_disk;
                        }
                        if wanted_paused && !paused_on_disk {
                            st.start_watch();
                        }
                    }
                } else {
                    st.error_message = None;
                    if let (Some(p), Some(ref mut s)) = (set_paused, st.last_status.as_mut()) {
                        s.paused = p;
                    }
                }
                if restart_watch && !st.is_paused() && !st.external_watcher_active() {
                    st.start_watch();
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::SwitchDone {
                generation,
                path,
                error,
            } => {
                if generation != st.task_generation {
                    return;
                }
                st.switch_inflight = false;
                if let Some(e) = error {
                    st.error_message = Some(e);
                } else {
                    st.stop_watch();
                    st.workspace = Some(path);
                    st.invalidate_recent();
                    st.cached_recent();
                    st.reset_watch_policy();
                    st.last_status = None;
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::ForgetUnavailableDone {
                generation,
                before,
                result,
            } => {
                if generation != st.task_generation {
                    return;
                }
                st.switch_inflight = false;
                match result {
                    Ok(recent) => {
                        let removed = before.min(
                            before.saturating_sub(unavailable_workspace_count(&recent)),
                        );
                        st.recent = Some(recent);
                        st.recent_fetched_at = Some(Instant::now());
                        st.error_message = None;
                        if st.workspace.is_none() {
                            let _ = st.adopt_recent_if_unconfigured();
                        }
                        let noun = if removed == 1 { "folder" } else { "folders" };
                        let _ = rfd::MessageDialog::new()
                            .set_title("Folder list cleaned up")
                            .set_description(format!(
                                "Removed {removed} unavailable {noun} from the tray. No files, encrypted setup, credentials, services, hub data, or remote snapshots were changed."
                            ))
                            .set_level(rfd::MessageLevel::Info)
                            .set_buttons(rfd::MessageButtons::Ok)
                            .show();
                    }
                    Err(error) => st.error_message = Some(error),
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::SetupDone {
                generation,
                path,
                kind,
                error,
            } => {
                if generation != st.task_generation {
                    return;
                }
                st.setup_inflight = false;
                st.setup_kind = None;
                let dialog = if let Some(error) = error {
                    let configured = workspace_has_config(&path);
                    let (title, description) =
                        setup_failure_copy(kind, &path, configured, &error);
                    st.error_message = Some(format!(
                        "{} Your files were not changed.",
                        title.trim_end_matches('.')
                    ));
                    Some((title, description, false))
                } else if !workspace_has_config(&path) {
                    let error = "Setup ended before FeanorFS could save the encrypted folder configuration.";
                    let (title, description) =
                        setup_failure_copy(kind, &path, false, error);
                    st.error_message = Some(format!(
                        "{} Your files were not changed.",
                        title.trim_end_matches('.')
                    ));
                    Some((title, description, false))
                } else {
                    st.stop_watch();
                    st.workspace = Some(path.clone());
                    st.invalidate_recent();
                    st.cached_recent();
                    st.reset_watch_policy();
                    st.last_status = None;
                    let (title, description) = setup_success_copy(kind, &path);
                    Some((title, description, true))
                };
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
                if let Some((title, description, success)) = dialog {
                    show_setup_result_dialog(title, description, success);
                }
            }
            Action::SetupCanceled { generation } => {
                if generation != st.task_generation {
                    return;
                }
                st.setup_inflight = false;
                st.setup_kind = None;
                st.error_message = None;
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::StopDone {
                generation,
                path,
                error,
            } => {
                if generation != st.task_generation || st.workspace.as_ref() != Some(&path) {
                    return;
                }
                st.stop_inflight = false;
                if let Some(error) = error {
                    st.error_message = Some(error);
                } else {
                    st.workspace = None;
                    st.last_status = None;
                    st.invalidate_recent();
                    st.reset_watch_policy();
                    st.cached_recent();
                    let _ = st.adopt_recent_if_unconfigured();
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::PairReady {
                generation,
                code,
                expires_in_seconds,
            } => {
                if generation != st.task_generation || !st.pair_inflight {
                    st.cancel_pairing();
                    return;
                }
                st.error_message = Some("Waiting for the other computer…".into());
                apply_ui(&st, &tray, &mut visual);
                let description = pairing_dialog_description(&code, expires_in_seconds);
                copy_pairing_clipboard(&code);
                let _ = rfd::MessageDialog::new()
                    .set_title("Share selected folder")
                    .set_description(description)
                    .set_level(rfd::MessageLevel::Info)
                    .set_buttons(rfd::MessageButtons::OkCancel)
                    .show();
                clear_pairing_clipboard(&code);
                st.cancel_pairing();
                st.error_message = Some("Closing secure pairing…".into());
                apply_ui(&st, &tray, &mut visual);
            }
            Action::PairDone {
                generation,
                paired,
                canceled,
                error,
            } => {
                if generation != st.task_generation {
                    return;
                }
                st.pair_cancel = None;
                st.pair_inflight = false;
                st.error_message = if let Some(error) = error {
                    Some(error)
                } else if paired {
                    Some("Folder shared successfully.".into())
                } else if canceled {
                    None
                } else {
                    Some(
                        "The folder wasn’t shared. No access was granted. Try Share Selected Folder again."
                            .into(),
                    )
                };
                if st.quit_pending {
                    st.stop_watch();
                    std::process::exit(0);
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::RecoveryDone {
                generation,
                restored_folder,
                error,
            } => {
                if generation != st.task_generation {
                    return;
                }
                st.recovery_inflight = false;
                if let Some(error) = error {
                    st.error_message = Some(error);
                } else if let Some(path) = restored_folder {
                    if workspace_has_config(&path) {
                        st.stop_watch();
                        st.workspace = Some(path);
                        st.invalidate_recent();
                        st.cached_recent();
                        st.reset_watch_policy();
                        st.last_status = None;
                        let _ = rfd::MessageDialog::new()
                            .set_title("Workspace restored")
                            .set_description(
                                "The encrypted recovery kit was authenticated. FeanorFS restored the workspace and enabled automatic syncing.",
                            )
                            .set_level(rfd::MessageLevel::Info)
                            .set_buttons(rfd::MessageButtons::Ok)
                            .show();
                    } else {
                        st.error_message = Some(
                            "The recovery kit was accepted, but automatic mirroring was not enabled. Existing files were preserved. Try restoring again; if this continues, choose Check System Health… from the tray."
                                .into(),
                        );
                    }
                } else {
                    st.error_message = None;
                    let _ = rfd::MessageDialog::new()
                        .set_title("Recovery kit saved")
                        .set_description(
                            "The workspace capability is encrypted. Keep the kit and its passphrase in separate safe places.",
                        )
                        .set_level(rfd::MessageLevel::Info)
                        .set_buttons(rfd::MessageButtons::Ok)
                        .show();
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::tray_contract::{
        RecentWorkspaceEntry, TrayAgentsSummary, TrayStatusResult,
    };
    use icons::visual_from_state;

    fn make_status(mirror_state: &str, paused: bool) -> TrayStatusResult {
        TrayStatusResult {
            mirror_state: mirror_state.into(),
            paused,
            watching: true,
            workspace_path: "/tmp/test".into(),
            workspace_id: "test-workspace".into(),
            workspace_label: "test".into(),
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
    fn folder_setup_has_immediate_activity_and_clear_completion_copy() {
        let path = Path::new("/Users/test/project");
        let mut state = AppState::new(Some(path.to_path_buf()));
        state.setup_inflight = true;
        state.setup_kind = Some(SetupKind::AddFolder);
        assert_eq!(activity_header(&state), Some("FeanorFS — adding folder…"));

        let (title, success) = setup_success_copy(SetupKind::AddFolder, path);
        assert_eq!(title, "Folder ready");
        assert!(success.contains("sync automatically"));

        let (title, failure) =
            setup_failure_copy(SetupKind::AddFolder, path, true, "mirror offline");
        assert_eq!(title, "Folder wasn’t added");
        assert!(failure.contains("already has FeanorFS setup"));
        assert!(failure.contains("files and encrypted setup were not changed"));
        assert!(failure.contains("choose Add Folder again"));
        assert!(failure.contains("mirror offline"));
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
    fn first_run_custom_buttons_route_to_existing_start_and_join_actions() {
        assert_eq!(
            first_run_choice(rfd::MessageDialogResult::Custom(FIRST_RUN_START.into())),
            FirstRunChoice::Start
        );
        assert_eq!(
            first_run_choice(rfd::MessageDialogResult::Custom(FIRST_RUN_JOIN.into())),
            FirstRunChoice::Join
        );
        assert_eq!(
            first_run_choice(rfd::MessageDialogResult::Custom(FIRST_RUN_LATER.into())),
            FirstRunChoice::Later
        );
        assert_eq!(
            first_run_choice(rfd::MessageDialogResult::Cancel),
            FirstRunChoice::Later
        );
    }

    #[test]
    fn health_copy_uses_generic_labels_and_never_doctor_details() {
        let report = HealthReport {
            ok: false,
            checks: vec![
                feanorfs::HealthCheck {
                    name: "server".into(),
                    status: HealthStatus::Failure,
                },
                feanorfs::HealthCheck {
                    name: "relay".into(),
                    status: HealthStatus::Warning,
                },
                feanorfs::HealthCheck {
                    name: "unknown_future_check".into(),
                    status: HealthStatus::Failure,
                },
            ],
        };
        let copy = health_report_description(&report);
        assert!(health_report_needs_repair(&report));
        assert!(copy.contains("Mirror connection"));
        assert!(copy.contains("Off-LAN connection"));
        assert!(copy.contains("FeanorFS component"));
        assert!(!copy.contains("server"));
        assert!(!copy.contains("relay"));
        assert!(!copy.contains("unknown_future_check"));
    }

    #[test]
    fn healthy_report_is_plain_and_needs_no_repair() {
        let report = HealthReport {
            ok: true,
            checks: vec![feanorfs::HealthCheck {
                name: "e2ee".into(),
                status: HealthStatus::Ok,
            }],
        };
        assert!(!health_report_needs_repair(&report));
        assert!(health_report_description(&report).contains("healthy"));
    }

    #[test]
    fn health_repair_requires_the_explicit_custom_button() {
        assert!(health_choice_requests_repair(
            &rfd::MessageDialogResult::Custom(HEALTH_REPAIR.into())
        ));
        assert!(!health_choice_requests_repair(
            &rfd::MessageDialogResult::Custom(HEALTH_CLOSE.into())
        ));
        assert!(!health_choice_requests_repair(
            &rfd::MessageDialogResult::Cancel
        ));
    }

    #[test]
    fn health_check_blocks_mutations_but_keeps_open_and_quit_available() {
        assert!(action_allowed_while_background_check_runs(
            &MenuAction::OpenFolder
        ));
        assert!(action_allowed_while_background_check_runs(
            &MenuAction::Quit
        ));
        assert!(!action_allowed_while_background_check_runs(
            &MenuAction::SyncNow
        ));
        assert!(!action_allowed_while_background_check_runs(
            &MenuAction::StopMirroring
        ));
    }

    #[test]
    fn update_copy_and_open_choice_are_status_driven() {
        let available = UpdateCheckResult {
            status: UpdateStatus::UpdateAvailable,
            current_version: "0.4.0".into(),
            latest_version: "0.5.0".into(),
            release_url: "https://github.com/rapm94/feanorfs/releases/tag/v0.5.0".into(),
        };
        let copy = update_description(&available);
        assert!(copy.contains("0.5.0"));
        assert!(copy.contains("will not download or execute"));
        assert!(update_choice_opens_release(
            &rfd::MessageDialogResult::Custom(UPDATE_OPEN.into())
        ));
        assert!(!update_choice_opens_release(
            &rfd::MessageDialogResult::Custom(UPDATE_LATER.into())
        ));
        assert!(!update_choice_opens_release(
            &rfd::MessageDialogResult::Cancel
        ));

        let current = UpdateCheckResult {
            status: UpdateStatus::UpToDate,
            latest_version: "0.4.0".into(),
            ..available.clone()
        };
        assert!(update_description(&current).contains("up to date"));
        let development = UpdateCheckResult {
            status: UpdateStatus::DevelopmentBuild,
            current_version: "0.6.0".into(),
            ..available
        };
        assert!(update_description(&development).contains("newer"));
    }

    #[test]
    fn pairing_duration_is_plain_language() {
        assert_eq!(format_duration(30), "30 seconds");
        assert_eq!(format_duration(60), "1 minute");
        assert_eq!(format_duration(300), "5 minutes");
    }

    #[test]
    fn off_lan_pairing_dialog_keeps_long_capability_in_clipboard() {
        let capability = format!("fnp2-{}", "ab".repeat(300));
        let description = pairing_dialog_description(&capability, 300);
        assert!(description.contains("one-time sharing code"));
        assert!(description.contains("Join a Shared Folder"));
        assert!(!description.contains("Terminal"));
        assert!(!description.contains(&capability));
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
        std::fs::create_dir_all(configured.join(".feanorfs")).unwrap();
        std::fs::write(configured.join(".feanorfs/config.json"), b"{}").unwrap();

        let recent = RecentWorkspacesResult {
            active: Some(stale.to_string_lossy().into_owned()),
            workspaces: vec![RecentWorkspaceEntry {
                path: configured.to_string_lossy().into_owned(),
                workspace_id: "fsw1-test".into(),
                label: "configured".into(),
            }],
        };
        assert_eq!(configured_recent_workspace(&recent), Some(configured));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parse_menu_action_known_ids() {
        assert!(matches!(
            parse_menu_action("add-folder"),
            Some(MenuAction::AddFolder)
        ));
        assert!(matches!(
            parse_menu_action("join-computer"),
            Some(MenuAction::JoinComputer)
        ));
        assert!(matches!(
            parse_menu_action("stop-mirroring"),
            Some(MenuAction::StopMirroring)
        ));
        assert!(matches!(
            parse_menu_action("open"),
            Some(MenuAction::OpenFolder)
        ));
        assert!(matches!(
            parse_menu_action("pause"),
            Some(MenuAction::TogglePause)
        ));
        assert!(matches!(
            parse_menu_action("sync-now"),
            Some(MenuAction::SyncNow)
        ));
        assert!(matches!(parse_menu_action("pair"), Some(MenuAction::Pair)));
        assert!(matches!(
            parse_menu_action("recovery-export"),
            Some(MenuAction::ExportRecovery)
        ));
        assert!(matches!(
            parse_menu_action("recovery-import"),
            Some(MenuAction::ImportRecovery)
        ));
        assert!(matches!(
            parse_menu_action("forget-unavailable"),
            Some(MenuAction::ForgetUnavailable)
        ));
        assert!(matches!(
            parse_menu_action("health"),
            Some(MenuAction::CheckHealth)
        ));
        assert!(matches!(
            parse_menu_action("update"),
            Some(MenuAction::CheckUpdates)
        ));
        assert!(matches!(parse_menu_action("quit"), Some(MenuAction::Quit)));
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
        std::fs::create_dir_all(available.join(".feanorfs")).unwrap();
        std::fs::write(available.join(".feanorfs/config.json"), b"{}").unwrap();
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

        assert_eq!(unavailable_workspace_count(&recent), 1);
        let unavailable_item = workspace_switch_item(
            "offline drive",
            &unavailable.to_string_lossy(),
            recent.active.as_deref(),
        );
        assert!(!unavailable_item.available);
        assert!(unavailable_item.selected);
        assert!(unavailable_item.label.contains("offline drive"));
        assert!(unavailable_item.label.ends_with("— unavailable"));

        let available_item = workspace_switch_item("available", &available.to_string_lossy(), None);
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
        let items = mirrored_folder_menu_items(&state);
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

    #[test]
    fn parse_menu_action_keep_prefixes() {
        assert!(matches!(
            parse_menu_action("keep-local:src/main.rs"),
            Some(MenuAction::Keep { ref path, ref choice }) if path == "src/main.rs" && choice == "local"
        ));
        assert!(matches!(
            parse_menu_action("keep-cloud:src/lib.rs"),
            Some(MenuAction::Keep { ref path, ref choice }) if path == "src/lib.rs" && choice == "cloud"
        ));
        assert!(matches!(
            parse_menu_action("keep-both:README.md"),
            Some(MenuAction::Keep { ref path, ref choice }) if path == "README.md" && choice == "both"
        ));
    }

    #[test]
    fn parse_menu_action_land_prefix() {
        assert!(matches!(
            parse_menu_action("land:ci1"),
            Some(MenuAction::Land { ref agent }) if agent == "ci1"
        ));
    }

    #[test]
    fn parse_menu_action_switch_prefix() {
        match parse_menu_action("switch:/Users/test/project") {
            Some(MenuAction::SwitchWorkspace(ref p)) => {
                assert_eq!(p.to_string_lossy(), "/Users/test/project");
            }
            other => panic!("expected SwitchWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn parse_menu_action_unknown_returns_none() {
        assert!(parse_menu_action("random-id").is_none());
        assert!(parse_menu_action("").is_none());
        assert!(parse_menu_action("header").is_none());
    }

    #[test]
    fn visual_from_state_all_mirror_values() {
        assert_eq!(visual_from_state("idle", false), TrayVisual::Idle);
        assert_eq!(
            visual_from_state("out_of_sync", false),
            TrayVisual::OutOfSync
        );
        assert_eq!(visual_from_state("offline", false), TrayVisual::Offline);
        assert_eq!(visual_from_state("conflict", false), TrayVisual::Conflict);
        assert_eq!(visual_from_state("error", false), TrayVisual::Error);
        assert_eq!(visual_from_state("syncing", false), TrayVisual::Syncing);
    }

    #[test]
    fn visual_from_state_paused_overrides() {
        assert_eq!(visual_from_state("idle", true), TrayVisual::Paused);
        assert_eq!(visual_from_state("conflict", true), TrayVisual::Paused);
        assert_eq!(visual_from_state("error", true), TrayVisual::Paused);
    }

    #[test]
    fn visual_from_state_unknown_fallsback_to_idle() {
        assert_eq!(visual_from_state("bogus", false), TrayVisual::Idle);
        assert_eq!(visual_from_state("", false), TrayVisual::Idle);
    }

    #[test]
    fn header_label_idle() {
        let s = make_status("idle", false);
        assert!(header_label(&s).contains("up to date"));
    }

    #[test]
    fn header_label_paused() {
        let s = make_status("idle", true);
        assert!(header_label(&s).contains("(paused)"));
    }

    #[test]
    fn header_label_error() {
        let s = make_status("error", false);
        assert!(header_label(&s).contains("error"));
    }
}
