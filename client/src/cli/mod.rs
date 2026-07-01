pub mod agent;
pub mod conflicts;
pub mod util;

pub use util::{
    copy_to_clipboard, initialize_new_mirror, link_existing_mirror, output_json, probe_server_auth,
    read_password_hidden, resolve_server_url, setup_logging, truncate_password_for_display,
};

use clap::Subcommand;

#[derive(Subcommand)]
pub enum AgentAction {
    /// Spawn a new isolated agent workspace `.feanorfs/agents/<name>/`.
    Spawn { name: String },
    /// Diff agent workspace against base snapshot and split into clean-our / clean-their / conflicts.
    Commit { name: String },
    /// Alias for `commit`.
    Check { name: String },
    /// List all spawned agent workspaces.
    List,
    /// Remove an agent workspace and its snapshot rows.
    Clean { name: String },
    /// Run a command with the agent workspace as its working directory.
    Run {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ConflictsAction {
    /// List pending conflicts (paths blocked from LWW sync).
    List,
    /// Resolve a conflict by keeping ours, theirs, or both.
    Resolve {
        path: String,
        #[arg(long, value_parser = ["ours", "theirs", "both"])]
        keep: String,
    },
}
