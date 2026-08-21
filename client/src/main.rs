#[cfg(test)]
feanorfs_test_support::isolate_test_process!();

mod cli;

use clap::{Parser, Subcommand};
use cli::{
    setup_logging, AgentAction, ConflictsAction, HydrateAction, LoggingMode, SyncAction,
    WorkspaceAction,
};

#[derive(Parser)]
#[command(name = "feanorfs")]
#[command(version)]
#[command(about = "FeanorFS: end-to-end encrypted working-directory sync", long_about = None)]
struct Cli {
    /// Emit machine-readable JSON results instead of human prose.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(flatten)]
    Sync(SyncAction),
    #[command(flatten)]
    Hydrate(HydrateAction),
    #[command(flatten)]
    Workspace(WorkspaceAction),
    /// Isolated agent workspaces (copy-on-write snapshots).
    Agent {
        #[command(subcommand)]
        action: Option<AgentAction>,
    },
    /// Workspace sync conflicts needing attention.
    Conflicts {
        #[command(subcommand)]
        action: Option<ConflictsAction>,
    },
    /// Show immutable workspace snapshot history.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Restore a historical snapshot as a new snapshot.
    Undo { snapshot_id: String },
    /// Check the official stable release without downloading or installing it.
    Update {
        /// Respect the per-machine throttle: reuse the last result until the
        /// interval elapses (used by the tray and scheduled checks).
        #[arg(long)]
        periodic: bool,
    },
}

