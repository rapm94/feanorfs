mod feanorfs;
mod icons;

use feanorfs::feanorfs_bin;
use feanorfs::{
    agent_land, conflicts_keep, sync_once, tray_activate, tray_pause, tray_recent, tray_status,
    workspace_has_config,
};
use feanorfs_common::tray_contract::{RecentWorkspacesResult, TrayStatusResult};
use icons::{icon_for, visual_from_state, TrayVisual};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use std::path::PathBuf;
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

#[derive(Debug, Clone)]
enum Action {
    Refresh,
    StatusReady(Option<TrayStatusResult>),
    MenuClick(String),
    TaskDone {
        error: Option<String>,
        restart_watch: bool,
        /// `Some` only for pause/resume tasks — optimistic pause flag on success.
        set_paused: Option<bool>,
    },
    SwitchDone {
        path: PathBuf,
        error: Option<String>,
    },
}

struct AppState {
    workspace: PathBuf,
    watch_child: Option<Child>,
    owns_watch: bool,
    watch_failures: u32,
    last_spawn_at: Option<Instant>,
    respawn_disabled: bool,
    status_inflight: bool,
    last_status: Option<TrayStatusResult>,
    error_message: Option<String>,
    recent: Option<RecentWorkspacesResult>,
    recent_fetched_at: Option<Instant>,
}

impl AppState {
    fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            watch_child: None,
            owns_watch: false,
            watch_failures: 0,
            last_spawn_at: None,
            respawn_disabled: false,
            status_inflight: false,
            last_status: None,
            error_message: None,
            recent: None,
            recent_fetched_at: None,
        }
    }

    fn is_paused(&self) -> bool {
        self.last_status.as_ref().is_some_and(|s| s.paused)
    }

    fn external_watcher_active(&self) -> bool {
        self.watch_child.is_none() && self.last_status.as_ref().is_some_and(|s| s.watching)
    }

    fn start_watch(&mut self) {
        if self.is_paused() || self.respawn_disabled {
            return;
        }
        if self.watch_child.is_some() {
            return;
        }
        if self.external_watcher_active() {
            return;
        }

        match Command::new(feanorfs_bin())
            .args(["sync"])
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.watch_child = Some(child);
                self.owns_watch = true;
                self.last_spawn_at = Some(Instant::now());
                self.watch_failures = 0;
            }
            Err(e) => {
                self.respawn_disabled = true;
                self.error_message = Some(format!(
                    "feanorfs binary not found ({e}) — set FEANORFS_BIN"
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
                            "Sync watcher keeps crashing — check FEANORFS_BIN and workspace".into(),
                        );
                        return;
                    }
                    self.start_watch();
                }
                Ok(None) => {}
                Err(_) => {
                    self.watch_child = None;
                    self.owns_watch = false;
                    self.start_watch();
                }
            }
        } else if !self.external_watcher_active() {
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
        self.error_message = None;
    }
}

#[cfg(unix)]
fn graceful_stop_child(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn graceful_stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn resolve_initial_workspace() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FEANORFS_WORKSPACE") {
        return Some(PathBuf::from(p));
    }
    let recent = tray_recent()?;
    recent
        .active
        .into_iter()
        .chain(recent.workspaces.into_iter().map(|w| w.path))
        .map(PathBuf::from)
        .find(|p| workspace_has_config(p))
}

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

