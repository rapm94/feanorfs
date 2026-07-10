#![allow(unused_imports)]
use clap::Subcommand;

use feanorfs_client::ApiClient;
use feanorfs_client::ClientDb;
use feanorfs_client::{commands, load_config, watch};
use std::path::Path;

use super::util::output_json;

#[derive(Subcommand)]
pub enum SyncAction {
    /// Show local and remote differences
    Status,
    /// Upload local changes to the server (encrypted). Prefer `sync --up`.
    #[command(hide = true, visible_alias = "sync-up")]
    Push,
    /// Download remote changes from the server. Prefer `sync --down`.
    #[command(hide = true, visible_alias = "sync-down")]
    Pull {
        /// Defer downloading raw blob contents and create 0-byte placeholders instead
        #[arg(long)]
        lazy: bool,
    },
    /// Perform a bidirectional sync (pull and push)
    Sync {
        /// Upload local changes only (same as legacy `push`)
        #[arg(long, conflicts_with = "down")]
        up: bool,
        /// Download remote changes only (same as legacy `pull`)
        #[arg(long, conflicts_with = "up")]
        down: bool,
        /// Defer downloading raw blob contents and create 0-byte placeholders instead.
        /// Lazy sync can leave 0-byte placeholders; prefer full sync unless bandwidth is tight.
        #[arg(long)]
        lazy: bool,
        /// Perform the sync once and exit without entering the real-time watch loop
        #[arg(long)]
        no_watch: bool,
    },
    /// Watch for local changes and sync them in real time (legacy — `sync` enters watch by default)
    #[command(hide = true)]
    Watch,
    /// Remove ignored paths already tracked on the server
    #[command(hide = true)]
    PruneIgnored {
        /// List paths that would be pruned without deleting
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn run(current_dir: &Path, action: SyncAction, json: bool) -> anyhow::Result<()> {
    match action {
        SyncAction::Status => run_status(current_dir, json).await,
        SyncAction::Push => run_push(current_dir, json).await,
        SyncAction::Pull { lazy } => run_pull(current_dir, json, lazy).await,
        SyncAction::Sync {
            up,
            down,
            lazy,
            no_watch,
        } => run_sync(current_dir, json, up, down, lazy, no_watch).await,
        SyncAction::Watch => run_watch(current_dir).await,
        SyncAction::PruneIgnored { dry_run } => run_prune(current_dir, json, dry_run).await,
    }
}

async fn open(
    current_dir: &Path,
) -> anyhow::Result<(feanorfs_client::Config, ClientDb, ApiClient)> {
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    Ok((config, db, api))
}

async fn run_status(current_dir: &Path, json: bool) -> anyhow::Result<()> {
    let (config, db, api) = open(current_dir).await?;
    if !json {
        println!("Scanning workspace directory...");
    }
    let result = commands::do_status(
        &api,
        &db,
        current_dir,
        &config.workspace_id,
        config.encryption_password.as_deref(),
    )
    .await?;
    if json {
        output_json(&result)?;
        return Ok(());
    }
    let mut has_changes = false;
    if !result.upload_required.is_empty() {
        has_changes = true;
        println!("\nLocal changes not yet on the mirror (run 'feanorfs sync --up'):");
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
        println!("\nChanges on other machines to download (run 'feanorfs sync --down'):");
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
        println!("\nFiles removed on other machines (run 'feanorfs sync --down'):");
        for path in &result.delete_local {
            println!("  [delete]     {}", path);
        }
    }
    if !has_changes {
        println!("\nMirror is {}.", result.mirror_state);
    } else {
        println!("\nMirror state: {}", result.mirror_state);
    }
    if result.offline_backlog > 0 {
        println!(
            "  {} local change(s) waiting to sync when online.",
            result.offline_backlog
        );
    }
    if let Some(ref warn) = result.server_rollback_warning {
        println!("  Warning: {warn}");
    }
    if !result.skipped_symlinks.is_empty() {
        println!(
            "  Skipped {} symlink(s) (not synced): {}",
            result.skipped_symlinks.len(),
            result.skipped_symlinks.join(", ")
        );
    }
    Ok(())
}

async fn run_push(current_dir: &Path, json: bool) -> anyhow::Result<()> {
    let (config, db, api) = open(current_dir).await?;
    if !json {
        println!("Pushing...");
    }
    let result = commands::do_push_only(
        &api,
        &db,
        current_dir,
        &config.workspace_id,
        config.encryption_password.as_deref(),
    )
    .await?;
    if json {
        output_json(&result)?;
    } else {
        println!(
            "Push complete. Uploaded {} files, processed {} deletions.",
            result.uploads, result.deletes
        );
        if result.remote_updates_available {
            println!("Note: Remote updates available. Run 'feanorfs sync --down' to apply.");
        }
    }
    Ok(())
}

async fn run_pull(current_dir: &Path, json: bool, lazy: bool) -> anyhow::Result<()> {
    let (config, db, api) = open(current_dir).await?;
    if !json {
        println!("Pulling...");
    }
    let result = commands::do_pull_only(
        &api,
        &db,
        current_dir,
        &config.workspace_id,
        config.encryption_password.as_deref(),
        lazy,
    )
    .await?;
    if json {
        output_json(&result)?;
    } else {
        println!(
            "Pull complete. Downloaded {}, {} lazy placeholders, {} deletions.",
            result.downloads, result.placeholders, result.deletes
        );
    }
    Ok(())
}

async fn run_sync(
    current_dir: &Path,
    json: bool,
    up: bool,
    down: bool,
    lazy: bool,
    no_watch: bool,
) -> anyhow::Result<()> {
    if lazy && !json {
        eprintln!(
            "Warning: lazy sync creates 0-byte placeholders. Use full sync unless you need deferred downloads."
        );
    }
    let (config, db, api) = open(current_dir).await?;
    if up && down {
        anyhow::bail!("Use at most one of --up or --down");
    }
    if !json {
        println!("Syncing...");
    }
    if up {
        let result = commands::do_push_only(
            &api,
            &db,
            current_dir,
            &config.workspace_id,
            config.encryption_password.as_deref(),
        )
        .await?;
        if json {
            output_json(&result)?;
        } else {
            println!(
                "Upload complete. {} file(s) uploaded, {} deletion(s).",
                result.uploads, result.deletes
            );
        }
    } else if down {
        let result = commands::do_pull_only(
            &api,
            &db,
            current_dir,
            &config.workspace_id,
            config.encryption_password.as_deref(),
            lazy,
        )
        .await?;
        if json {
            output_json(&result)?;
        } else {
            println!(
                "Download complete. {} file(s), {} placeholder(s), {} deletion(s).",
                result.downloads, result.placeholders, result.deletes
            );
        }
    } else {
        let result = commands::do_sync(
            &api,
            &db,
            current_dir,
            &config.workspace_id,
            config.encryption_password.as_deref(),
            lazy,
        )
        .await?;
        if json {
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
                current_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;
        }
    }
    Ok(())
}

async fn run_watch(current_dir: &Path) -> anyhow::Result<()> {
    let (config, db, api) = open(current_dir).await?;
    watch::run_watch(
        &api,
        &db,
        current_dir,
        &config.workspace_id,
        config.encryption_password.as_deref(),
    )
    .await
}

async fn run_prune(current_dir: &Path, json: bool, dry_run: bool) -> anyhow::Result<()> {
    let (config, db, api) = open(current_dir).await?;
    let result =
        commands::prune_ignored(&api, &db, current_dir, &config.workspace_id, dry_run).await?;
    if json {
        output_json(&result)?;
    } else if dry_run {
        println!("Would prune {} path(s):", result.candidates.len());
        for p in &result.candidates {
            println!("  {p}");
        }
    } else {
        println!("Pruned {} path(s).", result.pruned.len());
    }
    Ok(())
}
