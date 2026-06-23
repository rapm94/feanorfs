mod api;
mod commands;
mod local;
mod watch;

use api::ApiClient;
use clap::{Parser, Subcommand};
use local::{load_config, save_config, ClientDb, Config};

#[derive(Parser)]
#[command(name = "feanorfs")]
#[command(about = "FeanorFS: Developer-focused filesystem sync tool (client)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the current directory as a synced workspace
    Init {
        /// Server URL (e.g. http://localhost:3030)
        server_url: String,

        /// Workspace ID to sync with
        #[arg(short, long, default_value = "default")]
        workspace: String,

        /// Encryption password for end-to-end zero-knowledge secrecy
        #[arg(short, long)]
        password: Option<String>,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let current_dir = std::env::current_dir()?;

    match cli.command {
        Commands::Init {
            server_url,
            workspace,
            password,
        } => {
            let config = Config {
                server_url: server_url.clone(),
                workspace_id: workspace.clone(),
                encryption_password: password.clone(),
            };
            save_config(&current_dir, &config)?;

            let _db = ClientDb::new(current_dir.join(".feanorfs")).await?;

            println!("Initialized standalone FeanorFS workspace!");
            println!("  Blob Server:  {}", server_url);
            println!("  Workspace ID: {}", workspace);
            if password.is_some() {
                println!("  Encryption:   Enabled (Blake3 symmetric stream)");
            } else {
                println!("  Encryption:   Disabled (default credentials)");
            }
        }
        Commands::Status => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url);

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
                println!("\nLocal changes to push (run 'fs-sync push'):");
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
                println!("\nRemote changes to pull (run 'fs-sync pull'):");
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
                println!("\nRemote deletions to apply (run 'fs-sync pull'):");
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
            let api = ApiClient::new(&config.server_url);

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
            let api = ApiClient::new(&config.server_url);

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
        Commands::Sync { lazy } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url);

            commands::do_sync(
                &api,
                &db,
                &current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
                lazy,
            )
            .await?;
        }
        Commands::Hydrate { path } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
            let api = ApiClient::new(&config.server_url);

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
            let api = ApiClient::new(&config.server_url);

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
            let api = ApiClient::new(&config.server_url);

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

    Ok(())
}