fn build_menu(state: &AppState) -> Menu {
    let menu = Menu::new();
    let status = state.last_status.as_ref();

    if let Some(s) = status {
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("header"),
            header_label(s),
            false,
            None,
        ));
        if s.watching && !state.owns_watch {
            let _ = menu.append(&MenuItem::with_id(
                muda::MenuId::new("external-watch"),
                "Watching (external feanorfs sync)",
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

        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("open"),
            "Open Folder",
            true,
            None,
        ));

        let pause_label = if s.paused {
            "Resume Syncing"
        } else {
            "Pause Syncing"
        };
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("pause"),
            pause_label,
            true,
            None,
        ));

        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("sync-now"),
            "Sync Now",
            true,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());

        if !s.pending_conflicts.is_empty() {
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
                        true,
                        None,
                    ));
                }
                let _ = conflict_menu.append(&PredefinedMenuItem::separator());
            }
            let _ = menu.append(&conflict_menu);
        }

        if !s.agents.entries.is_empty() {
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
                        true,
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
    } else {
        let _ = menu.append(&MenuItem::with_id(
            muda::MenuId::new("header"),
            "FeanorFS — no workspace",
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
    }

    if let Some(ref recent) = state.recent {
        if !recent.workspaces.is_empty() {
            let switch = Submenu::with_id(muda::MenuId::new("switch"), "Switch Workspace", true);
            for w in &recent.workspaces {
                let mark = if recent.active.as_deref() == Some(w.path.as_str()) {
                    format!("✓ {}", w.label)
                } else {
                    w.label.clone()
                };
                let _ = switch.append(&MenuItem::with_id(
                    muda::MenuId::new(format!("switch:{}", w.path)),
                    mark,
                    true,
                    None,
                ));
            }
            let _ = menu.append(&switch);
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(
        muda::MenuId::new("quit"),
        "Quit FeanorFS Tray",
        true,
        None,
    ));
    menu
}

#[derive(Debug, Clone)]
enum MenuAction {
    OpenFolder,
    TogglePause,
    SyncNow,
    Keep { path: String, choice: String },
    Land { agent: String },
    SwitchWorkspace(PathBuf),
    Quit,
}

fn parse_menu_action(id: &str) -> Option<MenuAction> {
    if id == "open" {
        return Some(MenuAction::OpenFolder);
    }
    if id == "pause" {
        return Some(MenuAction::TogglePause);
    }
    if id == "sync-now" {
        return Some(MenuAction::SyncNow);
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

fn apply_ui(state: &AppState, tray: &TrayIcon, visual: &mut TrayVisual) {
    let v = match &state.last_status {
        Some(s) => visual_from_state(&s.mirror_state, s.paused),
        None => TrayVisual::Error,
    };
    if v != *visual {
        let _ = tray.set_icon(Some(icon_for(v)));
        *visual = v;
    }
    let menu = build_menu(state);
    tray.set_menu(Some(Box::new(menu)));
}

fn request_status_fetch(state: &mut AppState, proxy: &tao::event_loop::EventLoopProxy<Action>) {
    if state.status_inflight {
        return;
    }
    state.status_inflight = true;
    let workspace = state.workspace.clone();
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let status = tray_status(&workspace);
        let _ = proxy.send_event(Action::StatusReady(status));
    });
}

fn handle_menu_action(
    state: &mut AppState,
    action: MenuAction,
    proxy: &tao::event_loop::EventLoopProxy<Action>,
) {
    match action {
        MenuAction::OpenFolder => {
            let _ = open::that(&state.workspace);
        }
        MenuAction::Quit => {
            state.stop_watch();
            std::process::exit(0);
        }
        MenuAction::TogglePause => {
            let pause = !state.is_paused();
            if pause {
                state.stop_watch();
                if let Some(ref mut s) = state.last_status {
                    s.paused = true;
                }
            }
            let workspace = state.workspace.clone();
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = tray_pause(&workspace, pause).err();
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: !pause,
                    set_paused: Some(pause),
                });
            });
        }
        MenuAction::SyncNow => {
            state.stop_watch();
            let workspace = state.workspace.clone();
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = sync_once(&workspace).err();
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: true,
                    set_paused: None,
                });
            });
        }
        MenuAction::Keep { path, choice } => {
            let workspace = state.workspace.clone();
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = conflicts_keep(&workspace, &path, &choice).err();
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: false,
                    set_paused: None,
                });
            });
        }
        MenuAction::Land { agent } => {
            let workspace = state.workspace.clone();
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = agent_land(&workspace, &agent).err();
                let _ = proxy.send_event(Action::TaskDone {
                    error,
                    restart_watch: false,
                    set_paused: None,
                });
            });
        }
        MenuAction::SwitchWorkspace(path) => {
            if !workspace_has_config(&path) {
                state.error_message = Some(format!(
                    "Not a FeanorFS workspace (missing .feanorfs/config.json): {}",
                    path.display()
                ));
                return;
            }
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = tray_activate(&path).err();
                let _ = proxy.send_event(Action::SwitchDone { path, error });
            });
        }
    }
}

fn main() {
    let workspace = match resolve_initial_workspace() {
        Some(p) => p,
        None => {
            eprintln!(
                "No workspace configured. Run `feanorfs start` in a folder, or set FEANORFS_WORKSPACE."
            );
            std::process::exit(1);
        }
    };

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

    let state = AppState::new(workspace);

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

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        let tao::event::Event::UserEvent(action) = event else {
            return;
        };

        let mut st = shared.lock().unwrap();

        match action {
            Action::Refresh => {
                request_status_fetch(&mut st, &proxy);
            }
            Action::StatusReady(status) => {
                st.status_inflight = false;
                match status {
                    Some(s) => {
                        st.last_status = Some(s);
                        st.error_message = None;
                    }
                    // Keep the last good status on a transient CLI failure.
                    None => {
                        st.error_message =
                            Some("Could not read workspace status (feanorfs failed)".into());
                    }
                }
                st.check_watch_alive();
                st.cached_recent();
                apply_ui(&st, &tray, &mut visual);
            }
            Action::MenuClick(id) => {
                if let Some(menu_action) = parse_menu_action(&id) {
                    let needs_ui = matches!(
                        menu_action,
                        MenuAction::OpenFolder
                            | MenuAction::TogglePause
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
            } => {
                if let Some(e) = error {
                    st.error_message = Some(e);
                } else {
                    st.error_message = None;
                    if let (Some(p), Some(ref mut s)) = (set_paused, st.last_status.as_mut()) {
                        s.paused = p;
                    }
                }
                if restart_watch && !st.is_paused() {
                    st.start_watch();
                }
                request_status_fetch(&mut st, &proxy);
                apply_ui(&st, &tray, &mut visual);
            }
            Action::SwitchDone { path, error } => {
                if let Some(e) = error {
                    st.error_message = Some(e);
                } else {
                    st.stop_watch();
                    st.workspace = path;
                    st.invalidate_recent();
                    st.reset_watch_policy();
                    // Old workspace's status (paused/watching) no longer applies;
                    // the refetch below restarts the watcher via check_watch_alive.
                    st.last_status = None;
                    request_status_fetch(&mut st, &proxy);
                }
                apply_ui(&st, &tray, &mut visual);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::tray_contract::{TrayAgentsSummary, TrayStatusResult};
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
    fn parse_menu_action_known_ids() {
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
        assert!(matches!(parse_menu_action("quit"), Some(MenuAction::Quit)));
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
