use clap::Subcommand;
use feanorfs_client::{
    do_tray_status, forget_unavailable_workspaces, list_recent_workspaces, load_config,
    register_workspace, set_active_workspace, set_paused,
};
use feanorfs_common::TrayPauseResult;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use super::pair::{receive, PairCode};
use super::start::{run_start, StartOptions};
use super::util::output_json;

const MAX_PAIRING_STDIN_BYTES: u64 = 1024;
const MAX_JOIN_DECISION_BYTES: u64 = 32;

#[derive(serde::Serialize)]
struct JoinPreviewEvent<'a> {
    event: &'static str,
    preview: &'a feanorfs_client::JoinPreflight,
}

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
    /// Remove unavailable folders from the tray list without touching workspace data.
    ForgetUnavailable,
    /// Set the active workspace for the tray switcher.
    Activate {
        /// Workspace folder path
        path: std::path::PathBuf,
    },
    /// Join another computer from the bundled tray's bounded stdin capability.
    Join {
        /// New or unconfigured workspace folder
        folder: PathBuf,
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
        TrayAction::ForgetUnavailable => {
            let before = list_recent_workspaces()?.workspaces.len();
            let recent = forget_unavailable_workspaces()?;
            if json {
                output_json(&recent)?;
            } else {
                let removed = before.saturating_sub(recent.workspaces.len());
                println!(
                    "Removed {removed} unavailable workspace entr{} from the tray. No workspace data was changed.",
                    if removed == 1 { "y" } else { "ies" }
                );
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
        TrayAction::Join { folder } => {
            if json {
                anyhow::bail!("tray pairing join is interactive and does not support --json");
            }
            if folder.join(".feanorfs").join("config.json").exists() {
                anyhow::bail!(
                    "{} is already a FeanorFS workspace; choose a new or unconfigured folder",
                    folder.display()
                );
            }
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            let pair_code = read_pairing_code(&mut stdin)?;
            let token = receive(&pair_code, std::time::Duration::from_secs(20)).await?;
            let invite = feanorfs_client::decode_invite(token.as_str())?;
            drop(token);
            let preview = feanorfs_client::preview_join(&folder, &invite).await?;
            write_join_preview(&preview)?;
            if !read_join_decision(&mut stdin)? {
                anyhow::bail!("Join canceled. No FeanorFS setup or workspace files were changed.");
            }
            Box::pin(run_start(
                current_dir,
                StartOptions {
                    target: None,
                    folder: Some(folder),
                    workspace: None,
                    encryption_key: None,
                    server_token: None,
                    lan: false,
                    local: false,
                    host: false,
                    relay: None,
                    no_watch: false,
                    foreground: false,
                    accept_join: true,
                    recovery_invite: Some(invite),
                    pair_code: None,
                },
            ))
            .await?;
        }
    }
    Ok(())
}

fn write_join_preview(preview: &feanorfs_client::JoinPreflight) -> anyhow::Result<()> {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(
        &mut stdout,
        &JoinPreviewEvent {
            event: "join_preview",
            preview,
        },
    )?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

fn read_join_decision<R: BufRead>(reader: R) -> anyhow::Result<bool> {
    let mut limited = reader.take(MAX_JOIN_DECISION_BYTES + 1);
    let mut decision = String::new();
    let bytes = limited.read_line(&mut decision)?;
    if bytes == 0 || bytes as u64 > MAX_JOIN_DECISION_BYTES {
        anyhow::bail!("bundled tray did not provide a valid join decision");
    }
    Ok(decision.trim() == "CONFIRM")
}

fn read_pairing_code<R: BufRead>(reader: R) -> anyhow::Result<PairCode> {
    let mut limited = reader.take(MAX_PAIRING_STDIN_BYTES + 1);
    let mut input = Zeroizing::new(String::new());
    let bytes = limited
        .read_line(&mut input)
        .map_err(|error| anyhow::anyhow!("read pairing capability from tray: {error}"))?;
    if bytes == 0 {
        anyhow::bail!("bundled tray did not provide a pairing capability");
    }
    if bytes as u64 > MAX_PAIRING_STDIN_BYTES {
        anyhow::bail!("pairing capability input is too large");
    }
    while input.ends_with(['\r', '\n']) {
        input.pop();
    }
    if input.contains('\0') {
        anyhow::bail!("pairing capability contains an unsupported NUL character");
    }
    PairCode::parse(input.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_stdin_pairing_code_is_validated_and_canonicalized() {
        let code = read_pairing_code(Cursor::new(b"fnp1-2345-6789-abcd-efgh\n")).unwrap();
        assert_eq!(code.as_str(), "fnp1-2345-6789-ABCD-EFGH");
        assert!(read_pairing_code(Cursor::new(b"not-a-pairing-code\n")).is_err());
        assert!(read_pairing_code(Cursor::new(vec![
            b'x';
            MAX_PAIRING_STDIN_BYTES as usize + 1
        ]))
        .is_err());
    }
}
