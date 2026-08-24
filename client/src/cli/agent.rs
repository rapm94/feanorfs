use clap::{Subcommand, ValueEnum};
use feanorfs_client::{
    check_agent, clean_agent, invalidate_agent_cache, land_agent, list_agents, load_config,
    refresh_agent_with_options, spawn_agent, AgentCleanResult, AgentInboxQuery, AgentListEntry,
    AgentListOfflineResult, AgentListResult, AgentMessageInput, AgentMessageKind, ApiClient,
    ClientDb, RefreshOptions, SpawnResult,
};
use std::path::{Path, PathBuf};

use super::util::{output_json, terminal_line};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MessageKindArg {
    Request,
    Status,
    Result,
    Blocked,
}

impl From<MessageKindArg> for AgentMessageKind {
    fn from(value: MessageKindArg) -> Self {
        match value {
            MessageKindArg::Request => AgentMessageKind::Request,
            MessageKindArg::Status => AgentMessageKind::Status,
            MessageKindArg::Result => AgentMessageKind::Result,
            MessageKindArg::Blocked => AgentMessageKind::Blocked,
        }
    }
}

struct SendSignalArgs {
    recipient: String,
    kind: MessageKindArg,
    about: Option<String>,
    reply_to: Option<String>,
    from: Option<String>,
    body: String,
}

