mod api;
mod commands;
mod local;
mod watch;

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
        /// Server URL (e.g. http://192.168.1.50:3030). If omitted, auto-discovers via mDNS on local network.
        url: Option<String>,

        /// Server access password (for servers that require authentication)
        #[arg(long)]
        password: Option<String>,
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

        /// Server access password (overrides the one cached by `feanorfs connect`)
        #[arg(long)]
        server_password: Option<String>,
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
        Commands::Connect { url, password } => {
            let server_url = match url {
                Some(u) => u,
                None => {
                    println!("Searching for FeanorFS server on local network...");
                    discover_server_mdns(Duration::from_secs(3))?
                }
            };

            let global = GlobalConfig {
                server_url: server_url.clone(),
                server_password: password.clone(),
            };
            save_global_config(&global)?;

            println!("Connected to FeanorFS server at {}", server_url);
            if password.is_some() {
                println!("  Server password: saved");
            }
            println!("\nNow run: feanorfs init --workspace <name>");
        }
        Commands::Init {
            server_url,
            workspace,
            password,
            server_password,
        } => {
            let (url, srv_pass) = match server_url {
                Some(u) => (u, server_password),
                None => {
                    let global = load_global_config()?;
                    (
                        global.server_url,
                        server_password.or(global.server_password),
                    )
                }
            };

            let e2ee_password = match password {
                Some(p) => p,
                None => {
                    let generated = feanorfs_common::generate_password()?;
                    println!("Generated E2EE password: {}", generated);
                    println!(
                        "Save this password! Other machines must use the same one to decrypt your files."
                    );
                    generated
                }
            };

            let config = Config {
                server_url: url.clone(),
                workspace_id: workspace.clone(),
                encryption_password: Some(e2ee_password),
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
        }
        Commands::Status => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            println!("Scanning workspace directory...");
            let local_files = local::scan_local_directory(
                &current_dir,
                &db,
                config.encryption_password.as_deref(),
            )
            .await?;

            let files_vec: Vec<feanorfs_common::FileState> =
                local_files.values().cloned().collect();
            let request = feanorfs_common::SyncRequest {
                workspace_id: config.workspace_id.clone(),
                files: files_vec,
            };

            println!("Querying server for diff...");
            let response = api.negotiate_sync(&request).await?;

            for (path, file) in &local_files {
                if file.deleted {
                    let _ = db.delete_cache_entry(path).await;
                }
            }

            let mut has_changes = false;

            if !response.upload_required.is_empty() {
                has_changes = true;
                println!("\nLocal changes to push (run 'feanorfs push'):");
                for path in &response.upload_required {
                    if let Some(f) = local_files.get(path) {
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

            if !response.download_required.is_empty() {
                has_changes = true;
                println!("\nRemote changes to pull (run 'feanorfs pull'):");
                for f in &response.download_required {
                    println!(
                        "  [download]   {} ({:.1} KB)",
                        f.path,
                        f.size as f64 / 1024.0
                    );
                }
            }

            if !response.delete_local.is_empty() {
                has_changes = true;
                println!("\nRemote deletions to apply (run 'feanorfs pull'):");
                for path in &response.delete_local {
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

            commands::do_push_only(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;
        }
        Commands::Pull { lazy } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            commands::do_pull_only(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                lazy,
            )
            .await?;
        }
        Commands::Sync { lazy, no_watch } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            commands::do_sync(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                lazy,
            )
            .await?;

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

            commands::do_hydrate(
                &api,
                &db,
                &current_dir,
                path,
                config.encryption_password.as_deref(),
            )
            .await?;
        }
        Commands::Cat { path } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url, config.server_password.as_deref());

            commands::do_cat(
                &api,
                &db,
                &current_dir,
                &path,
                config.encryption_password.as_deref(),
            )
            .await?;
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
