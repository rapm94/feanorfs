use clap::Subcommand;
use feanorfs_client::{
    check_agent, clean_agent, invalidate_agent_cache, land_agent, list_agents, load_config,
    refresh_agent_with_options, spawn_agent, AgentCleanResult, AgentListEntry,
    AgentListOfflineResult, AgentListResult, ApiClient, ClientDb, RefreshOptions, SpawnResult,
};
use std::path::Path;

use super::util::output_json;

#[derive(Subcommand)]
pub enum AgentAction {
    /// List agents or preview one agent's changes (read-only)
    Status {
        /// Agent name. If omitted, lists all agents with a one-line summary.
        name: Option<String>,
    },
    /// Spawn a new isolated agent workspace `.feanorfs/agents/<name>/`.
    Spawn {
        name: String,
        /// Skip pre-spawn sync (requires folder to match last synced state).
        #[arg(long)]
        no_sync: bool,
        /// Replace an existing agent with the same name.
        #[arg(long)]
        replace: bool,
    },
    /// Integrate agent work into your folder (applies clean changes, registers conflicts).
    #[command(alias = "commit", hide = true)]
    Land {
        name: String,
        /// Remove agent workspace after a successful land.
        #[arg(long)]
        clean: bool,
        /// Write diff3 `.proposed` artifacts for conflicts (never auto-applied).
        #[arg(long)]
        propose: bool,
    },
    /// Pull cloud changes into the agent for paths the agent hasn't edited.
    Refresh {
        name: String,
        #[arg(long)]
        replace: bool,
    },
    /// Remove an agent workspace and its snapshot rows.
    Clean { name: String },
    /// Run a command with the agent workspace as its working directory.
    Run {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Preview agent changes (legacy — prefer `agent status <name>`)
    #[command(hide = true)]
    Check { name: String },
    /// List agent workspaces (legacy — prefer `agent status`)
    #[command(hide = true)]
    List,
}

pub async fn run(current_dir: &Path, action: AgentAction, json: bool) -> anyhow::Result<()> {
    match action {
        AgentAction::Status { name: Some(name) } | AgentAction::Check { name } => {
            run_agent_check(current_dir, &name, json).await?
        }
        AgentAction::Status { name: None } => run_agent_status_list(current_dir, json).await?,
        AgentAction::List => run_agent_list_legacy(current_dir, json).await?,
        AgentAction::Spawn {
            name,
            no_sync,
            replace,
        } => {
            let config = load_config(current_dir)?;
            let db = crate::open_client_db(current_dir).await?;
            let api = crate::open_api_client(current_dir, &config).await?;
            let count = spawn_agent(
                current_dir,
                &db,
                &api,
                &config.workspace_id,
                &name,
                config.encryption_password.as_deref(),
                no_sync,
                replace,
            )
            .await?;
            if json {
                output_json(&SpawnResult {
                    agent: name.clone(),
                    files_copied: count,
                })?;
            } else {
                println!("Agent '{name}' spawned with {count} files at .feanorfs/agents/{name}/");
            }
        }
        AgentAction::Land {
            name,
            clean,
            propose,
        } => {
            let config = load_config(current_dir)?;
            let db = crate::open_client_db(current_dir).await?;
            let api = crate::open_api_client(current_dir, &config).await?;
            let result = land_agent(
                current_dir,
                &db,
                &api,
                &config.workspace_id,
                &name,
                config.encryption_password.as_deref(),
                clean,
                propose,
            )
            .await?;
            invalidate_agent_cache(current_dir);
            if json {
                output_json(&result)?;
            } else {
                println!("{}", result.message);
            }
        }
        AgentAction::Refresh { name, replace } => {
            let config = load_config(current_dir)?;
            let db = crate::open_client_db(current_dir).await?;
            let api = crate::open_api_client(current_dir, &config).await?;
            let result = refresh_agent_with_options(
                current_dir,
                &db,
                &api,
                &config.workspace_id,
                &name,
                config.encryption_password.as_deref(),
                RefreshOptions { replace },
            )
            .await?;
            if json {
                output_json(&result)?;
            } else {
                println!("Refreshed: {:?}", result.refreshed);
            }
        }
        AgentAction::Clean { name } => {
            let db = crate::open_client_db(current_dir).await?;
            clean_agent(current_dir, &db, &name).await?;
            if json {
                output_json(&AgentCleanResult { cleaned: name })?;
            } else {
                println!("Agent '{name}' removed.");
            }
        }
        AgentAction::Run { name, command } => {
            if command.is_empty() {
                anyhow::bail!("`agent run` requires a command after `--`");
            }
            feanorfs_client::agent::validate_name(&name)?;
            let agent_path = feanorfs_client::agent::agent_dir(current_dir, &name);
            if !agent_path.exists() {
                anyhow::bail!(
                    "Agent workspace '{name}' not found. Run `feanorfs agent spawn {name}` first."
                );
            }
            let agent_dir_abs = agent_path.canonicalize().unwrap_or(agent_path.clone());
            let mut cmd = std::process::Command::new(&command[0]);
            cmd.args(&command[1..])
                .current_dir(&agent_path)
                .env("FEANORFS_AGENT", &name)
                .env("FEANORFS_AGENT_DIR", agent_dir_abs);
            let status = cmd.status()?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    Ok(())
}

async fn run_agent_check(current_dir: &Path, name: &str, json: bool) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let result = check_agent(
        current_dir,
        &db,
        &api,
        &config.workspace_id,
        name,
        config.encryption_password.as_deref(),
    )
    .await?;
    if json {
        output_json(&result)?;
    } else {
        println!("Agent '{name}':");
        println!("  Changes to land: {}", result.our_changes.len());
        println!("  Cloud changes:   {}", result.their_changes.len());
        println!("  Needs attention: {}", result.conflicts.len());
        if !result.conflict_risk.is_empty() {
            println!("  Consider refresh: {}", result.conflict_risk.join(", "));
        }
        if !result.conflicts.is_empty() {
            println!("  Conflicting paths:");
            for c in &result.conflicts {
                println!("    ! {}", c.path);
            }
        }
    }
    Ok(())
}

async fn run_agent_list_legacy(current_dir: &Path, json: bool) -> anyhow::Result<()> {
    let db = crate::open_client_db(current_dir).await?;
    let names = list_agents(current_dir, &db).await?;
    if json {
        output_json(&AgentListOfflineResult { agents: names })?;
    } else if names.is_empty() {
        println!("No agent workspaces.");
    } else {
        for n in &names {
            println!("  * {n}");
        }
    }
    Ok(())
}

async fn agent_one_line_state(
    current_dir: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    password: Option<&str>,
    name: &str,
) -> String {
    match check_agent(current_dir, db, api, workspace_id, name, password).await {
        Ok(check) => {
            if !check.conflicts.is_empty() {
                format!("{} conflict(s)", check.conflicts.len())
            } else if !check.our_changes.is_empty() {
                format!("{} change(s)", check.our_changes.len())
            } else {
                "clean".into()
            }
        }
        Err(_) => "(offline)".into(),
    }
}

async fn run_agent_status_list(current_dir: &Path, json: bool) -> anyhow::Result<()> {
    let db = crate::open_client_db(current_dir).await?;
    let names = list_agents(current_dir, &db).await?;

    let enriched = match load_config(current_dir) {
        Ok(config) => match crate::open_api_client(current_dir, &config).await {
            Ok(api) => Some((config, api)),
            Err(_) => None,
        },
        Err(_) => None,
    };

    if json {
        if let Some((config, api)) = enriched {
            let mut agents = Vec::new();
            for name in &names {
                let state = agent_one_line_state(
                    current_dir,
                    &db,
                    &api,
                    &config.workspace_id,
                    config.encryption_password.as_deref(),
                    name,
                )
                .await;
                agents.push(AgentListEntry {
                    name: name.clone(),
                    state,
                });
            }
            output_json(&AgentListResult { agents })?;
        } else {
            output_json(&AgentListOfflineResult { agents: names })?;
        }
    } else if names.is_empty() {
        println!("No agent workspaces.");
    } else if let Some((config, api)) = enriched {
        for name in &names {
            let state = agent_one_line_state(
                current_dir,
                &db,
                &api,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                name,
            )
            .await;
            println!("  {name}: {state}");
        }
    } else {
        for name in &names {
            println!("  {name}");
        }
    }
    Ok(())
}
