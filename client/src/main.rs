use anyhow::Context as _;
use clap::{Parser, Subcommand};
use feanorfs_client::{
    agent, commands, load_config, load_global_config, predictive, save_config, save_global_config,
    summary, watch, ApiClient, ClientDb, Config, GlobalConfig,
};
use std::fs::OpenOptions;
use std::time::Duration;
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Registry};

fn setup_logging(current_dir: &std::path::Path) -> anyhow::Result<()> {
    let log_dir = current_dir.join(".feanorfs");
    let _ = std::fs::create_dir_all(&log_dir)
        .map_err(|e| eprintln!("Warning: could not create log directory: {e:?}"));

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("feanorfs.log"))?;

    let log_file_clone = log_file.try_clone()?;

    // Standard output layer: warnings and errors only.
    // User-facing progress output is handled by println! in commands.rs/watch.rs.
    // tracing::info!/debug! go to the file layer only, avoiding double output.
    let stdout_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_target(false)
        .without_time()
        .with_filter(EnvFilter::new("warn"));

    // File logging layer: detailed debug traces.
    // The writer closure must never panic — if the file handle becomes invalid
    // mid-run, fall back to a sink so log events are dropped silently rather
    // than crashing the process.
    let file_layer = fmt::layer()
        .with_writer(move || -> Box<dyn std::io::Write + Send> {
            match log_file_clone.try_clone() {
                Ok(f) => Box::new(f),
                Err(_) => Box::new(std::io::sink()),
            }
        })
        .with_target(true)
        .with_ansi(false)
        .with_filter(EnvFilter::new("debug"));

    let _ = Registry::default()
        .with(stdout_layer)
        .with(file_layer)
        .try_init();

    Ok(())
}

fn discover_server_mdns(timeout: Duration) -> anyhow::Result<String> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon =
        ServiceDaemon::new().map_err(|e| anyhow::anyhow!("Failed to start mDNS daemon: {}", e))?;
    let receiver = daemon
        .browse("_feanorfs._tcp.local.")
        .map_err(|e| anyhow::anyhow!("Failed to browse mDNS: {}", e))?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(ip) = info.addresses.iter().next() {
                    let url = format!("http://{}:{}", ip, info.port);
                    let _ = daemon.shutdown();
                    return Ok(url);
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    let _ = daemon.shutdown();
    anyhow::bail!(
        "No FeanorFS server found on local network within {} seconds. \
         Specify URL explicitly: feanorfs connect <URL>",
        timeout.as_secs()
    )
}

fn resolve_server_url(explicit: Option<String>, allow_lan: bool) -> anyhow::Result<String> {
    match explicit {
        Some(u) => Ok(u),
        None => match load_global_config() {
            Ok(g) => Ok(g.server_url),
            Err(_) => {
                if allow_lan {
                    println!("Searching for FeanorFS server on local network...");
                    discover_server_mdns(Duration::from_secs(3))
                } else {
                    anyhow::bail!(
                        "No server URL specified and no cached connection found.\n\
                         \n\
                         Connect to a server first:\n  \
                         feanorfs connect https://your-server.com:3030\n\n\
                         Or for LAN discovery:\n  \
                         feanorfs connect --lan"
                    )
                }
            }
        },
    }
}

fn resolve_server_password(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| load_global_config().ok().and_then(|g| g.server_password))
}

fn try_clipboard_cmd(cmd: &str, args: &[&str], text: &str) -> Option<std::process::ExitStatus> {
    std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait()
        })
        .ok()
}

fn copy_to_clipboard(text: &str) {
    let result = if cfg!(target_os = "macos") {
        try_clipboard_cmd("pbcopy", &[], text)
    } else if cfg!(target_os = "linux") {
        try_clipboard_cmd("xclip", &["-selection", "clipboard"], text)
            .or_else(|| try_clipboard_cmd("wl-copy", &[], text))
            .or_else(|| try_clipboard_cmd("xsel", &["--clipboard", "--input"], text))
    } else {
        None
    };
    let _ = result;
}

