//! Pure menu construction and menu action IDs.
//!
//! Builds the tray menu from the derived view model and maps menu item ids
//! back to typed actions. No native windows and no subprocesses.

use crate::model::{
    activity_header, menu_actions_enabled, mirrored_folder_menu_items, unavailable_workspace_count,
    unmanaged_terminal_watcher_active, AppState,
};
use feanorfs_common::tray_contract::TrayStatusResult;
use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use std::path::PathBuf;

pub(crate) fn header_label(status: &TrayStatusResult) -> String {
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

pub(crate) fn build_menu(state: &AppState) -> Menu {
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
        if let Some(msg) = &state.error_message {
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
            let _ = conflict_menu.append(&MenuItem::with_id(
                muda::MenuId::new("keep-all-local"),
                format!("Keep all {} local versions…", s.pending_conflicts.len()),
                actions_enabled,
                None,
            ));
            let _ = conflict_menu.append(&MenuItem::with_id(
                muda::MenuId::new("keep-all-cloud"),
                format!("Keep all {} mirror versions…", s.pending_conflicts.len()),
                actions_enabled,
                None,
            ));
            let _ = conflict_menu.append(&PredefinedMenuItem::separator());
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
        if let Some(msg) = &state.error_message {
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
pub(crate) enum MenuAction {
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
    KeepAll { choice: String },
    Land { agent: String },
    SwitchWorkspace(PathBuf),
    ForgetUnavailable,
    CheckHealth,
    CheckUpdates,
    Quit,
}

pub(crate) fn parse_menu_action(id: &str) -> Option<MenuAction> {
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
    if let Some(choice) = id.strip_prefix("keep-all-") {
        if matches!(choice, "local" | "cloud") {
            return Some(MenuAction::KeepAll {
                choice: choice.into(),
            });
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_menu_action_keep_prefixes() {
        assert!(matches!(
            parse_menu_action("keep-local:src/main.rs"),
            Some(MenuAction::Keep { path, choice })
                if path == "src/main.rs" && choice == "local"
        ));
        assert!(matches!(
            parse_menu_action("keep-cloud:src/lib.rs"),
            Some(MenuAction::Keep { path, choice })
                if path == "src/lib.rs" && choice == "cloud"
        ));
        assert!(matches!(
            parse_menu_action("keep-both:README.md"),
            Some(MenuAction::Keep { path, choice })
                if path == "README.md" && choice == "both"
        ));
    }

    #[test]
    fn parse_menu_action_bulk_keep_choices() {
        assert!(matches!(
            parse_menu_action("keep-all-local"),
            Some(MenuAction::KeepAll { choice }) if choice == "local"
        ));
        assert!(matches!(
            parse_menu_action("keep-all-cloud"),
            Some(MenuAction::KeepAll { choice }) if choice == "cloud"
        ));
    }

    #[test]
    fn parse_menu_action_land_prefix() {
        assert!(matches!(
            parse_menu_action("land:ci1"),
            Some(MenuAction::Land { agent }) if agent == "ci1"
        ));
    }

    #[test]
    fn parse_menu_action_switch_prefix() {
        match parse_menu_action("switch:/Users/test/project") {
            Some(MenuAction::SwitchWorkspace(p)) => {
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
    fn header_label_idle() {
        let s = status("idle", false);
        assert!(header_label(&s).contains("up to date"));
    }

    #[test]
    fn header_label_paused() {
        let s = status("idle", true);
        assert!(header_label(&s).contains("(paused)"));
    }

    #[test]
    fn header_label_error() {
        let s = status("error", false);
        assert!(header_label(&s).contains("error"));
    }

    fn status(mirror_state: &str, paused: bool) -> TrayStatusResult {
        TrayStatusResult {
            mirror_state: mirror_state.into(),
            paused,
            watching: true,
            workspace_path: "/tmp/test".into(),
            workspace_id: "test-workspace".into(),
            workspace_label: "test".into(),
            pending_conflict_count: 0,
            pending_conflicts: vec![],
            agents: feanorfs_common::tray_contract::TrayAgentsSummary {
                working: 0,
                need_attention: 0,
                entries: vec![],
            },
        }
    }
}