#[derive(Subcommand)]
pub enum AgentAction {
    /// List agents or preview one agent's changes (read-only)
    Status {
        /// Agent name. If omitted, lists all agents with a one-line summary.
        name: Option<String>,
    },
    /// Spawn a new isolated agent workspace under global FeanorFS state.
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
    /// Send an encrypted agent signal tied to a snapshot (no file changes).
    Send {
        /// Recipient agent name, or `*` to broadcast.
        recipient: String,
        /// Signal kind: request, status, result, or blocked.
        #[arg(long)]
        kind: MessageKindArg,
        /// Snapshot this signal concerns; defaults to the current workspace head.
        #[arg(long)]
        about: Option<String>,
        /// Signal snapshot being answered.
        #[arg(long)]
        reply_to: Option<String>,
        /// Explicit sender for controlled automation; otherwise FEANORFS_AGENT or human.
        #[arg(long)]
        from: Option<String>,
        /// Message body (bounded at 8 KiB).
        body: String,
    },
    /// Read encrypted agent signals addressed to you.
    Inbox {
        /// Recipient identity; defaults to FEANORFS_AGENT or human.
        #[arg(long = "for")]
        for_recipient: Option<String>,
        /// Previous inbox cursor (workspace head) to read the delta after.
        #[arg(long)]
        after: Option<String>,
        /// Bounded result count; default 50, maximum 1000.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Random integrator assignment (dispatcher-side orchestration).
    Integrator {
        #[command(subcommand)]
        action: super::integrator::IntegratorAction,
    },
    /// Encrypted work-intent coordination (propose, decide, amend, yield,
    /// settle, complete, block, status).
    Work {
        #[command(subcommand)]
        action: super::work::WorkAction,
    },
    /// Exact-fingerprint automatic conflict resolution (prepare, status,
    /// submit, apply). Submit never applies; apply revalidates every
    /// identity field immediately before a single CAS.
    Resolution {
        #[command(subcommand)]
        action: super::resolution::ResolutionAction,
    },
    /// Configure and control the workspace's unattended agent runner.
    Runner {
        #[command(subcommand)]
        action: super::runner::RunnerAction,
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
        AgentAction::Integrator { action } => {
            super::integrator::run(current_dir, action, json).await?
        }
        AgentAction::Work { action } => super::work::run(current_dir, action, json).await?,
        AgentAction::Resolution { action } => {
            super::resolution::run(current_dir, action, json).await?
        }
        AgentAction::Runner { action } => {
            let control_root = control_workspace_root(current_dir)?;
            super::runner::run(&control_root, action, json).await?
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
                let path = feanorfs_agent_core::agent_dir(current_dir, &name)?;
                println!(
                    "Agent '{name}' spawned with {count} files at {}",
                    path.display()
                );
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
                if !result.deferred.is_empty() {
                    println!("Deferred overlapping paths: {:?}", result.deferred);
                    println!(
                        "Reconcile those agent-local edits manually, or use `agent refresh {name} --replace` only to discard them in favor of the shared head."
                    );
                }
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
            let workspace_root = current_dir.canonicalize().map_err(|error| {
                anyhow::anyhow!(
                    "Could not resolve shared workspace root '{}': {error}",
                    current_dir.display()
                )
            })?;
            if !workspace_root.is_dir() {
                anyhow::bail!(
                    "Shared workspace root '{}' is not a directory.",
                    workspace_root.display()
                );
            }
            let agent_path = feanorfs_client::agent::agent_dir(current_dir, &name)?;
            if !agent_path.exists() {
                anyhow::bail!(
                    "Agent workspace '{name}' not found. Run `feanorfs agent spawn {name}` first."
                );
            }
            let outcome =
                super::agent_live::run_agent_interactive(current_dir, &name, &command).await?;
            if json {
                output_json(&outcome)?;
            } else {
                eprintln!("{}", render_live_outcome(&name, &outcome));
            }
            if let Some(code) = outcome.child_exit {
                if code != 0 {
                    std::process::exit(code);
                }
            }
        }
        AgentAction::Send {
            recipient,
            kind,
            about,
            reply_to,
            from,
            body,
        } => {
            run_agent_send(
                current_dir,
                json,
                SendSignalArgs {
                    recipient,
                    kind,
                    about,
                    reply_to,
                    from,
                    body,
                },
            )
            .await?
        }
        AgentAction::Inbox {
            for_recipient,
            after,
            limit,
        } => {
            run_agent_inbox(
                current_dir,
                json,
                for_recipient.as_deref(),
                after.as_deref(),
                limit,
            )
            .await?
        }
    }
    Ok(())
}
/// Human-readable continuous-run summary: settled, offline, or attention.
fn render_live_outcome(name: &str, outcome: &super::agent_live::LiveFinalOutcome) -> String {
    let settled = outcome
        .settled_snapshot
        .as_deref()
        .map(|id| format!(" (snapshot {id})"))
        .unwrap_or_default();
    if let Some(attention) = &outcome.attention {
        format!(
            "Agent '{name}' needs attention ({reason}): {detail}",
            reason = terminal_line(&attention.reason),
            detail = terminal_line(&attention.detail)
        )
    } else if outcome.offline {
        format!(
            "Agent '{name}' finished offline: changes are preserved. Run `feanorfs agent run {name} -- <command>` again when the hub is reachable to reconcile them."
        )
    } else if outcome.settled {
        format!("Agent '{name}' settled{settled}.")
    } else {
        format!("Agent '{name}' stopped with pending work{settled}.")
    }
}

fn agent_sender(explicit: Option<String>) -> String {
    explicit
        .or_else(|| std::env::var("FEANORFS_AGENT").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "human".to_string())
}

pub(super) fn control_workspace_root(current_dir: &Path) -> anyhow::Result<PathBuf> {
    let Some(value) = std::env::var_os("FEANORFS_WORKSPACE_ROOT") else {
        return Ok(current_dir.to_path_buf());
    };
    let root = PathBuf::from(value);
    if !root.is_absolute() {
        anyhow::bail!(
            "FEANORFS_WORKSPACE_ROOT must be an absolute workspace path; got '{}'. Unset it or rerun `feanorfs agent run` from the shared workspace.",
            root.display()
        );
    }
    let root = root.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "FEANORFS_WORKSPACE_ROOT '{}' is unavailable: {error}. Unset it or rerun `feanorfs agent run` from the shared workspace.",
            root.display()
        )
    })?;
    if !root.is_dir() {
        anyhow::bail!(
            "FEANORFS_WORKSPACE_ROOT '{}' must identify a workspace directory.",
            root.display()
        );
    }
    Ok(root)
}