fn read_password_hidden(prompt: &str) -> anyhow::Result<String> {
    Ok(rpassword::prompt_password(prompt)?)
}

fn truncate_password_for_display(p: &str) -> String {
    let chars: Vec<char> = p.chars().collect();
    if chars.len() > 12 {
        let head: String = chars.iter().take(6).collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{}...{}", head, tail)
    } else {
        p.to_string()
    }
}

async fn probe_server_auth(url: &str) -> anyhow::Result<bool> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/workspaces", url.trim_end_matches('/')))
        .send()
        .await
        .context("Failed to reach server")?;
    Ok(resp.status() == reqwest::StatusCode::UNAUTHORIZED)
}

async fn initialize_new_mirror(
    current_dir: &std::path::Path,
    url: String,
    workspace: String,
    encryption_key: Option<String>,
    server_token: Option<String>,
    save_global: bool,
) -> anyhow::Result<()> {
    let srv_pass = resolve_server_password(server_token);

    if save_global {
        let global = GlobalConfig {
            server_url: url.clone(),
            server_password: srv_pass.clone(),
        };
        save_global_config(&global)?;
    }

    let (e2ee_key, was_generated) = match encryption_key {
        Some(k) => (k, false),
        None => (feanorfs_common::generate_password()?, true),
    };

    let config = Config {
        server_url: url.clone(),
        workspace_id: workspace.clone(),
        encryption_password: Some(e2ee_key.clone()),
        server_password: srv_pass.clone(),
    };
    save_config(current_dir, &config)?;

    let _db = ClientDb::new(current_dir.join(".feanorfs")).await?;

    println!("This folder is now mirrored to FeanorFS.");
    println!("  Server:       {}", url);
    println!("  Workspace:    {}", workspace);
    println!("  Encryption:   enabled (zero-knowledge)");
    if srv_pass.is_some() {
        println!("  Server auth:  enabled");
    }

    if was_generated {
        println!("\nWorkspace encryption key: {}", e2ee_key);
        copy_to_clipboard(&e2ee_key);
        println!("Copied to clipboard.");
        println!("\nOn your other machine, link the same workspace:");
        println!(
            "  feanorfs attach {} --encryption-key {}",
            workspace, e2ee_key
        );
        println!("\nSave this key — without it your files cannot be decrypted.");
    }

    Ok(())
}

async fn link_existing_mirror(
    current_dir: &std::path::Path,
    url: String,
    workspace: String,
    encryption_key: String,
    server_token: Option<String>,
) -> anyhow::Result<()> {
    let srv_pass = resolve_server_password(server_token);

    let global = GlobalConfig {
        server_url: url.clone(),
        server_password: srv_pass.clone(),
    };
    save_global_config(&global)?;

    let config = Config {
        server_url: url.clone(),
        workspace_id: workspace.clone(),
        encryption_password: Some(encryption_key),
        server_password: srv_pass.clone(),
    };
    save_config(current_dir, &config)?;

    let _db = ClientDb::new(current_dir.join(".feanorfs")).await?;

    println!("Linked this folder to mirrored workspace '{}'.", workspace);
    println!("  Server:     {}", url);
    println!("  Encryption: enabled");
    if srv_pass.is_some() {
        println!("  Server auth: enabled");
    }
    println!("\nRun 'feanorfs sync --no-watch' to download files from the mirror.");

    Ok(())
}

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

fn output_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    println!("{}", s);
    Ok(())
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
}

