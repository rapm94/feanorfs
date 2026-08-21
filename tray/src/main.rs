#[cfg(test)]
feanorfs_test_support::isolate_test_process!();

mod actions;
mod dialogs;
mod feanorfs;
mod icons;
mod instance;
mod menu;
mod model;
mod password_dialog;
mod ui;

use actions::{begin_workspace_repair, handle_menu_action, quit_tray, request_status_fetch};
use dialogs::{
    health_report_needs_repair, setup_failure_copy, setup_success_copy, show_first_run_choice,
    show_forget_unavailable_result, show_health_dialog, show_health_unavailable,
    show_pairing_dialog, show_recovery_kit_saved_dialog, show_setup_result_dialog,
    show_update_dialog, show_update_error, show_workspace_restored_dialog, FirstRunChoice,
};
use feanorfs::{workspace_has_config, HealthReport, UpdateCheckResult};
use feanorfs_common::tray_contract::{
    RecentWorkspacesResult, SetupResult, SetupStage, TrayStatusResult, MANAGED_TRAY_ARG,
};
use icons::{icon_for, visual_from_state, TrayVisual};
use menu::{build_menu, parse_menu_action, MenuAction};
use model::{
    first_run_requested, is_paused_on_disk, menu_revision, resolve_initial_workspace,
    should_prompt_first_run, unavailable_workspace_count, AppState, SetupKind, REFRESH_SECS,
};
use muda::MenuEvent;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{TrayIcon, TrayIconBuilder};

const SUPERVISED_TRAY_CONTENTION_EXIT_CODE: i32 = 75;

fn print_version_and_exit(arguments: &[std::ffi::OsString]) -> bool {
    let version_requested = arguments.len() == 1
        && arguments[0]
            .to_str()
            .is_some_and(|argument| matches!(argument, "--version" | "-V"));
    if version_requested {
        println!("feanorfs-tray {}", env!("CARGO_PKG_VERSION"));
    }
    version_requested
}

fn managed_tray_launch(arguments: &[std::ffi::OsString]) -> bool {
    arguments
        .iter()
        .any(|argument| argument == std::ffi::OsStr::new(MANAGED_TRAY_ARG))
}

#[cfg(debug_assertions)]
fn runner_test_launch(profile: Option<&std::ffi::OsStr>, mode: Option<&std::ffi::OsStr>) -> bool {
    profile.is_some() || mode.is_some()
}

#[cfg(not(debug_assertions))]
fn runner_test_launch(_: Option<&std::ffi::OsStr>, _: Option<&std::ffi::OsStr>) -> bool {
    false
}

#[derive(Clone)]
pub(crate) enum Action {
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
        result: SetupResult,
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

fn main() {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if print_version_and_exit(&arguments) {
        return;
    }
    if runner_test_launch(
        std::env::var_os("FEANORFS_RUNNER_TEST_PROFILE").as_deref(),
        std::env::var_os("FEANORFS_RUNNER_TEST_MODE").as_deref(),
    ) {
        return;
    }
    let managed = managed_tray_launch(&arguments);
    let _instance_guard = match instance::claim() {
        Ok(instance::Claim::Primary(guard)) => guard,
        Ok(instance::Claim::AlreadyRunning) if managed => {
            std::process::exit(SUPERVISED_TRAY_CONTENTION_EXIT_CODE);
        }
        Ok(instance::Claim::AlreadyRunning) => return,
        Err(error) => {
            eprintln!("FeanorFS tray could not claim its single-instance lock: {error}");
            std::process::exit(1);
        }
    };
    match instance::take_user_quit() {
        Ok(true) if managed => return,
        Ok(_) => {}
        Err(error) => {
            eprintln!("FeanorFS tray could not consume its user-quit marker: {error}");
            std::process::exit(1);
        }
    }
    let workspace = resolve_initial_workspace();
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
    state.managed_launch = managed;
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
                        show_health_unavailable(error);
                    }
                    Ok(report) => {
                        if show_health_dialog(&report) {
                            begin_workspace_repair(&mut st, workspace, &proxy);
                        } else if health_report_needs_repair(&report) {
                            st.error_message = Some(
                                "System health found issues that need attention.".into(),
                            );
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
                        show_update_error(error);
                    }
                    Ok(result) => {
                        if show_update_dialog(&result)
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
                        if let Some(s) = &mut st.last_status {
                            s.paused = paused_on_disk;
                        }
                        if wanted_paused && !paused_on_disk {
                            st.start_watch();
                        }
                    }
                } else {
                    st.error_message = None;
                    if let (Some(p), Some(s)) = (set_paused, st.last_status.as_mut()) {
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
                        show_forget_unavailable_result(removed);
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
                result,
            } => {
                if generation != st.task_generation {
                    return;
                }
                st.setup_inflight = false;
                st.setup_kind = None;
                let dialog = if result.stage == SetupStage::TrayRegistered {
                    st.stop_watch();
                    st.workspace = Some(path.clone());
                    st.invalidate_recent();
                    st.cached_recent();
                    st.reset_watch_policy();
                    st.last_status = None;
                    let (title, description) = setup_success_copy(kind, &path);
                    Some((title, description, true))
                } else {
                    let (title, description) = setup_failure_copy(kind, &path, &result);
                    st.error_message = Some(format!(
                        "{} Your files were not changed.",
                        title.trim_end_matches('.')
                    ));
                    Some((title, description, false))
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
                show_pairing_dialog(&code, expires_in_seconds);
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
                    quit_tray(&mut st);
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
                        show_workspace_restored_dialog();
                    } else {
                        st.error_message = Some(
                            "The recovery kit was accepted, but automatic mirroring was not enabled. Existing files were preserved. Try restoring again; if this continues, choose Check System Health… from the tray."
                                .into(),
                        );
                    }
                } else {
                    st.error_message = None;
                    show_recovery_kit_saved_dialog();
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
    use icons::visual_from_state;

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
    fn version_flag_never_starts_the_ui() {
        assert!(print_version_and_exit(&["--version".into()]));
        assert!(print_version_and_exit(&["-V".into()]));
        assert!(!print_version_and_exit(&[]));
        assert!(!print_version_and_exit(&["--first-run".into()]));
    }

    #[test]
    fn only_the_exact_managed_argument_marks_a_managed_launch() {
        assert!(managed_tray_launch(&[MANAGED_TRAY_ARG.into()]));
        assert!(!managed_tray_launch(&[]));
        assert!(!managed_tray_launch(&["--first-run".into()]));
        assert!(!managed_tray_launch(&["--managed-extra".into()]));
    }

    #[test]
    fn runner_test_profiles_exit_before_claiming_the_user_tray() {
        assert!(runner_test_launch(Some("test".as_ref()), None));
        assert!(runner_test_launch(None, Some("1".as_ref())));
        assert!(!runner_test_launch(None, None));
    }
}
