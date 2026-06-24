mod api;
mod commands;
mod local;
mod watch;

use anyhow::Context as _;
use api::ApiClient;
use clap::{Parser, Subcommand};
use local::{
    load_config, load_global_config, save_config, save_global_config, ClientDb, Config,
    GlobalConfig,
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

fn copy_to_clipboard(text: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("pbcopy")
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
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xclip")
            .args(["-selection", "clipboard"])
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
    } else {
        Ok(std::process::ExitStatus::default())
    };
    let _ = result;
}

fn read_password_hidden(prompt: &str) -> anyhow::Result<String> {
    use std::io::{self, BufRead, Write};
    let mut stderr = io::stderr();
    write!(stderr, "{}", prompt)?;
    stderr.flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    eprintln!();
    Ok(line.trim().to_string())
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

#[derive(Parser)]
#[command(name = "feanorfs")]
#[command(about = "FeanorFS: Developer-focused filesystem sync tool (client)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Connect to a FeanorFS server (cached for future commands)
    Connect {
        /// Server URL (e.g. https://my-server.com:3030). Required unless --lan is used.
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
        #[arg(short, long)]
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
    /// Join an existing workspace (combines connect + init in one command)
    Join {
        /// Workspace ID to join
        workspace: String,

        /// E2EE encryption password (required to decrypt files from other machines)
        #[arg(short, long)]
        password: String,

        /// Server URL. If omitted, uses the URL cached by `feanorfs connect`.
        #[arg(long)]
        server_url: Option<String>,

        /// Server access token (for servers that require authentication)
        #[arg(long, visible_alias = "password")]
        server_token: Option<String>,

        /// Discover server on local network via mDNS instead of using cached URL
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
                None => {
                    if probe_server_auth(&server_url).await.unwrap_or(false) {
                        Some(read_password_hidden("Server requires a token: ")?)
                    } else {
                        None
                    }
                }
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
            println!("\nNow run: feanorfs init --workspace <name>");
        }
        Commands::Init {
            server_url,
            workspace,
            password,
            server_token,
            lan,
        } => {
            let url = resolve_server_url(server_url.clone(), lan)?;
            let srv_pass = resolve_server_password(server_token);

            if server_url.is_some() {
                let global = GlobalConfig {
                    server_url: url.clone(),
                    server_password: srv_pass.clone(),
                };
                save_global_config(&global)?;
            }

            let (e2ee_password, was_generated) = match password {
                Some(p) => (p, false),
                None => {
                    let generated = feanorfs_common::generate_password()?;
                    (generated, true)
                }
            };

            let config = Config {
                server_url: url.clone(),
                workspace_id: workspace.clone(),
                encryption_password: Some(e2ee_password.clone()),
                server_password: srv_pass.clone(),
            };
            save_config(&current_dir, &config)?;

            let _db = ClientDb::new(current_dir.join(".feanorfs")).await?;

            println!("Initialized FeanorFS workspace!");
            println!("  Blob Server:  {}", url);
            println!("  Workspace ID: {}", workspace);
            println!("  Encryption:   Enabled (Blake3 XOF E2EE)");
            if srv_pass.is_some() {
                println!("  Server auth:  Enabled");
            }

            if was_generated {
                println!("\nE2EE password: {}", e2ee_password);
                copy_to_clipboard(&e2ee_password);
                println!("Copied to clipboard.");
                println!("\nJoin from another machine:");
                println!("  feanorfs join {} --password {}", workspace, e2ee_password);
                println!("\nSave this password! Without it, your files cannot be decrypted.");
            }
        }
        Commands::Join {
            workspace,
            password,
            server_url,
            server_token,
            lan,
        } => {
            let url = resolve_server_url(server_url, lan)?;
            let srv_pass = resolve_server_password(server_token);

            let config = Config {
                server_url: url.clone(),
                workspace_id: workspace.clone(),
                encryption_password: Some(password.clone()),
                server_password: srv_pass.clone(),
            };
            save_config(&current_dir, &config)?;

            let _db = ClientDb::new(current_dir.join(".feanorfs")).await?;

            println!("Joined workspace '{}' on {}", workspace, url);
            println!("  Encryption:   Enabled (Blake3 XOF E2EE)");
            if srv_pass.is_some() {
                println!("  Server auth:  Enabled");
            }
            println!("\nRun 'feanorfs sync' to pull files from the workspace.");
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
                        let display = if p.len() > 12 {
                            format!("{}...{}", &p[..6], &p[p.len() - 4..])
                        } else {
                            p.to_string()
                        };
                        println!("  E2EE key:      {}", display);
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
                    println!("Workspace: not initialized (run 'feanorfs init' in this directory)");
                }
            }
        }
        Commands::Status => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            println!("Scanning workspace directory...");
            let result = commands::do_status(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;

            let mut has_changes = false;

            if !result.upload_required.is_empty() {
                has_changes = true;
                println!("\nLocal changes to push (run 'feanorfs push'):");
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
                println!("\nRemote changes to pull (run 'feanorfs pull'):");
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
                println!("\nRemote deletions to apply (run 'feanorfs pull'):");
                for path in &result.delete_local {
                    println!("  [delete]     {}", path);
                }
            }

            if !has_changes {
                println!("\nEverything is up to date!");
            }
        }
        Commands::Push => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            println!("Pushing...");
            let result = commands::do_push_only(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;

            println!(
                "Push complete. Uploaded {} files, processed {} deletions.",
                result.uploads, result.deletes
            );
            if result.remote_updates_available {
                println!("Note: Remote updates available. Run 'feanorfs pull' to apply.");
            }
        }
        Commands::Pull { lazy } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            println!("Pulling...");
            let result = commands::do_pull_only(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                lazy,
            )
            .await?;

            println!(
                "Pull complete. Downloaded {}, {} lazy placeholders, {} deletions.",
                result.downloads, result.placeholders, result.deletes
            );
        }
        Commands::Sync { lazy, no_watch } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            println!("Syncing...");
            let result = commands::do_sync(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                lazy,
            )
            .await?;

            println!(
                "Sync complete. Uploaded {}, Downloaded {} (lazy: {}), Local Deletes {}, Remote Deletes {}.",
                result.uploads,
                result.downloads,
                result.placeholders,
                result.deletes_local,
                result.deletes_remote
            );

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
                path,
                config.encryption_password.as_deref(),
            )
            .await?;

            println!("{}", result.message);
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
                    eprintln!("Join from another machine:");
                    eprintln!("  feanorfs join {} --password {}", config.workspace_id, key);
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
                    println!("[FAIL] Workspace config: not initialized (run 'feanorfs init')");
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
            println!("Querying workspaces from server at {}...", url);
            let workspaces = api.get_workspaces().await?;
            if workspaces.is_empty() {
                println!("No active workspaces found on the server.");
            } else {
                println!("\nActive Workspaces:");
                for w in workspaces {
                    println!("  * {}", w);
                }
            }
        }
    }

    Ok(())
}
