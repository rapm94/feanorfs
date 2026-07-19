use anyhow::Context as _;
use clap::Subcommand;
use std::io::{BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use super::start::{run_start, StartOptions};
use super::util::invite_from_config;

const MAX_STDIN_PASSPHRASE_BYTES: u64 = 4096;

#[derive(Subcommand)]
pub enum RecoveryAction {
    /// Save this workspace's complete capability in an encrypted offline kit
    Export {
        /// Destination `.fnrk` file
        destination: PathBuf,
        /// Replace an existing kit atomically
        #[arg(long)]
        replace: bool,
        /// Read one passphrase line from stdin (bundled tray only)
        #[arg(long, hide = true)]
        passphrase_stdin: bool,
    },
    /// Restore a workspace through the ordinary secure `start` flow
    Import {
        /// Encrypted `.fnrk` file
        source: PathBuf,
        /// Folder to restore (default: current directory)
        folder: Option<PathBuf>,
        /// Sync once without installing automatic background sync
        #[arg(long, conflicts_with = "foreground")]
        no_watch: bool,
        /// Keep sync attached to this terminal
        #[arg(long)]
        foreground: bool,
        /// Read one passphrase line from stdin (bundled tray only)
        #[arg(long, hide = true)]
        passphrase_stdin: bool,
    },
}

pub async fn run(current_dir: &Path, action: RecoveryAction, json: bool) -> anyhow::Result<()> {
    if json {
        anyhow::bail!("workspace recovery is interactive and does not support --json");
    }

    match action {
        RecoveryAction::Export {
            destination,
            replace,
            passphrase_stdin,
        } => {
            let config = feanorfs_client::load_config(current_dir)
                .context("open a mirrored folder before exporting its recovery kit")?;
            let invite =
                invite_from_config(&config).context("workspace has no recoverable E2EE key")?;
            let invite = super::hub_service::portable_invite(invite);
            let passphrase = if passphrase_stdin {
                read_passphrase_stdin()?
            } else {
                read_new_passphrase()?
            };
            feanorfs_client::export_recovery_kit(
                &destination,
                &invite,
                passphrase.as_str(),
                replace,
            )?;
            println!("Encrypted recovery kit saved to {}.", destination.display());
            println!("Keep the kit and its passphrase in separate safe places.");
            Ok(())
        }
        RecoveryAction::Import {
            source,
            folder,
            no_watch,
            foreground,
            passphrase_stdin,
        } => {
            let destination = folder.as_deref().unwrap_or(current_dir);
            if feanorfs_agent_core::workspace_is_configured(destination) {
                anyhow::bail!(
                    "{} is already a FeanorFS workspace; choose a new or unconfigured folder",
                    destination.display()
                );
            }

            let passphrase = if passphrase_stdin {
                read_passphrase_stdin()?
            } else {
                Zeroizing::new(rpassword::prompt_password("Recovery kit passphrase: ")?)
            };
            let invite = feanorfs_client::open_recovery_kit(&source, passphrase.as_str())?;
            drop(passphrase);
            println!("Recovery kit authenticated. Restoring encrypted workspace…");
            // `run_start` composes the complete onboarding, sync, and service
            // state machines. Keep that large future off the Windows main
            // thread's smaller stack when recovery adds another async layer.
            Box::pin(run_start(
                current_dir,
                StartOptions {
                    target: None,
                    folder,
                    workspace: None,
                    encryption_key: None,
                    server_token: None,
                    lan: false,
                    local: false,
                    host: false,
                    relay: None,
                    no_watch,
                    foreground,
                    accept_join: false,
                    recovery_invite: Some(invite),
                    pair_code: None,
                },
            ))
            .await
        }
    }
}

fn read_new_passphrase() -> anyhow::Result<Zeroizing<String>> {
    let passphrase = Zeroizing::new(rpassword::prompt_password(
        "New recovery passphrase (12+ characters): ",
    )?);
    let confirmation = Zeroizing::new(rpassword::prompt_password("Confirm passphrase: ")?);
    if passphrase.as_str() != confirmation.as_str() {
        anyhow::bail!("recovery passphrases do not match");
    }
    Ok(passphrase)
}

fn read_passphrase_stdin() -> anyhow::Result<Zeroizing<String>> {
    let stdin = std::io::stdin();
    let mut limited = stdin.lock().take(MAX_STDIN_PASSPHRASE_BYTES + 1);
    let mut passphrase = Zeroizing::new(String::new());
    let bytes = limited
        .read_line(&mut passphrase)
        .context("read recovery passphrase from tray")?;
    if bytes == 0 {
        anyhow::bail!("bundled tray did not provide a recovery passphrase");
    }
    if bytes as u64 > MAX_STDIN_PASSPHRASE_BYTES {
        anyhow::bail!("recovery passphrase input is too large");
    }
    while passphrase.ends_with(['\r', '\n']) {
        passphrase.pop();
    }
    if passphrase.contains('\0') {
        anyhow::bail!("recovery passphrase contains an unsupported NUL character");
    }
    Ok(passphrase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_actions_keep_passphrases_out_of_argv() {
        use clap::Parser as _;

        #[derive(clap::Parser)]
        struct Harness {
            #[command(subcommand)]
            action: RecoveryAction,
        }

        let export = Harness::try_parse_from([
            "recovery",
            "export",
            "--passphrase-stdin",
            "--",
            "/tmp/workspace.fnrk",
        ])
        .unwrap();
        assert!(matches!(
            export.action,
            RecoveryAction::Export {
                passphrase_stdin: true,
                ..
            }
        ));
        let import = Harness::try_parse_from([
            "recovery",
            "import",
            "--passphrase-stdin",
            "--",
            "/tmp/workspace.fnrk",
            "/tmp/restored",
        ])
        .unwrap();
        assert!(matches!(
            import.action,
            RecoveryAction::Import {
                passphrase_stdin: true,
                ..
            }
        ));
    }
}
