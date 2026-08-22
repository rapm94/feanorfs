//! Menu action dispatch and background task policy.
//!
//! Translates typed menu actions into native file/password dialogs, exclusive
//! service actions, and background worker threads that report back through the
//! event loop. Nothing here touches the menu structure.

use crate::dialogs::{
    activate_for_native_dialog, native_password_input, prompt_new_recovery_passphrase,
    prompt_recovery_passphrase,
};
use crate::feanorfs::{
    agent_land, background_service_managed, background_service_start, background_service_stop,
    check_for_updates, conflicts_keep, conflicts_keep_all, export_recovery_kit,
    forget_unavailable_workspaces, import_recovery_kit, install_update, join_workspace,
    run_pairing_session, stop_workspace, sync_once, system_health, tray_activate, tray_pause,
    tray_setup, tray_status, workspace_has_config, PairSessionEvent, UpdateStatus,
};
use crate::menu::MenuAction;
use crate::model::{unavailable_workspace_count, AppState, SetupKind};
use crate::Action;
use std::path::Path;
use std::path::PathBuf;
use tao::event_loop::EventLoopProxy;

pub(crate) fn quit_tray(state: &mut AppState) {
    if !state.managed_launch {
        if let Err(error) = crate::instance::record_user_quit() {
            state.quit_pending = false;
            state.error_message = Some(format!(
                "FeanorFS could not record Quit safely, so the tray is still open: {error}"
            ));
            return;
        }
    }
    state.stop_watch();
    std::process::exit(0);
}

pub(crate) fn request_status_fetch(state: &mut AppState, proxy: &EventLoopProxy<Action>) {
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

pub(crate) fn begin_workspace_repair(
    state: &mut AppState,
    workspace: PathBuf,
    proxy: &EventLoopProxy<Action>,
) {
    state.task_generation = state.task_generation.saturating_add(1);
    let generation = state.task_generation;
    state.setup_inflight = true;
    state.setup_kind = Some(SetupKind::Repair);
    state.error_message = Some("Repairing encrypted mirroring…".into());
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let result = tray_setup(&workspace);
        let _ = proxy.send_event(Action::SetupDone {
            generation,
            path: workspace,
            kind: SetupKind::Repair,
            result,
        });
    });
}

fn action_allowed_while_background_check_runs(action: &MenuAction) -> bool {
    matches!(action, MenuAction::OpenFolder | MenuAction::Quit)
}

pub(crate) fn handle_menu_action(
    state: &mut AppState,
    action: MenuAction,
    proxy: &EventLoopProxy<Action>,
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
                let result = tray_setup(&path);
                let _ = proxy.send_event(Action::SetupDone {
                    generation,
                    path,
                    kind: SetupKind::AddFolder,
                    result,
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
            std::thread::spawn(move || {
                let outcome = join_workspace(&path, pairing_code);
                if outcome.canceled {
                    let _ = proxy.send_event(Action::SetupCanceled { generation });
                } else {
                    let _ = proxy.send_event(Action::SetupDone {
                        generation,
                        path,
                        kind: SetupKind::JoinFolder,
                        result: outcome.result,
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
        MenuAction::InstallUpdate => {
            let Some(expected) = state
                .last_update
                .as_ref()
                .filter(|check| check.status == UpdateStatus::UpdateAvailable)
                .map(|check| check.latest_version.clone())
            else {
                return;
            };
            if state.update_inflight
                || state.setup_inflight
                || state.stop_inflight
                || state.switch_inflight
                || state.pair_inflight
                || state.recovery_inflight
            {
                return;
            }
            // Clicking the gated item is the explicit install consent.
            state.update_inflight = true;
            state.error_message = Some(format!("Installing FeanorFS {expected}…"));
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let _ = proxy.send_event(Action::ApplyReady(install_update(&expected)));
            });
        }
        MenuAction::Quit => {
            if state.pair_inflight {
                state.quit_pending = true;
                state.error_message = Some("Closing secure pairing…".into());
                state.cancel_pairing();
                return;
            }
            quit_tray(state);
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
        MenuAction::KeepAll { choice } => {
            let Some(workspace) = state.workspace.clone() else {
                return;
            };
            let count = state
                .last_status
                .as_ref()
                .map_or(0, |status| status.pending_conflicts.len());
            if count == 0 {
                return;
            }
            let (title, consequence) = if choice == "local" {
                (
                    format!("Keep all {count} local versions?"),
                    "Every conflicting mirror version will be discarded. Current local files and local deletions become the shared result.",
                )
            } else {
                (
                    format!("Keep all {count} mirror versions?"),
                    "Every conflicting local version will be replaced or deleted to match the mirror. Local conflict copies remain recoverable only in immutable FeanorFS history.",
                )
            };
            let confirmed = rfd::MessageDialog::new()
                .set_title(title)
                .set_description(format!(
                    "This applies one choice to {count} paths.\n\n{consequence}\n\nFeanorFS will not merge file contents. Choose OK only if this single policy is correct for every listed conflict."
                ))
                .set_level(rfd::MessageLevel::Warning)
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if !matches!(confirmed, rfd::MessageDialogResult::Ok) {
                return;
            }
            let external_watcher = state.external_watcher_active();
            state.stop_watch();
            let generation = state.task_generation;
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                let error = run_exclusive_service_action(&workspace, external_watcher, || {
                    conflicts_keep_all(&workspace, &choice).and_then(|()| sync_once(&workspace))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
