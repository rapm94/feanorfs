use clap::Subcommand;
use feanorfs_client::{
    load_config, load_global_config, save_global_config, summary, ApiClient, ClientDb, GlobalConfig,
};
use std::path::Path;

use super::serve::{run_serve, ServeCli};
use super::start::{run_start, StartOptions};
use super::util::{
    acquire_token, copy_to_clipboard, initialize_local_mirror, initialize_new_mirror,
    invite_from_config, join_from_invite, link_existing_mirror, output_json, print_invite,
    probe_server_auth, read_password_hidden, resolve_server_url, truncate_password_for_display,
};

#[derive(Subcommand)]
pub enum WorkspaceAction {
    /// Begin or resume: create, join, sync, and watch
    Start {
        /// Server URL, fnr1-… invite, or omit to resume / guided setup
        target: Option<String>,
        /// Folder to mirror (default: current directory)
        folder: Option<std::path::PathBuf>,
        /// Workspace name for this mirrored folder
        #[arg(short, long, default_value = "default")]
        workspace: String,
        /// Workspace encryption key (manual link — requires --workspace)
        #[arg(long)]
        encryption_key: Option<String>,
        /// Server access token
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        /// Discover server on local network via mDNS
        #[arg(long)]
        lan: bool,
        /// Embedded local hub (no remote server)
        #[arg(long)]
        local: bool,
        /// Sync once and exit without entering watch mode
        #[arg(long)]
        no_watch: bool,
    },
    /// Run the sync hub HTTP server
    Serve(ServeCli),
    /// Show connection and workspace configuration
    Config {
        /// Show full encryption key and invite (copies to clipboard)
        #[arg(long)]
        key: bool,
    },
    /// Diagnose connection and configuration issues
    Doctor,
    /// Summarize files that changed since you last opened this workspace
    Summary {
        /// Shell out to FEANORFS_SUMMARY_CMD to produce prose instead of listing paths
        #[arg(long)]
        summarize: bool,
        /// Do not save the current snapshot as the baseline for the next catch-up diff
        #[arg(long)]
        no_remember: bool,
    },
    /// Re-seal blobs as AEAD and upgrade workspace to format v2
    Migrate {
        #[arg(long)]
        rekey: bool,
    },
    /// Mirror this folder (legacy — prefer `feanorfs start`)
    #[command(alias = "init", hide = true)]
    Setup {
        #[arg(short, long, default_value = "default")]
        workspace: String,
        server_url: Option<String>,
        #[arg(long)]
        encryption_key: Option<String>,
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        #[arg(long)]
        lan: bool,
        #[arg(long)]
        local: bool,
    },
    /// Join via invite or manual flags (legacy — prefer `feanorfs start`)
    #[command(hide = true)]
    Join {
        invite: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        encryption_key: Option<String>,
        #[arg(long)]
        server_url: Option<String>,
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        #[arg(long)]
        lan: bool,
    },
    /// Link with explicit flags (legacy — prefer `feanorfs start --encryption-key …`)
    #[command(hide = true)]
    Attach {
        workspace: String,
        #[arg(long)]
        encryption_key: String,
        #[arg(long)]
        server_url: Option<String>,
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        #[arg(long)]
        lan: bool,
    },
    /// Cache server URL in ~/.feanorfs/global.json (legacy — prefer `feanorfs start <URL>`)
    #[command(hide = true)]
    Connect {
        url: Option<String>,
        #[arg(long = "token")]
        token: Option<String>,
        #[arg(long)]
        lan: bool,
    },
    /// Show E2EE key (legacy — prefer `feanorfs config --key`)
    #[command(hide = true)]
    ShowKey,
    /// List workspaces on the server (legacy)
    #[command(hide = true, aliases = ["list", "ls"])]
    Workspaces { server_url: Option<String> },
    /// NDJSON event stream for orchestrators
    #[command(hide = true)]
    Events,
    /// Stdio MCP tool server for orchestrators
    #[command(hide = true)]
    Mcp,
    /// Tray companion commands (status, pause, recent workspaces).
    #[command(hide = true)]
    Tray {
        #[command(subcommand)]
        action: super::tray::TrayAction,
    },
}