async fn run_agent_send(
    current_dir: &Path,
    json: bool,
    args: SendSignalArgs,
) -> anyhow::Result<()> {
    let control_root = control_workspace_root(current_dir)?;
    let config = load_config(&control_root)?;
    let db = crate::open_client_db(&control_root).await?;
    let api = crate::open_api_client(&control_root, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, &control_root, &config)?;
    let sender = agent_sender(args.from);
    let result = feanorfs_client::send_message(
        &ctx,
        AgentMessageInput {
            to: args.recipient.clone(),
            kind: args.kind.into(),
            body: args.body,
            about_snapshot: args.about,
            reply_to: args.reply_to,
            from: Some(sender),
        },
    )
    .await?;
    if json {
        output_json(&result)?;
    } else {
        let kind_name = AgentMessageKind::from(args.kind).as_str();
        println!(
            "Sent {kind_name} signal {} to '{}' (about {}).",
            &result.message_id[..8],
            args.recipient,
            &result.about_snapshot[..8]
        );
    }
    Ok(())
}

async fn run_agent_inbox(
    current_dir: &Path,
    json: bool,
    for_recipient: Option<&str>,
    after: Option<&str>,
    limit: Option<usize>,
) -> anyhow::Result<()> {
    let control_root = control_workspace_root(current_dir)?;
    let config = load_config(&control_root)?;
    let db = crate::open_client_db(&control_root).await?;
    let api = crate::open_api_client(&control_root, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, &control_root, &config)?;
    let recipient = agent_sender(for_recipient.map(str::to_string));
    let result = feanorfs_client::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: recipient.clone(),
            after: after.map(str::to_string),
            limit: limit.unwrap_or(50),
        },
    )
    .await?;
    if json {
        output_json(&result)?;
        return Ok(());
    }
    if result.messages.is_empty() {
        println!("No signals for '{recipient}'.");
    } else {
        for message in &result.messages {
            let about = &message.about_snapshot[..8];
            let reply = message
                .reply_to
                .as_deref()
                .map(|id| format!(" reply {}", &id[..8]))
                .unwrap_or_default();
            println!(
                "{} {} -> {} {} about {}{}: {}",
                &message.message_id[..8],
                message.from,
                message.to,
                message.kind.as_str(),
                about,
                reply,
                terminal_line(&message.body)
            );
        }
    }
    if result.cursor_reset {
        println!(
            "Warning: the previous cursor or result bound could not cover all history; older signals may have been missed."
        );
    }
    if !result.cursor.is_empty() {
        println!(
            "New signals since this read: run with --after {} ",
            result.cursor
        );
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
        if let Some(live) = &result.live {
            println!("  Live: {}", render_live_line(live));
        }
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

/// One bounded human line for the live continuous projection.
fn render_live_line(live: &feanorfs_common::ContinuousAgentStatus) -> String {
    let phase = live.phase.as_str();
    let head = live
        .observed_head
        .as_deref()
        .map(|id| format!(" head={}", id.chars().take(8).collect::<String>()))
        .unwrap_or_default();
    let settled = live
        .settled_snapshot
        .as_deref()
        .map(|id| format!(" settled={}", id.chars().take(8).collect::<String>()))
        .unwrap_or_default();
    let attention = live
        .attention
        .as_ref()
        .map(|attention| {
            format!(
                " !{}: {}",
                terminal_line(&attention.reason),
                terminal_line(&attention.detail)
            )
        })
        .unwrap_or_default();
    format!(
        "{phase}{head}{settled} pending_local={} deferred={}{attention}",
        live.pending_local, live.deferred_count
    )
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
    let live = match feanorfs_agent_core::live_continuous_status(current_dir, name) {
        Ok(Some(status)) if status.active => Some(status),
        _ => None,
    };
    match check_agent(current_dir, db, api, workspace_id, name, password).await {
        Ok(check) => {
            let state = if !check.conflicts.is_empty() {
                format!("{} conflict(s)", check.conflicts.len())
            } else if !check.our_changes.is_empty() {
                format!("{} change(s)", check.our_changes.len())
            } else {
                "clean".into()
            };
            match live {
                Some(live) => format!("{state} [{}]", live.phase.as_str()),
                None => state,
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
