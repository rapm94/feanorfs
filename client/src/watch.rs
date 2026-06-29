use crate::api::ApiClient;
use crate::commands::do_sync;
use crate::local::ClientDb;
use anyhow::Result;
use feanorfs_common::normalize_path;
use notify::Watcher;
use std::path::Path;

pub async fn run_watch(
    api: &ApiClient,
    db: &ClientDb,
    current_dir: &Path,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<()> {
    tracing::info!("Starting watcher on {}", current_dir.display());
    println!("Starting change watcher on {}...", current_dir.display());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(100);

    let tx_clone = tx.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let mut interest = false;
                for path in &event.paths {
                    if let Some(path_str) = path.to_str() {
                        let normalized = normalize_path(path_str);
                        if !normalized.contains("/.feanorfs/")
                            && !normalized.contains("/.git/")
                            && !normalized.ends_with(".feanorfs")
                            && !normalized.ends_with(".git")
                        {
                            interest = true;
                            break;
                        }
                    }
                }
                if interest {
                    tracing::debug!("FS event: {:?}", event);
                    let _ = tx_clone.try_send(());
                }
            }
        })?;

    watcher.watch(current_dir, notify::RecursiveMode::Recursive)?;
    println!("Watching for changes... (Press Ctrl+C to stop)");

    tracing::info!("Initial sync");
    println!("Performing initial sync...");
    match do_sync(api, db, current_dir, workspace_id, password, false).await {
        Ok(result) => print_sync_result(&result),
        Err(e) => {
            tracing::error!("Initial sync failed: {:?}", e);
            eprintln!("Initial sync failed: {:?}", e);
        }
    }

    loop {
        if rx.recv().await.is_none() {
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        tracing::info!("Change detected, syncing");
        println!("Changes detected! Syncing...");
        match do_sync(api, db, current_dir, workspace_id, password, false).await {
            Ok(result) => print_sync_result(&result),
            Err(e) => {
                tracing::error!("Auto-sync failed: {:?}", e);
                eprintln!("Auto-sync failed: {:?}", e);
            }
        }
    }

    Ok(())
}

fn print_sync_result(result: &crate::commands::SyncResult) {
    println!(
        "Sync complete. Uploaded {}, Downloaded {} (lazy: {}), Local Deletes {}, Remote Deletes {}.",
        result.uploads,
        result.downloads,
        result.placeholders,
        result.deletes_local,
        result.deletes_remote
    );
}
