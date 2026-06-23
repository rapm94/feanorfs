mod api;
mod local;

use api::ApiClient;
use clap::{Parser, Subcommand};
use fs_sync_common::normalize_path;
use local::{load_config, save_config, ClientDb, Config};
use notify::Watcher;
use std::path::Path;
use tokio::fs;

#[derive(Parser)]
#[command(name = "fs-sync")]
#[command(about = "Developer-focused filesystem sync tool (client)", long_about = None)]
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
            
            // Initialize database schema
            let _db = ClientDb::new(current_dir.join(".fs-sync")).await?;

            println!("Initialized standalone fs-sync workspace!");
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
            let db = ClientDb::new(current_dir.join(".fs-sync")).await?;
            let api = ApiClient::new(&config.server_url);

            println!("Scanning workspace directory...");
            let local_files = local::scan_local_directory(&current_dir, &db, config.encryption_password.as_deref()).await?;
            
            let files_vec: Vec<fs_sync_common::FileState> = local_files.values().cloned().collect();
            let request = fs_sync_common::SyncRequest {
                workspace_id: config.workspace_id.clone(),
                files: files_vec,
            };

            println!("Querying server for diff...");
            let response = api.negotiate_sync(&request).await?;

            // Clean up locally deleted files that have been reported to the server
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
                    println!("  [download]   {} ({:.1} KB)", f.path, f.size as f64 / 1024.0);
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
            let db = ClientDb::new(current_dir.join(".fs-sync")).await?;
            let api = ApiClient::new(&config.server_url);

            do_push_only(&api, &db, &current_dir, &config.workspace_id, config.encryption_password.as_deref()).await?;
        }
        Commands::Pull { lazy } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".fs-sync")).await?;
            let api = ApiClient::new(&config.server_url);

            do_pull_only(&api, &db, &current_dir, &config.workspace_id, config.encryption_password.as_deref(), lazy).await?;
        }
        Commands::Sync { lazy } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".fs-sync")).await?;
            let api = ApiClient::new(&config.server_url);

            do_sync(&api, &db, &current_dir, &config.workspace_id, config.encryption_password.as_deref(), lazy).await?;
        }
        Commands::Hydrate { path } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".fs-sync")).await?;
            let api = ApiClient::new(&config.server_url);

            do_hydrate(&api, &db, &current_dir, path, config.encryption_password.as_deref()).await?;
        }
        Commands::Cat { path } => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".fs-sync")).await?;
            let api = ApiClient::new(&config.server_url);

            do_cat(&api, &db, &current_dir, &path, config.encryption_password.as_deref()).await?;
        }
        Commands::Watch => {
            let config = load_config(&current_dir)?;
            let db = ClientDb::new(current_dir.join(".fs-sync")).await?;
            let api = ApiClient::new(&config.server_url);

            println!("Starting change watcher on {}...", current_dir.display());
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(100);

            let tx_clone = tx.clone();
            let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let mut interest = false;
                    for path in event.paths {
                        if let Some(path_str) = path.to_str() {
                            let normalized = normalize_path(path_str);
                            if !normalized.contains("/.fs-sync/")
                                && !normalized.contains("/.git/")
                                && !normalized.ends_with(".fs-sync")
                                && !normalized.ends_with(".git")
                            {
                                interest = true;
                                break;
                            }
                        }
                    }
                    if interest {
                        let _ = tx_clone.try_send(());
                    }
                }
            })?;

            watcher.watch(Path::new("."), notify::RecursiveMode::Recursive)?;
            println!("Watching for changes... (Press Ctrl+C to stop)");

            // Run an initial sync to ensure current states are aligned
            println!("Performing initial sync...");
            if let Err(e) = do_sync(&api, &db, &current_dir, &config.workspace_id, config.encryption_password.as_deref(), false).await {
                eprintln!("Initial sync failed: {:?}", e);
            }

            // Debouncing loop
            loop {
                if rx.recv().await.is_none() {
                    break;
                }

                // Debounce: wait 500ms after last event
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                // Drain any additional events that fired during sleep
                while let Ok(_) = rx.try_recv() {}

                println!("Changes detected! Syncing with server...");
                if let Err(e) = do_sync(&api, &db, &current_dir, &config.workspace_id, config.encryption_password.as_deref(), false).await {
                    eprintln!("Auto-sync failed: {:?}", e);
                } else {
                    println!("Sync complete.");
                }
            }
        }
    }

    Ok(())
}