#[derive(Subcommand)]
enum AgentAction {
    /// Spawn a new isolated agent workspace `.feanorfs/agents/<name>/`.
    Spawn { name: String },
    /// Diff agent workspace against base snapshot and split into clean-our / clean-their / conflicts.
    Commit { name: String },
    /// List all spawned agent workspaces.
    List,
    /// Remove an agent workspace and its snapshot rows.
    Clean { name: String },
    /// Run a command inside an agent workspace (Level 1 process isolation).
    Run {
        name: String,
        /// Command and arguments to execute inside the agent workspace.
        /// Example: `feanorfs agent run ci1 -- cargo test`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
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
                    print!("{}", result.content);
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
        Commands::Agent { action } => match action {
            AgentAction::Spawn { name } => {
                let config = load_config(&current_dir)?;
                let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
                let count = agent::spawn_agent(
                    &current_dir,
                    &db,
                    &name,
                    config.encryption_password.as_deref(),
                )
                .await?;
                if cli.json {
                    output_json(&serde_json::json!({
                        "agent": name,
                        "files_linked": count,
                    }))?;
                } else {
                    println!(
                        "Agent '{}' spawned with {} files at .feanorfs/agents/{}/",
                        name, count, name
                    );
                }
            }
            AgentAction::Commit { name } => {
                let config = load_config(&current_dir)?;
                let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
                let api = ApiClient::new(&config.server_url, config.server_password.as_deref());
                let result = agent::commit_agent(
                    &current_dir,
                    &db,
                    &api,
                    &config.workspace_id,
                    &name,
                    config.encryption_password.as_deref(),
                )
                .await?;

                if cli.json {
                    output_json(&result)?;
                } else {
                    println!("Agent '{}' commit:", name);
                    println!("  Our changes:    {}", result.our_changes.len());
                    println!("  Their changes: {}", result.their_changes.len());
                    println!("  Conflicts:     {}", result.conflicts.len());
                    if !result.conflicts.is_empty() {
                        println!("\nConflicting paths (look in .feanorfs/conflicts/ for base/ours/theirs):");
                        for c in &result.conflicts {
                            println!("  ! {}", c.path);
                        }
                    }
                }
            }
            AgentAction::List => {
                let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
                let names = agent::list_agents(&current_dir, &db).await?;
                if cli.json {
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
                agent::clean_agent(&current_dir, &db, &name).await?;
                if cli.json {
                    output_json(&serde_json::json!({ "cleaned": name }))?;
                } else {
                    println!("Agent '{}' removed.", name);
                }
            }
            AgentAction::Run { name, command } => {
                if command.is_empty() {
                    anyhow::bail!("`agent run` requires a command after `--`. Example: feanorfs agent run ci -- cargo test");
                }
                agent::validate_name(&name)?;
                let agent_path = agent::agent_dir(&current_dir, &name);
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
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::truncate_password_for_display;

    #[test]
    fn display_short_password_returns_unchanged() {
        assert_eq!(truncate_password_for_display("short"), "short");
    }

    #[test]
    fn display_long_ascii_password_is_truncated_with_ellipsis() {
        let pw = "0123456789abcdef0123456789abcdef";
        let display = truncate_password_for_display(pw);
        assert!(
            display.contains("..."),
            "display must contain ellipsis: {}",
            display
        );
        assert!(
            display.starts_with("012345"),
            "head must be first 6 chars: {}",
            display
        );
        assert!(
            display.ends_with("cdef"),
            "tail must be last 4 chars: {}",
            display
        );
    }

    #[test]
    fn display_multibyte_password_does_not_panic() {
        let pw = "日本語のパスワード1234567890";
        let display = truncate_password_for_display(pw);
        assert!(
            !display.is_empty(),
            "display must be non-empty for multibyte password"
        );
        assert!(
            display.contains("..."),
            "long multibyte password must be truncated: {}",
            display
        );
    }

    #[test]
    fn display_exactly_twelve_chars_returns_unchanged() {
        let pw = "012345678901";
        assert_eq!(truncate_password_for_display(pw), pw);
    }

    #[test]
    fn display_thirteen_chars_is_truncated() {
        let pw = "0123456789012";
        let display = truncate_password_for_display(pw);
        assert!(
            display.contains("..."),
            "13-char password must be truncated: {}",
            display
        );
    }
}
