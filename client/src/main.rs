mod cli;

use clap::{Parser, Subcommand};
use cli::{
    setup_logging, AgentAction, ConflictsAction, HydrateAction, SyncAction, WorkspaceAction,
};

#[derive(Parser)]
#[command(name = "feanorfs")]
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let current_dir = std::env::current_dir()?;

    if let Err(e) = setup_logging(&current_dir) {
        eprintln!("Warning: failed to initialize log file: {:?}", e);
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
    }

    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn cli_debug_asserts_pass() {
        Cli::try_parse_from(["feanorfs", "start"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "https://x:3030"]).unwrap();
        Cli::try_parse_from(["feanorfs", "start", "fnr1-deadbeef"]).unwrap();
        Cli::try_parse_from(["feanorfs", "sync", "--no-watch"]).unwrap();
        Cli::try_parse_from(["feanorfs", "agent"]).unwrap();
        Cli::try_parse_from(["feanorfs", "agent", "status"]).unwrap();
        Cli::try_parse_from(["feanorfs", "conflicts"]).unwrap();
        Cli::try_parse_from(["feanorfs", "setup", "--workspace", "w"]).unwrap();
        Cli::try_parse_from(["feanorfs", "init", "127.0.0.1:3030", "--workspace", "w"]).unwrap();
    }

    /// The tray app shells these exact argument shapes (`--` before untrusted values).
    #[test]
    fn cli_parses_tray_subprocess_shapes() {
        Cli::try_parse_from(["feanorfs", "--json", "tray", "status"]).unwrap();
        Cli::try_parse_from(["feanorfs", "tray", "pause"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "tray", "pause"]).unwrap();
        Cli::try_parse_from(["feanorfs", "tray", "resume"]).unwrap();
        Cli::try_parse_from(["feanorfs", "--json", "tray", "recent"]).unwrap();
        Cli::try_parse_from(["feanorfs", "tray", "activate", "--", "/tmp/ws"]).unwrap();
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