async fn do_push_only(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
) -> anyhow::Result<()> {
    println!("Scanning directory...");
    let local_files = local::scan_local_directory(base_path, db, password).await?;
    
    let files_vec: Vec<fs_sync_common::FileState> = local_files.values().cloned().collect();
    let request = fs_sync_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    println!("Querying server for diff...");
    let response = api.negotiate_sync(&request).await?;

    let mut uploads = 0;
    let mut deletes = 0;

    let password_str = password.unwrap_or("default-secret-key");

    // Upload required files
    for path in &response.upload_required {
        if let Some(local_file) = local_files.get(path) {
            if !local_file.deleted {
                println!("Uploading {}...", path);
                let plain_content = fs::read(base_path.join(path)).await?;
                let encrypted_content = fs_sync_common::crypt_bytes(&plain_content, password_str, path);
                
                api.upload_file(
                    workspace_id,
                    path,
                    &local_file.hash,
                    local_file.size,
                    local_file.mtime,
                    encrypted_content,
                ).await?;
                uploads += 1;
            }
        }
    }

    // Clean up cache entries for locally deleted files
    for (path, local_file) in &local_files {
        if local_file.deleted {
            db.delete_cache_entry(path).await?;
            deletes += 1;
        }
    }

    println!("Push completed. Uploaded {} files, processed {} deletions.", uploads, deletes);
    
    // Warn user about pending downloads if any
    if !response.download_required.is_empty() || !response.delete_local.is_empty() {
        println!("Note: There are remote updates available. Run 'fs-sync pull' to apply them.");
    }

    Ok(())
}

async fn do_pull_only(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> anyhow::Result<()> {
    println!("Scanning directory...");
    let local_files = local::scan_local_directory(base_path, db, password).await?;
    
    let files_vec: Vec<fs_sync_common::FileState> = local_files.values().cloned().collect();
    let request = fs_sync_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    println!("Querying server for diff...");
    let response = api.negotiate_sync(&request).await?;

    let mut downloads = 0;
    let mut deletes = 0;
    let mut placeholders = 0;

    let password_str = password.unwrap_or("default-secret-key");

    // Download required files
    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if lazy {
            println!("Creating lazy placeholder for {}...", path);
            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, b"").await?;

            let cache_entry = local::CacheEntry {
                path: path.clone(),
                plaintext_hash: "".to_string(),
                encrypted_hash: replica_file.hash.clone(),
                size: replica_file.size,
                mtime: replica_file.mtime,
                server_mtime: replica_file.mtime,
                hydrated: false,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            placeholders += 1;
        } else {
            println!("Downloading {}...", path);
            let encrypted_content = api.download_file(&replica_file.hash).await?;
            let plain_content = fs_sync_common::crypt_bytes(&encrypted_content, password_str, path);
            
            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let metadata = fs::metadata(&full_path).await?;
            let actual_mtime = metadata.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(replica_file.mtime);

            let plaintext_hash = fs_sync_common::hash_bytes(&plain_content);

            let cache_entry = local::CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: replica_file.hash.clone(),
                size: replica_file.size,
                mtime: actual_mtime,
                server_mtime: replica_file.mtime,
                hydrated: true,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            downloads += 1;
        }
    }

    // Delete local files
    for path in &response.delete_local {
        let full_path = base_path.join(path);
        if full_path.exists() {
            println!("Deleting {} (remote deletion)...", path);
            let _ = fs::remove_file(full_path).await;
        }
        db.delete_cache_entry(path).await?;
        deletes += 1;
    }

    // Clean up cache entries for locally deleted files
    for (path, local_file) in &local_files {
        if local_file.deleted {
            db.delete_cache_entry(path).await?;
        }
    }

    println!(
        "Pull completed. Downloaded {} files, created {} lazy placeholders, deleted {} files.",
        downloads, placeholders, deletes
    );
    Ok(())
}

