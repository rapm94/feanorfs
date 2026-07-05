use feanorfs_client::{commit_agent, load_config, spawn_agent, ApiClient, ClientDb};
use std::path::Path;

use super::util::output_json;
use super::AgentAction;

pub async fn run(current_dir: &Path, action: AgentAction, json: bool) -> anyhow::Result<()> {
    match action {
        AgentAction::Spawn { name } => {
            let config = load_config(current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let count = spawn_agent(
                current_dir,
                &db,
                &name,
                config.encryption_password.as_deref(),
            )
            .await?;
            if json {
                output_json(&serde_json::json!({
                    "agent": name,
                    "files_copied": count,
                }))?;
            } else {
                println!(
                    "Agent '{}' spawned with {} files (copied) at .feanorfs/agents/{}/",
                    name, count, name
                );
            }
        }
        AgentAction::Commit { name } | AgentAction::Check { name } => {
            let config = load_config(current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());
            let result = commit_agent(
                current_dir,
                &db,
                &api,
                &config.workspace_id,
                &name,
                config.encryption_password.as_deref(),
            )
            .await?;

            if json {
                output_json(&result)?;
            } else {
                println!("Agent '{}' commit:", name);
                println!("  Our changes:    {}", result.our_changes.len());
                println!("  Their changes: {}", result.their_changes.len());
                println!("  Conflicts:     {}", result.conflicts.len());
                if !result.conflicts.is_empty() {
                    println!(
                        "\nConflicting paths (look in .feanorfs/conflicts/ for base/ours/theirs):"
                    );
                    for c in &result.conflicts {
                        println!("  ! {}", c.path);
                    }
                }
            }
        }
        AgentAction::List => {
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let names = feanorfs_client::list_agents(current_dir, &db).await?;
            if json {
                output_json(&serde_json::json!({ "agents": names }))?;
            } else if names.is_empty() {
                println!("No agent workspaces.");
            } else {
                println!("Agent workspaces:");
                for n in &names {
                    println!("  * {}", n);
                }
            }
        }
        AgentAction::Clean { name } => {
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            feanorfs_client::clean_agent(current_dir, &db, &name).await?;
            if json {
                output_json(&serde_json::json!({ "cleaned": name }))?;
            } else {
                println!("Agent '{}' removed.", name);
            }
        }
        AgentAction::Run { name, command } => {
            if command.is_empty() {
                anyhow::bail!(
                    "`agent run` requires a command after `--`. Example: feanorfs agent run ci -- cargo test"
                );
            }
            feanorfs_client::agent::validate_name(&name)?;
            let agent_path = feanorfs_client::agent::agent_dir(current_dir, &name);
            if !agent_path.exists() {
                anyhow::bail!(
                    "Agent workspace '{}' not found. Run `feanorfs agent spawn {}` first.",
                    name,
                    name
                );
            }
            let mut cmd = std::process::Command::new(&command[0]);
            cmd.args(&command[1..]).current_dir(&agent_path);
            let status = cmd.status()?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    Ok(())
}