pub async fn run(current_dir: &Path, action: WorkspaceAction, json: bool) -> anyhow::Result<()> {
    match action {
        WorkspaceAction::Start {
            target,
            folder,
            workspace,
            encryption_key,
            server_token,
            lan,
            local,
            no_watch,
        } => {
            run_start(
                current_dir,
                StartOptions {
                    target,
                    folder,
                    workspace,
                    encryption_key,
                    server_token,
                    lan,
                    local,
                    no_watch,
                },
            )
            .await
        }
        WorkspaceAction::Setup {
            workspace,
            server_url,
            encryption_key,
            server_token,
            lan,
            local,
        } => {
            run_setup(
                current_dir,
                workspace,
                server_url,
                encryption_key,
                server_token,
                lan,
                local,
            )
            .await
        }
        WorkspaceAction::Join {
            invite,
            workspace,
            encryption_key,
            server_token,
            server_url,
            lan,
        } => {
            if let Some(ref key) = encryption_key {
                let ws = workspace.ok_or_else(|| {
                    anyhow::anyhow!("--workspace is required with --encryption-key")
                })?;
                return run_attach(current_dir, ws, key.clone(), server_url, server_token, lan)
                    .await;
            }
            if let Some(token) = invite {
                return join_from_invite(current_dir, &token, true).await;
            }
            run_join_interactive(current_dir, lan).await
        }
        WorkspaceAction::Attach {
            workspace,
            encryption_key,
            server_url,
            server_token,
            lan,
        } => {
            run_attach(
                current_dir,
                workspace,
                encryption_key,
                server_url,
                server_token,
                lan,
            )
            .await
        }
        WorkspaceAction::Connect { url, token, lan } => run_connect(url, token, lan).await,
        WorkspaceAction::Serve(args) => run_serve(args).await,
        WorkspaceAction::Config { key } => run_config(current_dir, key),
        WorkspaceAction::ShowKey => run_config(current_dir, true),
        WorkspaceAction::Doctor => run_doctor(current_dir).await,
        WorkspaceAction::Workspaces { server_url } => {
            run_workspaces(current_dir, json, server_url).await
        }
        WorkspaceAction::Summary {
            summarize,
            no_remember,
        } => run_summary(current_dir, json, summarize, no_remember).await,
        WorkspaceAction::Migrate { rekey } => {
            feanorfs_client::migrate_workspace(current_dir, rekey).await
        }
        WorkspaceAction::Events => super::events::run_events(current_dir).await,
        WorkspaceAction::Mcp => super::mcp::run_mcp(current_dir).await,
        WorkspaceAction::Tray { action } => super::tray::run(current_dir, action, json).await,
    }
}

async fn run_setup(
    current_dir: &Path,
    workspace: String,
    server_url: Option<String>,
    encryption_key: Option<String>,
    server_token: Option<String>,
    lan: bool,
    local: bool,
) -> anyhow::Result<()> {
    if local {
        return initialize_local_mirror(current_dir, workspace, encryption_key).await;
    }
    let url = match server_url {
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => u,
        Some(u) => format!("http://{u}"),
        None => resolve_server_url(None, lan)?,
    };
    let final_token = acquire_token(&url, server_token).await?;
    initialize_new_mirror(
        current_dir,
        url,
        workspace,
        encryption_key,
        final_token,
        true,
        false,
    )
    .await
}

async fn run_attach(
    current_dir: &Path,
    workspace: String,
    encryption_key: String,
    server_url: Option<String>,
    server_token: Option<String>,
    lan: bool,
) -> anyhow::Result<()> {
    let url = match server_url {
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => u,
        Some(u) => format!("http://{u}"),
        None => resolve_server_url(None, lan)?,
    };
    link_existing_mirror(
        current_dir,
        url,
        workspace,
        encryption_key,
        server_token,
        false,
        false,
    )
    .await
}

async fn run_connect(url: Option<String>, token: Option<String>, lan: bool) -> anyhow::Result<()> {
    let server_url = resolve_server_url(url, lan)?;
    let final_token = match token {
        Some(t) => Some(t),
        None => match probe_server_auth(&server_url).await {
            Ok(true) => Some(read_password_hidden("Server requires a token: ")?),
            Ok(false) => None,
            Err(e) => {
                tracing::warn!(
                    "Server auth probe failed for {}: {:?}. Saving without token.",
                    server_url,
                    e
                );
                None
            }
        },
    };
    let global = GlobalConfig {
        server_url: server_url.clone(),
        server_password: final_token.clone(),
    };
    save_global_config(&global)?;
    println!("Connected to FeanorFS server at {}", server_url);
    if final_token.is_some() {
        println!("  Server token: saved");
    }
    println!("\nNow run: feanorfs start");
    Ok(())
}

async fn run_join_interactive(current_dir: &Path, lan: bool) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("No invite provided. Pass fnr1-… or run: feanorfs start fnr1-…");
    }
    if lan {
        let url = resolve_server_url(None, true)?;
        println!("Discovered server at {url}. Paste an fnr1-… invite from the host machine.");
    } else {
        println!("Paste the fnr1-… invite from the host machine.");
        println!("For LAN discovery, re-run with: feanorfs start --lan");
    }
    print!("Invite: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let token = line.trim();
    if token.is_empty() {
        anyhow::bail!("No invite provided.");
    }
    join_from_invite(current_dir, token, true).await
}

