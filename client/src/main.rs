mod cli;

use clap::{Parser, Subcommand};
use cli::{
    copy_to_clipboard, initialize_new_mirror, link_existing_mirror, output_json, probe_server_auth,
    read_password_hidden, resolve_server_url, setup_logging, truncate_password_for_display,
    AgentAction, ConflictsAction,
};
use feanorfs_client::{
    commands, load_config, load_global_config, predictive, save_global_config, summary, watch,
    ApiClient, ClientDb, GlobalConfig,
};
use std::io::Write as _;

#[derive(Parser)]
#[command(name = "feanorfs")]
#[command(about = "FeanorFS: Developer-focused filesystem sync tool (client)", long_about = None)]
struct Cli {
    /// Emit machine-readable JSON results instead of human prose. Honored by sync/push/pull/hydrate/cat/status/agent/summary.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Mirror this folder in one step (connect to server + create workspace)
    Setup {
        /// Workspace name for this mirrored folder
        #[arg(short, long, default_value = "default")]
        workspace: String,

        /// Server URL. If omitted, uses cached connection or `--lan` discovery.
        server_url: Option<String>,

        /// Workspace encryption key. If omitted, one is generated and saved.
        #[arg(long, visible_alias = "password")]
        encryption_key: Option<String>,

        /// Server access token
        #[arg(long, visible_alias = "password")]
        server_token: Option<String>,

        /// Discover server on local network via mDNS instead of providing a URL
        #[arg(long)]
        lan: bool,
    },
    /// Connect to a FeanorFS server (cached for future commands)
    Connect {
        /// Server URL (e.g. <https://my-server.com:3030>). Required unless --lan is used.
        url: Option<String>,

        /// Server access token (Bearer auth). In SaaS mode, this is your per-user API key.
        #[arg(long, visible_alias = "password")]
        token: Option<String>,

        /// Discover server on local network via mDNS instead of providing a URL
        #[arg(long)]
        lan: bool,
    },
    /// Initialize the current directory as a synced workspace
    Init {
        /// Server URL. If omitted, uses the URL cached by `feanorfs connect`.
        server_url: Option<String>,

        /// Workspace ID to sync with
        #[arg(short, long, default_value = "default")]
        workspace: String,

        /// E2EE encryption password. If omitted, one is auto-generated and saved.
        #[arg(short, long, visible_alias = "encryption-key")]
        password: Option<String>,

        /// Server access token (overrides the one cached by `feanorfs connect`)
        #[arg(long, visible_alias = "password")]
        server_token: Option<String>,

        /// Discover server on local network via mDNS instead of using cached URL
        #[arg(long)]
        lan: bool,
    },
    /// Show local and remote differences
    Status,
    /// Upload local changes to the server (encrypted)
    Push,
    /// Download remote changes from the server
    Pull {
        /// Defer downloading raw blob contents and create 0-byte placeholders instead
        #[arg(long)]
        lazy: bool,
    },
    /// Perform a bidirectional sync (pull and push)
    Sync {
        /// Defer downloading raw blob contents and create 0-byte placeholders instead
        #[arg(long)]
        lazy: bool,

        /// Perform the sync once and exit without entering the real-time watch loop
        #[arg(long)]
        no_watch: bool,
    },
    /// Download and decrypt deferred lazy placeholder files
    Hydrate {
        /// A specific file to hydrate. If omitted, hydrates all placeholder files.
        path: Option<String>,
    },
    /// Print a file's contents, downloading and decrypting it first if it is not hydrated
    Cat {
        /// The relative path of the file to display
        path: String,
    },
    /// Link this folder to a workspace already mirrored on another machine
    #[command(visible_alias = "join")]
    Attach {
        /// Workspace name to link to
        workspace: String,

        /// Workspace encryption key (from the machine that created the mirror)
        #[arg(long, visible_alias = "password")]
        encryption_key: String,

        /// Server URL. If omitted, uses cached connection or `--lan` discovery.
        #[arg(long)]
        server_url: Option<String>,

        /// Server access token
        #[arg(long, visible_alias = "password")]
        server_token: Option<String>,

        /// Discover server on local network via mDNS
        #[arg(long)]
        lan: bool,
    },
    /// Show current connection and workspace configuration
    Config,
    /// Show the E2EE password for this workspace (use to share with other machines)
    ShowKey,
    /// Diagnose connection and configuration issues
    Doctor,
    /// Watch for local changes and sync them in real time
    Watch,
    /// List all active workspaces tracked on the server
    #[command(aliases = ["list", "ls"])]
    Workspaces {
        /// Optional Server URL (overrides config URL)
        server_url: Option<String>,
    },
    /// Summarize files that changed since you last opened this workspace.
    Summary {
        /// Shell out to FEANORFS_SUMMARY_CMD to produce prose instead of listing paths.
        #[arg(long)]
        summarize: bool,

        /// Do not save the current snapshot as the baseline for the next catch-up diff.
        #[arg(long)]
        no_remember: bool,
    },
    /// Manage isolated agent workspaces (copy-on-write snapshots of the current workspace).
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// List or resolve workspace sync conflicts.
    Conflicts {
        #[command(subcommand)]
        action: ConflictsAction,
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
        Commands::Connect { url, token, lan } => {
            let server_url = resolve_server_url(url, lan)?;

            let final_token = match token {
                Some(t) => Some(t),
                None => match probe_server_auth(&server_url).await {
                    Ok(true) => Some(read_password_hidden("Server requires a token: ")?),
                    Ok(false) => None,
                    Err(e) => {
                        tracing::warn!(
                            "Server auth probe failed for {}: {:?}. Saving without token. \
                             If server requires auth, re-run 'feanorfs connect <URL> --token <T>'.",
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
            println!("\nNow run: feanorfs setup --workspace <name>");
        }
        Commands::Setup {
            workspace,
            server_url,
            encryption_key,
            server_token,
            lan,
        } => {
            let url = resolve_server_url(server_url, lan)?;
            let final_token = match server_token {
                Some(t) => Some(t),
                None => match probe_server_auth(&url).await {
                    Ok(true) => Some(read_password_hidden("Server requires a token: ")?),
                    Ok(false) => None,
                    Err(e) => {
                        tracing::warn!(
                            "Server auth probe failed for {}: {:?}. Continuing without token.",
                            url,
                            e
                        );
                        None
                    }
                },
            };
            initialize_new_mirror(
                &current_dir,
                url,
                workspace,
                encryption_key,
                final_token,
                true,
            )
            .await?;
        }
        Commands::Init {
            server_url,
            workspace,
            password,
            server_token,
            lan,
        } => {
            let url = resolve_server_url(server_url.clone(), lan)?;
            initialize_new_mirror(
                &current_dir,
                url,
                workspace,
                password,
                server_token,
                server_url.is_some(),
            )
            .await?;
        }
        Commands::Attach {
            workspace,
            encryption_key,
            server_url,
            server_token,
            lan,
        } => {
            let url = resolve_server_url(server_url, lan)?;
            link_existing_mirror(&current_dir, url, workspace, encryption_key, server_token)
                .await?;
        }
        Commands::Config => {
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
                    println!("Global connection: not configured (run 'feanorfs connect')");
                }
            }

            println!();
            match load_config(&current_dir) {
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
                }
                Err(_) => {
                    println!(
                        "Workspace: not mirrored yet (run 'feanorfs setup' in this directory)"
                    );
                }
            }
        }
        Commands::Status => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            if !cli.json {
                println!("Scanning workspace directory...");
            }
            let result = commands::do_status(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;

            if cli.json {
                output_json(&result)?;
            } else {
                let mut has_changes = false;

                if !result.upload_required.is_empty() {
                    has_changes = true;
                    println!("\nLocal changes not yet on the mirror (run 'feanorfs push'):");
                    for path in &result.upload_required {
                        if let Some(f) = result.local_files.get(path) {
                            if f.deleted {
                                println!("  [delete]     {}", path);
                            } else {
                                println!("  [modify/add] {}", path);
                            }
                        } else {
                            println!("  [modify/add] {}", path);
                        }
                    }
                }

                if !result.download_required.is_empty() {
                    has_changes = true;
                    println!("\nChanges on other machines to download (run 'feanorfs pull'):");
                    for f in &result.download_required {
                        println!(
                            "  [download]   {} ({:.1} KB)",
                            f.path,
                            f.size as f64 / 1024.0
                        );
                    }
                }

                if !result.delete_local.is_empty() {
                    has_changes = true;
                    println!("\nFiles removed on other machines (run 'feanorfs pull'):");
                    for path in &result.delete_local {
                        println!("  [delete]     {}", path);
                    }
                }

                if !has_changes {
                    println!("\nMirror is up to date.");
                } else {
                    println!("\nMirror state: {}", result.mirror_state);
                }
            }
        }
        Commands::Push => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            if !cli.json {
                println!("Pushing...");
            }
            let result = commands::do_push_only(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;

            if cli.json {
                output_json(&result)?;
            } else {
                println!(
                    "Push complete. Uploaded {} files, processed {} deletions.",
                    result.uploads, result.deletes
                );
                if result.remote_updates_available {
                    println!("Note: Remote updates available. Run 'feanorfs pull' to apply.");
                }
            }
        }
        Commands::Pull { lazy } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            if !cli.json {
                println!("Pulling...");
            }
            let result = commands::do_pull_only(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                lazy,
            )
            .await?;

            if cli.json {
                output_json(&result)?;
            } else {
                println!(
                    "Pull complete. Downloaded {}, {} lazy placeholders, {} deletions.",
                    result.downloads, result.placeholders, result.deletes
                );
            }
        }
        Commands::Sync { lazy, no_watch } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            if !cli.json {
                println!("Syncing...");
            }
            let result = commands::do_sync(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                lazy,
            )
            .await?;

            if cli.json {
                output_json(&result)?;
            } else {
                println!(
                    "Sync complete. Uploaded {}, Downloaded {} (lazy: {}), Local Deletes {}, Remote Deletes {}.",
                    result.uploads,
                    result.downloads,
                    result.placeholders,
                    result.deletes_local,
                    result.deletes_remote
                );
            }

            if !no_watch {
                watch::run_watch(
                    &api,
                    &db,
                    &current_dir,
                    &config.workspace_id,
                    config.encryption_password.as_deref(),
                )
                .await?;
            }
        }
        Commands::Hydrate { path } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            let result = commands::do_hydrate(
                &api,
                &db,
                &current_dir,
                path.clone(),
                config.encryption_password.as_deref(),
            )
            .await?;

