use clap::Subcommand;
use feanorfs_client::{
    do_tray_status, list_recent_workspaces, load_config, register_workspace, set_active_workspace,
    set_paused,
};
use feanorfs_common::TrayPauseResult;
use std::path::Path;

use super::util::output_json;

#[derive(Subcommand)]
pub enum TrayAction {
    /// Aggregate dashboard for the menu-bar tray (`TrayStatusResult`).
    Status,
    /// Stop automatic sync until `tray resume`.
    Pause,
    /// Resume automatic sync.
    Resume,
    /// Add this folder to the tray recent list.
    Register,
    /// List recent workspace folders.
    Recent,
    /// Set the active workspace for the tray switcher.
    Activate {
        /// Workspace folder path
        path: std::path::PathBuf,
    },
}

pub async fn run(current_dir: &Path, action: TrayAction, json: bool) -> anyhow::Result<()> {
    match action {
        TrayAction::Status => {
            let result = do_tray_status(current_dir).await?;
            if json {
                output_json(&result)?;
            } else {
                println!(
                    "{} — {} (paused: {}, watching: {})",
                    result.workspace_label, result.mirror_state, result.paused, result.watching
                );
                if !result.pending_conflicts.is_empty() {
                    println!("Needs attention: {}", result.pending_conflicts.len());
                }
                if result.agents.working > 0 {
                    println!(
                        "Agents: {} working · {} need attention",
                        result.agents.working, result.agents.need_attention
                    );
                }
            }
        }
        TrayAction::Pause => {
            load_config(current_dir)?;
            set_paused(current_dir, true)?;
            if json {
                output_json(&TrayPauseResult { paused: true })?;
            } else {
                println!("Sync paused for this workspace.");
            }
        }
        TrayAction::Resume => {
            load_config(current_dir)?;
            set_paused(current_dir, false)?;
            if json {
                output_json(&TrayPauseResult { paused: false })?;
            } else {
                println!("Sync resumed for this workspace.");
            }
        }
        TrayAction::Register => {
            register_workspace(current_dir)?;
            if json {
                output_json(&list_recent_workspaces()?)?;
            } else {
                println!("Registered {} for the tray.", current_dir.display());
            }
        }
        TrayAction::Recent => {
            let recent = list_recent_workspaces()?;
            if json {
                output_json(&recent)?;
            } else if recent.workspaces.is_empty() {
                println!("No recent workspaces.");
            } else {
                for w in &recent.workspaces {
                    let mark = if recent.active.as_deref() == Some(w.path.as_str()) {
                        "* "
                    } else {
                        "  "
                    };
                    println!("{mark}{} ({})", w.label, w.path);
                }
            }
        }
        TrayAction::Activate { path } => {
            set_active_workspace(&path)?;
            if json {
                output_json(&list_recent_workspaces()?)?;
            } else {
                println!("Active workspace: {}", path.display());
            }
        }
    }
    Ok(())
}