fn logging_mode(command: &Commands) -> LoggingMode {
    let Commands::Workspace(WorkspaceAction::Tray { action }) = command else {
        return LoggingMode::Standard;
    };
    match action {
        cli::tray::TrayAction::Recent
        | cli::tray::TrayAction::ForgetUnavailable
        | cli::tray::TrayAction::Activate { .. }
        | cli::tray::TrayAction::Join { .. } => LoggingMode::TrayGlobal,
        cli::tray::TrayAction::Status { .. }
        | cli::tray::TrayAction::Overview
        | cli::tray::TrayAction::Pause
        | cli::tray::TrayAction::Resume
        | cli::tray::TrayAction::Register => LoggingMode::TrayWorkspace,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let current_dir = std::env::current_dir()?;

    if let Err(e) = setup_logging(&current_dir, logging_mode(&cli.command)) {
        eprintln!("Warning: failed to initialize log file: {e:?}");
    }

    match cli.command {
        Commands::Sync(action) => cli::sync::run(&current_dir, action, cli.json).await?,
        Commands::Hydrate(action) => cli::hydrate::run(&current_dir, action, cli.json).await?,
        Commands::Workspace(action) => cli::workspace::run(&current_dir, action, cli.json).await?,
        Commands::Agent { action } => {
            let action = action.unwrap_or(AgentAction::Status { name: None });
            cli::agent::run(&current_dir, action, cli.json).await?
        }
        Commands::Conflicts { action } => {
            let action = action.unwrap_or(ConflictsAction::List);
            cli::conflicts::run(&current_dir, action, cli.json).await?
        }
        Commands::Log { limit } => cli::history::log(&current_dir, limit, cli.json).await?,
        Commands::Undo { snapshot_id } => {
            cli::history::undo(&current_dir, &snapshot_id, cli.json).await?
        }
        Commands::Update { periodic } => cli::update::run(cli.json, periodic).await?,
    }

    Ok(())
}

use feanorfs_client::{open_api_client, open_client_db};

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn parsed_logging_mode(args: &[&str]) -> LoggingMode {
        let cli = Cli::try_parse_from(args).unwrap();
        logging_mode(&cli.command)
    }

    #[test]
    fn tray_commands_select_latency_safe_logging_modes() {
        for args in [
            &["feanorfs", "tray", "recent"][..],
            &["feanorfs", "tray", "forget-unavailable"][..],
            &["feanorfs", "tray", "activate", "/configured/workspace"][..],
            &["feanorfs", "tray", "join", "/new/workspace"][..],
        ] {
            assert_eq!(parsed_logging_mode(args), LoggingMode::TrayGlobal);
        }

        for args in [
            &["feanorfs", "tray", "status"][..],
            &["feanorfs", "tray", "overview"][..],
            &["feanorfs", "tray", "pause"][..],
            &["feanorfs", "tray", "resume"][..],
            &["feanorfs", "tray", "register"][..],
        ] {
            assert_eq!(parsed_logging_mode(args), LoggingMode::TrayWorkspace);
        }

        assert_eq!(
            parsed_logging_mode(&["feanorfs", "update", "--periodic"]),
            LoggingMode::Standard
        );
    }

    #[test]
    fn cli_debug_asserts_pass() {
        Cli::try_parse_from(["feanorfs", "start"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "https://x:3030"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "fnr1-deadbeef"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "fnp1-2345-6789-ABCD-EFGH"]).unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "agent",
            "integrator",
            "assign",
            "--about",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--candidate",
            "agent-a",
            "--require",
            "rust",
            "--ack-timeout",
            "5m",
            "Integrate parser tests",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "agent",
            "integrator",
            "status",
            "0123456789abcdef0123456789abcdef",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "agent",
            "integrator",
            "revoke",
            "0123456789abcdef0123456789abcdef",
            "--reason",
            "integrator went quiet",
        ])
        .unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "integrator", "resume"]).unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "agent",
            "runner",
            "setup",
            "worker",
            "--timeout",
            "120",
            "--",
            "/usr/bin/env",
            "python3",
            "--fixed-flag",
        ])
        .unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "runner", "start"]).unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "runner", "start", "--foreground"]).unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "runner", "stop"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "agent", "runner", "status"]).unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "runner", "reset", "--discard-pending"]).unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "runner", "remove", "--discard-pending"])
            .unwrap();
        assert!(Cli::try_parse_from([
            "feanorfs",
            "agent",
            "runner",
            "setup",
            "worker",
            "/usr/bin/env",
        ])
        .is_err());
        assert!(Cli::try_parse_from(["feanorfs", "agent", "runner", "reset"]).is_err());
        assert!(Cli::try_parse_from(["feanorfs", "agent", "runner", "remove"]).is_err());
        Cli::try_parse_from([
            "feanorfs",
            "agent",
            "integrator",
            "resume",
            "--ack-timeout",
            "60s",
            "--fallback-on-blocked",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "conflicts",
            "materialize",
            "--about",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--path",
            "src/main.rs",
        ])
        .unwrap();
        Cli::try_parse_from(["feanorfs", "conflicts", "materialize"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "fnh1-deadbeef"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "fnr1-deadbeef", "/tmp/workspace"]).unwrap();
        Cli::try_parse_from(["feanorfs", "sync", "--no-watch"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "--foreground"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "--host", "/tmp/workspace"]).unwrap();
        Cli::try_parse_from(["feanorfs", "stop"]).unwrap();
        Cli::try_parse_from(["feanorfs", "stop", "--", "/tmp/workspace"]).unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "transfer-hub",
            "--source-url",
            "http://127.0.0.1:3031",
            "/tmp/destination",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "start",
            "--workspace",
            "team-app",
            "/tmp/workspace",
        ])
        .unwrap();
        Cli::try_parse_from(["feanorfs", "service", "status"]).unwrap();
        Cli::try_parse_from(["feanorfs", "service", "hub-run", "/tmp/hub"]).unwrap();
        Cli::try_parse_from(["feanorfs", "service", "runner-run", "/tmp/workspace"]).unwrap();
        Cli::try_parse_from(["feanorfs", "service", "supervise"]).unwrap();
        Cli::try_parse_from(["feanorfs", "pair"]).unwrap();
        Cli::try_parse_from(["feanorfs", "pair", "--tray", "--expires", "300"]).unwrap();
        Cli::try_parse_from(["feanorfs", "pair", "--relay", "https://relay.example"]).unwrap();
        Cli::try_parse_from(["feanorfs", "recovery", "export", "/tmp/workspace.fnrk"]).unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "recovery",
            "import",
            "/tmp/workspace.fnrk",
            "/tmp/restored",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "serve",
            "recovery",
            "export",
            "/tmp/hub.recovery",
            "--data-dir",
            "/tmp/hub",
        ])
        .unwrap();
        Cli::try_parse_from(["feanorfs", "serve", "--pair-relay"]).unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "serve",
            "recovery",
            "import",
            "/tmp/hub.recovery",
            "--replace",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "serve",
            "recovery",
            "rotate",
            "/tmp/new-hub.recovery",
            "--data-dir",
            "/tmp/hub",
        ])
        .unwrap();
        assert!(Cli::try_parse_from(["feanorfs", "start", "--foreground", "--no-watch"]).is_err());
        assert!(Cli::try_parse_from(["feanorfs", "start", "--host", "--local"]).is_err());
        assert!(Cli::try_parse_from(["feanorfs", "start", "--encryption-key", "a-key"]).is_err());
        Cli::try_parse_from(["feanorfs", "agent"]).unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "status"]).unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "agent",
            "send",
            "mac-test",
            "--kind",
            "request",
            "--about",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "Run iOS simulator tests",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "--json",
            "agent",
            "send",
            "*",
            "--kind",
            "blocked",
            "--reply-to",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--from",
            "ci1",
            "cannot reach snapshot",
        ])
        .unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "agent",
            "inbox",
            "--for",
            "mac-test",
            "--after",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--limit",
            "100",
        ])
        .unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "agent", "inbox"]).unwrap();
        assert!(Cli::try_parse_from(["feanorfs", "agent", "send", "mac-test", "no kind"]).is_err());
        Cli::try_parse_from(["feanorfs", "conflicts"]).unwrap();
        Cli::try_parse_from(["feanorfs", "doctor", "--migration-report"]).unwrap();
        Cli::try_parse_from(["feanorfs", "folders"]).unwrap();
        Cli::try_parse_from(["feanorfs", "ignore"]).unwrap();
        Cli::try_parse_from(["feanorfs", "ignore", "server-data/"]).unwrap();
        Cli::try_parse_from(["feanorfs", "ignore", "--remove", "server-data/"]).unwrap();
        Cli::try_parse_from(["feanorfs", "ignore", "--clear"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "folders"]).unwrap();
        Cli::try_parse_from(["feanorfs", "update"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "update"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "update", "--periodic"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "doctor", "--migration-report"]).unwrap();
        Cli::try_parse_from(["feanorfs", "setup", "--workspace", "w"]).unwrap();
        Cli::try_parse_from(["feanorfs", "init", "127.0.0.1:3030", "--workspace", "w"]).unwrap();
    }

    /// The tray app shells these exact argument shapes (`--` before untrusted values).
    #[test]
    fn cli_parses_tray_subprocess_shapes() {
        let parsed = Cli::try_parse_from(["feanorfs", "start", "--", "/tmp/workspace"])
            .expect("tray folder setup shape must remain accepted");
        match parsed.command {
            Commands::Workspace(WorkspaceAction::Start { target, folder, .. }) => {
                assert_eq!(target.as_deref(), Some("/tmp/workspace"));
                assert!(folder.is_none());
            }
            _ => panic!("expected workspace start command"),
        }
        let parsed = Cli::try_parse_from(["feanorfs", "--json", "start", "--", "/tmp/workspace"])
            .expect("tray typed setup shape must remain accepted");
        match parsed.command {
            Commands::Workspace(WorkspaceAction::Start { target, folder, .. }) => {
                assert_eq!(target.as_deref(), Some("/tmp/workspace"));
                assert!(folder.is_none());
            }
            _ => panic!("expected workspace start command"),
        }
        Cli::try_parse_from(["feanorfs", "--json", "tray", "status"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "tray", "overview"]).unwrap();
        Cli::try_parse_from(["feanorfs", "tray", "pause"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "tray", "pause"]).unwrap();
        Cli::try_parse_from(["feanorfs", "tray", "resume"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "tray", "recent"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "tray", "forget-unavailable"]).unwrap();
        Cli::try_parse_from(["feanorfs", "tray", "activate", "--", "/tmp/ws"]).unwrap();
        Cli::try_parse_from(["feanorfs", "tray", "join", "--", "/tmp/joined"]).unwrap();
        Cli::try_parse_from(["feanorfs", "service", "refresh-installation"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "stop", "--", "/tmp/ws"]).unwrap();
        Cli::try_parse_from([
            "feanorfs",
            "conflicts",
            "keep",
            "--local",
            "--",
            "--weird.txt",
        ])
        .unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "land", "--", "-name"]).unwrap();
        Cli::try_parse_from(["feanorfs", "sync", "--no-watch"]).unwrap();
    }
}