fn run_config(current_dir: &Path, show_key: bool) -> anyhow::Result<()> {
    if show_key {
        return run_show_key(current_dir);
    }
    match load_global_config() {
        Ok(g) => {
            println!("Global connection (~/.feanorfs/global.json):");
            println!("  Server:        {}", g.server_url);
            println!(
                "  Server auth:   {}",
                if g.server_password.is_some() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }
        Err(_) => {
            println!("Global connection: not configured");
        }
    }
    println!();
    match load_config(current_dir) {
        Ok(c) => {
            println!("Workspace (.feanorfs/config.json):");
            println!("  Server:        {}", c.server_url);
            println!("  Workspace ID:  {}", c.workspace_id);
            let e2ee_status = if c.encryption_password.is_some() {
                "enabled"
            } else {
                "disabled"
            };
            println!("  E2EE:          {}", e2ee_status);
            if let Some(ref p) = c.encryption_password {
                println!("  E2EE key:      {}", truncate_password_for_display(p));
            }
            println!(
                "  Server auth:   {}",
                if c.server_password.is_some() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!("\nFull key + invite: feanorfs config --key");
        }
        Err(_) => {
            println!("Workspace: not mirrored yet (run `feanorfs start` in this directory)");
        }
    }
    Ok(())
}

fn run_show_key(current_dir: &Path) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    match config.encryption_password {
        Some(ref key) => {
            println!("{}", key);
            copy_to_clipboard(key);
            eprintln!("\nCopied to clipboard.");
            if config.is_local_hub() {
                eprintln!(
                    "\nEmbedded local hub — invites are not portable. \
                     Run `feanorfs serve --data-dir .feanorfs/hub-data` to share on the network."
                );
            } else if let Some(invite) = invite_from_config(&config) {
                print_invite(&invite)?;
            } else {
                eprintln!("\nOn another machine:");
                eprintln!(
                    "  feanorfs start --workspace {} --encryption-key {}",
                    config.workspace_id, key
                );
            }
        }
        None => {
            println!("No encryption key set for this workspace.");
        }
    }
    Ok(())
}

async fn run_doctor(current_dir: &Path) -> anyhow::Result<()> {
    println!("Running diagnostics...\n");
    let mut all_ok = true;
    match load_global_config() {
        Ok(g) => {
            println!("[OK]  Global config: server at {}", g.server_url);
        }
        Err(_) => {
            println!("[INFO] Global config: not set (optional if workspace has a server URL)");
        }
    }
    match load_config(current_dir) {
        Ok(c) => {
            println!(
                "[OK]  Workspace config: {} on {}",
                c.workspace_id, c.server_url
            );
            if c.encryption_password.is_some() {
                println!("[OK]  E2EE: enabled");
            } else {
                println!("[WARN] E2EE: no password set (using insecure default)");
            }
            if c.format_version < 2 {
                println!("[WARN] Workspace format v1 — run `feanorfs migrate`");
            }
            let api = ApiClient::from_config(current_dir, &c).await?;
            match api.get_workspaces().await {
                Ok(workspaces) => {
                    println!(
                        "[OK]  Server reachable: {} workspace(s) found",
                        workspaces.len()
                    );
                    if workspaces.contains(&c.workspace_id) {
                        println!("[OK]  Workspace '{}' exists on server", c.workspace_id);
                    } else {
                        println!(
                            "[INFO] Workspace '{}' not yet on server (run `feanorfs sync`)",
                            c.workspace_id
                        );
                    }
                }
                Err(e) => {
                    println!("[FAIL] Server unreachable: {}", e);
                    all_ok = false;
                }
            }
            match ClientDb::new(current_dir.join(".feanorfs")).await {
                Ok(_) => println!("[OK]  Local cache DB: accessible"),
                Err(e) => {
                    println!("[FAIL] Local cache DB: {}", e);
                    all_ok = false;
                }
            }
        }
        Err(_) => {
            println!("[FAIL] Workspace: not mirrored (run `feanorfs start`)");
            all_ok = false;
        }
    }
    println!();
    if all_ok {
        println!("All checks passed.");
    } else {
        println!("Some checks failed. See above.");
    }
    Ok(())
}

async fn run_workspaces(
    current_dir: &Path,
    json: bool,
    server_url: Option<String>,
) -> anyhow::Result<()> {
    let (url, srv_pass) = if let Some(u) = server_url {
        (u, None)
    } else {
        let config = load_config(current_dir)?;
        (config.server_url, config.server_password)
    };
    let api = ApiClient::new(&url, srv_pass.as_deref());
    if !json {
        println!("Querying workspaces from server at {}...", url);
    }
    let workspaces = api.get_workspaces().await?;
    if json {
        output_json(&workspaces)?;
    } else if workspaces.is_empty() {
        println!("No active workspaces found on the server.");
    } else {
        println!("\nActive Workspaces:");
        for w in workspaces {
            println!("  * {}", w);
        }
    }
    Ok(())
}

async fn run_summary(
    current_dir: &Path,
    json: bool,
    summarize: bool,
    no_remember: bool,
) -> anyhow::Result<()> {
    let password = load_config(current_dir)
        .ok()
        .and_then(|c| c.encryption_password);
    let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
    let result = summary::diff_since_last_session(current_dir, &db, password.as_deref()).await?;
    if !no_remember {
        summary::commit_session_marker(current_dir, &db, password.as_deref()).await?;
    }
    if json {
        output_json(&result)?;
    } else if summarize {
        let rendered = summary::render_via_summary_tool(&result)?;
        println!("{}", rendered);
    } else {
        let rendered = summary::render_via_summary_tool(&result);
        match rendered {
            Ok(s) => println!("{}", s),
            Err(e) => {
                eprintln!("Summary tool error: {}", e);
                output_json(&result)?;
            }
        }
    }
    Ok(())
}
