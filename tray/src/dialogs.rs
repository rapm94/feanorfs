//! Native dialogs and user-facing copy.
//!
//! Every native window opened by the tray goes through this module, together
//! with the copy text built for those windows. Pure copy builders are unit
//! tested here; the native dialog entry points run on the event-loop thread
//! exactly where the wiring calls them.

use crate::feanorfs::{
    clear_pairing_clipboard, copy_pairing_clipboard, HealthReport, HealthStatus, UpdateCheckResult,
    UpdateStatus,
};
use crate::model::SetupKind;
use crate::password_dialog;
use feanorfs_common::tray_contract::{SetupResult, SetupStage};
use std::path::Path;

pub(crate) const FIRST_RUN_START: &str = "Start Mirroring a Folder…";
pub(crate) const FIRST_RUN_JOIN: &str = "Join a Shared Folder…";
pub(crate) const FIRST_RUN_LATER: &str = "Not Now";
pub(crate) const HEALTH_REPAIR: &str = "Repair Mirroring";
pub(crate) const HEALTH_CLOSE: &str = "Close";
pub(crate) const UPDATE_OPEN: &str = "Open Release Page";
pub(crate) const UPDATE_LATER: &str = "Later";

#[cfg(target_os = "macos")]
pub(crate) fn activate_for_native_dialog() {
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
pub(crate) fn activate_for_native_dialog() {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstRunChoice {
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

pub(crate) fn show_first_run_choice() -> FirstRunChoice {
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

pub(crate) fn health_report_needs_repair(report: &HealthReport) -> bool {
    !report.ok
        || report
            .checks
            .iter()
            .any(|check| check.status == HealthStatus::Failure)
}

pub(crate) fn health_choice_requests_repair(choice: &rfd::MessageDialogResult) -> bool {
    matches!(
        choice,
        rfd::MessageDialogResult::Custom(value) if value == HEALTH_REPAIR
    )
}

pub(crate) fn health_report_description(report: &HealthReport) -> String {
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

pub(crate) fn update_description(result: &UpdateCheckResult) -> String {
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

pub(crate) fn update_choice_opens_release(choice: &rfd::MessageDialogResult) -> bool {
    matches!(
        choice,
        rfd::MessageDialogResult::Custom(value) if value == UPDATE_OPEN
    )
}

fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} seconds")
    } else {
        let minutes = seconds / 60;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    }
}

pub(crate) fn pairing_dialog_description(code: &str, expires_in_seconds: u64) -> String {
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

pub(crate) fn prompt_recovery_passphrase() -> Option<zeroize::Zeroizing<String>> {
    native_password_input("FeanorFS recovery", "Recovery kit passphrase")
}

pub(crate) fn prompt_new_recovery_passphrase() -> Option<zeroize::Zeroizing<String>> {
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

pub(crate) fn native_password_input(
    title: &str,
    message: &str,
) -> Option<zeroize::Zeroizing<String>> {
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

fn folder_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("the selected folder")
        .to_string()
}

pub(crate) fn setup_success_copy(kind: SetupKind, path: &Path) -> (&'static str, String) {
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

pub(crate) fn setup_failure_copy(
    kind: SetupKind,
    path: &Path,
    result: &SetupResult,
) -> (&'static str, String) {
    let name = folder_name(path);
    let detail = result
        .detail
        .as_deref()
        .map(|detail| {
            detail
                .trim()
                .strip_prefix("Error:")
                .unwrap_or(detail.trim())
                .to_string()
        })
        .unwrap_or_default();
    match result.stage {
        SetupStage::ServiceInstalled => (
            "Folder synced — tray needs repair",
            format!(
                "“{name}” is securely synced and automatic background sync is running.\n\nChoose Add Folder again to retry the tray registration. FeanorFS will keep the existing workspace identity and recheck the completed stages.\n\nDetails: {detail}"
            ),
        ),
        SetupStage::InitialSync => (
            "Folder synced — automatic sync needs repair",
            format!(
                "The initial secure sync for “{name}” completed.\n\nChoose Add Folder again to retry automatic background sync and the tray. FeanorFS will keep the completed sync and existing workspace identity.\n\nDetails: {detail}"
            ),
        ),
        SetupStage::Paired => (
            "Folder setup saved — sync paused",
            format!(
                "The encrypted FeanorFS setup for “{name}” is saved, but the initial sync has not finished.\n\nMake sure the mirror is reachable, then choose Add Folder again. FeanorFS will resume without pairing again or changing the workspace identity.\n\nDetails: {detail}"
            ),
        ),
        _ => {
            let configured = result.committed.workspace_configured;
            let (title, outcome) = match kind {
                SetupKind::AddFolder => (
                    "Folder wasn’t added",
                    format!("“{name}” was not added."),
                ),
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
                "FeanorFS could not prepare this folder."
            };
            let next_step = if configured && kind == SetupKind::AddFolder {
                "Make sure the computer or service that hosts its existing mirror is available, then choose Add Folder again."
            } else {
                "Review the details and try again. If it keeps failing, reopen FeanorFS and retry."
            };
            (
                title,
                format!(
                    "{outcome} {cause}\n\nYour files and encrypted setup were not changed. {next_step}\n\nDetails: {detail}"
                ),
            )
        }
    }
}

pub(crate) fn show_setup_result_dialog(title: &str, description: String, success: bool) {
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

fn show_error_dialog(title: &str, description: String) {
    activate_for_native_dialog();
    let _ = rfd::MessageDialog::new()
        .set_title(title)
        .set_description(description)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

pub(crate) fn show_health_unavailable(error: String) {
    show_error_dialog("System health check unavailable", error);
}

/// Shows the system health dialog. Returns `true` when the user chose the
/// explicit Repair Mirroring button.
pub(crate) fn show_health_dialog(report: &HealthReport) -> bool {
    let needs_repair = health_report_needs_repair(report);
    let has_warning = report
        .checks
        .iter()
        .any(|check| check.status == HealthStatus::Warning);
    let mut description = health_report_description(report);
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
    needs_repair && health_choice_requests_repair(&choice)
}

pub(crate) fn show_update_error(error: String) {
    show_error_dialog("Could not check for updates", error);
}

pub(crate) fn show_update_install_error(error: String) {
    show_error_dialog("Could not install the update", error);
}

/// Confirms a completed, checksum-verified self-update.
pub(crate) fn show_update_installed(outcome: &crate::feanorfs::UpdateApplyOutcome) {
    activate_for_native_dialog();
    let message = format!(
        "FeanorFS {} is installed (previously {}). Supervised services restart on the new build automatically. Quit and reopen the tray if its menu does not refresh.",
        outcome.applied_version, outcome.previous_version
    );
    let dialog = rfd::MessageDialog::new()
        .set_title("FeanorFS updated")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok);
    let _ = dialog.show();
}

/// Shows the update-check dialog. Returns `true` when the user chose to open
/// the official release page.
pub(crate) fn show_update_dialog(result: &UpdateCheckResult) -> bool {
    let available = result.status == UpdateStatus::UpdateAvailable;
    activate_for_native_dialog();
    let mut dialog = rfd::MessageDialog::new()
        .set_title(if available {
            "FeanorFS update available"
        } else {
            "FeanorFS updates"
        })
        .set_description(update_description(result))
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
    available && update_choice_opens_release(&choice)
}

/// Shows the pairing dialog and copies the one-time code to the clipboard for
/// the duration of the dialog, exactly as the wiring previously did inline.
pub(crate) fn show_pairing_dialog(code: &str, expires_in_seconds: u64) {
    let description = pairing_dialog_description(code, expires_in_seconds);
    copy_pairing_clipboard(code);
    let _ = rfd::MessageDialog::new()
        .set_title("Share selected folder")
        .set_description(description)
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show();
    clear_pairing_clipboard(code);
}

pub(crate) fn show_forget_unavailable_result(removed: usize) {
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

pub(crate) fn show_workspace_restored_dialog() {
    let _ = rfd::MessageDialog::new()
        .set_title("Workspace restored")
        .set_description(
            "The encrypted recovery kit was authenticated. FeanorFS restored the workspace and enabled automatic syncing.",
        )
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

pub(crate) fn show_recovery_kit_saved_dialog() {
    let _ = rfd::MessageDialog::new()
        .set_title("Recovery kit saved")
        .set_description(
            "The workspace capability is encrypted. Keep the kit and its passphrase in separate safe places.",
        )
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feanorfs::HealthCheck;
    use crate::model::{activity_header, AppState};
    use feanorfs_common::tray_contract::SetupCommitted;

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

        let configured = SetupResult {
            stage: SetupStage::None,
            committed: SetupCommitted {
                workspace_configured: true,
                ..SetupCommitted::default()
            },
            retryable: true,
            recovery: None,
            detail: Some("mirror offline".into()),
        };
        let (title, failure) = setup_failure_copy(SetupKind::AddFolder, path, &configured);
        assert_eq!(title, "Folder wasn’t added");
        assert!(failure.contains("already has FeanorFS setup"));
        assert!(failure.contains("files and encrypted setup were not changed"));
        assert!(failure.contains("choose Add Folder again"));
        assert!(failure.contains("mirror offline"));

        let local_failure = SetupResult::generic("workspace identity changed");
        let (_, failure) = setup_failure_copy(SetupKind::AddFolder, path, &local_failure);
        assert!(failure.contains("could not prepare this folder"));
        assert!(failure.contains("Review the details"));
        assert!(!failure.contains("Check the connection"));
    }

    #[test]
    fn partial_setup_failures_name_the_completed_stage_and_safe_retry() {
        let path = Path::new("/Users/test/project");
        let committed = |configured, sync, service, tray| SetupCommitted {
            workspace_configured: configured,
            initial_sync_completed: sync,
            background_service_installed: service,
            tray_registered: tray,
        };
        let cases = [
            (
                SetupStage::Paired,
                committed(true, false, false, false),
                "Folder setup saved — sync paused",
                "without pairing again",
            ),
            (
                SetupStage::InitialSync,
                committed(true, true, false, false),
                "Folder synced — automatic sync needs repair",
                "retry automatic background sync and the tray",
            ),
            (
                SetupStage::ServiceInstalled,
                committed(true, true, true, false),
                "Folder synced — tray needs repair",
                "retry the tray registration",
            ),
        ];

        for (stage, committed, expected_title, expected_retry) in cases {
            let result = SetupResult {
                stage,
                committed,
                retryable: true,
                recovery: None,
                detail: Some("completely reworded detail the tray must never match".into()),
            };
            let (title, description) = setup_failure_copy(SetupKind::JoinFolder, path, &result);
            assert_eq!(title, expected_title);
            assert!(description.contains(expected_retry));
            assert!(description.contains("workspace identity"));
        }
    }

    /// Fixture: the tray classifies from the typed JSON contract, so changing
    /// CLI wording can never change tray state classification.
    #[test]
    fn setup_classification_uses_typed_json_fixtures_not_cli_wording() {
        let path = Path::new("/Users/test/project");
        let reworded = r#"{
            "stage": "initial_sync",
            "committed": {
                "workspace_configured": true,
                "initial_sync_completed": true,
                "background_service_installed": false,
                "tray_registered": false
            },
            "retryable": true,
            "recovery": "retry_start",
            "detail": "the CLI's wording changed completely and no longer mentions automatic background sync"
        }"#;
        let result: SetupResult = serde_json::from_str(reworded).unwrap();
        let (title, description) = setup_failure_copy(SetupKind::JoinFolder, path, &result);
        assert_eq!(title, "Folder synced — automatic sync needs repair");
        assert!(description.contains("workspace identity"));

        let tray_stage = r#"{
            "stage": "service_installed",
            "committed": {
                "workspace_configured": true,
                "initial_sync_completed": true,
                "background_service_installed": true,
                "tray_registered": false
            },
            "retryable": true,
            "recovery": "retry_tray",
            "detail": "reworded system tray wording"
        }"#;
        let result: SetupResult = serde_json::from_str(tray_stage).unwrap();
        let (title, _) = setup_failure_copy(SetupKind::AddFolder, path, &result);
        assert_eq!(title, "Folder synced — tray needs repair");

        let paired = r#"{
            "stage": "paired",
            "committed": {
                "workspace_configured": true,
                "initial_sync_completed": false,
                "background_service_installed": false,
                "tray_registered": false
            },
            "retryable": true,
            "recovery": "retry_start",
            "detail": "reworded initial sync wording"
        }"#;
        let result: SetupResult = serde_json::from_str(paired).unwrap();
        let (title, _) = setup_failure_copy(SetupKind::Repair, path, &result);
        assert_eq!(title, "Folder setup saved — sync paused");
    }

    #[test]
    fn setup_success_is_typed_completion_only() {
        let completed = SetupResult::completed();
        assert_eq!(completed.stage, SetupStage::TrayRegistered);
        assert_eq!(completed.recovery, None);

        let not_completed = SetupResult {
            stage: SetupStage::Paired,
            ..SetupResult::completed()
        };
        assert_ne!(not_completed.stage, SetupStage::TrayRegistered);
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
                HealthCheck {
                    name: "server".into(),
                    status: HealthStatus::Failure,
                },
                HealthCheck {
                    name: "relay".into(),
                    status: HealthStatus::Warning,
                },
                HealthCheck {
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
            checks: vec![HealthCheck {
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
}