async fn do_sync(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> anyhow::Result<()> {
    println!("Scanning directory...");
    let local_files = local::scan_local_directory(base_path, db, password).await?;
    
    let files_vec: Vec<fs_sync_common::FileState> = local_files.values().cloned().collect();
    let request = fs_sync_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    println!("Querying server for diff...");
    let response = api.negotiate_sync(&request).await?;

    let mut uploads = 0;
    let mut downloads = 0;
    let mut placeholders = 0;
    let mut deletes_local = 0;
    let mut deletes_remote = 0;

    let password_str = password.unwrap_or("default-secret-key");

    // 1. Process Downloads first to align state
    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if lazy {
            println!("Creating lazy placeholder for {}...", path);
            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, b"").await?;

            let cache_entry = local::CacheEntry {
                path: path.clone(),
                plaintext_hash: "".to_string(),
                encrypted_hash: replica_file.hash.clone(),
                size: replica_file.size,
                mtime: replica_file.mtime,
                server_mtime: replica_file.mtime,
                hydrated: false,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            placeholders += 1;
        } else {
            println!("Downloading {}...", path);
            let encrypted_content = api.download_file(&replica_file.hash).await?;
            let plain_content = fs_sync_common::crypt_bytes(&encrypted_content, password_str, path);
            
            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let metadata = fs::metadata(&full_path).await?;
            let actual_mtime = metadata.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(replica_file.mtime);

            let plaintext_hash = fs_sync_common::hash_bytes(&plain_content);

            let cache_entry = local::CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: replica_file.hash.clone(),
                size: replica_file.size,
                mtime: actual_mtime,
                server_mtime: replica_file.mtime,
                hydrated: true,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            downloads += 1;
        }
    }

    // Apply remote deletes to local disk
    for path in &response.delete_local {
        let full_path = base_path.join(path);
        if full_path.exists() {
            println!("Deleting {} (remote deletion)...", path);
            let _ = fs::remove_file(full_path).await;
        }
        db.delete_cache_entry(path).await?;
        deletes_local += 1;
    }

    // 2. Process Uploads
    for path in &response.upload_required {
        if let Some(local_file) = local_files.get(path) {
            if !local_file.deleted {
                println!("Uploading {}...", path);
                let plain_content = fs::read(base_path.join(path)).await?;
                let encrypted_content = fs_sync_common::crypt_bytes(&plain_content, password_str, path);
                
                api.upload_file(
                    workspace_id,
                    path,
                    &local_file.hash,
                    local_file.size,
                    local_file.mtime,
                    encrypted_content,
                ).await?;
                uploads += 1;
            }
        }
    }

    // Clean up cache entries for locally deleted files
    for (path, local_file) in &local_files {
        if local_file.deleted {
            db.delete_cache_entry(path).await?;
            deletes_remote += 1;
        }
    }

    println!(
        "Sync completed. Uploaded {}, Downloaded {} (lazy placeholders: {}), Local Deletes {}, Remote Deletes {}.",
        uploads, downloads, placeholders, deletes_local, deletes_remote
    );
    Ok(())
}

async fn do_hydrate(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    target_path: Option<String>,
    password: Option<&str>,
) -> anyhow::Result<()> {
    let password_str = password.unwrap_or("default-secret-key");
    let cache_entries = db.get_cache_entries().await?;

    let mut hydrated_count = 0;

    for (path, entry) in cache_entries {
        if let Some(ref target) = target_path {
            if path != *target {
                continue;
            }
        }

        if !entry.hydrated {
            println!("Hydrating {}...", path);
            let encrypted_content = api.download_file(&entry.encrypted_hash).await?;
            let plain_content = fs_sync_common::crypt_bytes(&encrypted_content, password_str, &path);
            
            let full_path = base_path.join(&path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let metadata = fs::metadata(&full_path).await?;
            let actual_mtime = metadata.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(entry.mtime);

            let plaintext_hash = fs_sync_common::hash_bytes(&plain_content);

            let updated_entry = local::CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: entry.encrypted_hash.clone(),
                size: plain_content.len() as u64,
                mtime: actual_mtime,
                server_mtime: entry.server_mtime,
                hydrated: true,
            };
            db.upsert_cache_entry(&updated_entry).await?;
            hydrated_count += 1;
        }
    }

    if hydrated_count == 0 {
        if let Some(ref target) = target_path {
            println!("File '{}' is already hydrated or not found.", target);
        } else {
            println!("All files are already hydrated.");
        }
    } else {
        println!("Hydrated {} files.", hydrated_count);
    }
    
    Ok(())
}

async fn do_cat(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    target_path: &str,
    password: Option<&str>,
) -> anyhow::Result<()> {
    let cache_entries = db.get_cache_entries().await?;
    if let Some(entry) = cache_entries.get(target_path) {
        if !entry.hydrated {
            do_hydrate(api, db, base_path, Some(target_path.to_string()), password).await?;
        }
    } else {
        println!("Warning: file '{}' is not tracked. Reading directly.", target_path);
    }

    let full_path = base_path.join(target_path);
    if full_path.exists() {
        let content = fs::read_to_string(full_path).await?;
        print!("{}", content);
    } else {
        println!("Error: file '{}' does not exist.", target_path);
    }
    Ok(())
}