            if let Some(ref p) = path {
                if let Err(e) = predictive::record_access_with_recent(&db, p).await {
                    tracing::warn!("Failed to record predictive access for {p}: {e:#}");
                }
                if let Err(e) = predictive::prefetch_related(
                    &current_dir,
                    &db,
                    &api,
                    config.encryption_password.as_deref(),
                    std::slice::from_ref(p),
                )
                .await
                {
                    tracing::warn!("Predictive prefetch failed for {p}: {e:#}");
                }
            }

            if cli.json {
                output_json(&result)?;
            } else {
                println!("{}", result.message);
            }
        }
        Commands::Cat { path } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            let result = commands::do_cat(
                &api,
                &db,
                &current_dir,
                &path,
                config.encryption_password.as_deref(),
            )
            .await?;

            if let Err(e) = predictive::record_access_with_recent(&db, &path).await {
                tracing::warn!("Failed to record predictive access for {path}: {e:#}");
            }

            if cli.json {
                output_json(&result)?;
            } else {
                if result.untracked {
                    println!("Warning: file '{}' is not tracked. Reading directly.", path);
                }
                if result.hydrated_first {
                    eprintln!("Hydrated {} from server.", path);
                }
                if result.not_found {
                    println!("Error: file '{}' does not exist.", path);
                } else {
                    std::io::stdout().write_all(&result.content)?;
                }
            }
        }
        Commands::Watch => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            watch::run_watch(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;
        }
        Commands::ShowKey => {
            let config = load_config(&current_dir)?;
            match config.encryption_password {
                Some(key) => {
                    println!("{}", key);
                    copy_to_clipboard(&key);
                    eprintln!("\nCopied to clipboard.");
                    eprintln!("\nOn another machine, link this workspace:");
                    eprintln!(
                        "  feanorfs attach {} --encryption-key {}",
                        config.workspace_id, key
                    );
                }
                None => {
                    println!("No E2EE password set for this workspace.");
                }
            }
        }
        Commands::Doctor => {
            println!("Running diagnostics...\n");

            let mut all_ok = true;

            match load_global_config() {
                Ok(g) => {
                    println!("[OK]  Global config: server at {}", g.server_url);
                }
                Err(_) => {
                    println!("[FAIL] Global config: not found (run 'feanorfs connect <URL>')");
                    all_ok = false;
                }
            }

            match load_config(&current_dir) {
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

                    let api = ApiClient::new(&c.server_url, c.server_password.as_deref());
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
                                    "[INFO] Workspace '{}' not yet on server (run 'feanorfs push')",
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
                    println!("[FAIL] Workspace: not mirrored (run 'feanorfs setup')");
                    all_ok = false;
                }
            }

            println!();
            if all_ok {
                println!("All checks passed.");
            } else {
                println!("Some checks failed. See above.");
            }
        }
        Commands::Workspaces { server_url } => {
            let (url, srv_pass) = if let Some(u) = server_url {
                (u, None)
            } else {
                let config = load_config(&current_dir)?;
                (config.server_url, config.server_password)
            };

            let api = ApiClient::new(&url, srv_pass.as_deref());
            if !cli.json {
                println!("Querying workspaces from server at {}...", url);
            }
            let workspaces = api.get_workspaces().await?;
            if cli.json {
                output_json(&workspaces)?;
            } else if workspaces.is_empty() {
                println!("No active workspaces found on the server.");
            } else {
                println!("\nActive Workspaces:");
                for w in workspaces {
                    println!("  * {}", w);
                }
            }
        }
        Commands::Summary {
            summarize,
            no_remember,
        } => {
            let password = load_config(&current_dir)
                .ok()
                .and_then(|c| c.encryption_password);
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let result =
                summary::diff_since_last_session(&current_dir, &db, password.as_deref()).await?;

            if !no_remember {
                summary::commit_session_marker(&current_dir, &db, password.as_deref()).await?;
            }

            if cli.json {
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
        }
        Commands::Agent { action } => {
            cli::agent::run(&current_dir, action, cli.json).await?;
        }
        Commands::Conflicts { action } => {
            cli::conflicts::run(&current_dir, action, cli.json).await?;
        }
    }

    Ok(())
}
