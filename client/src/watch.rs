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
    println!("Starting change watcher on {}...", current_dir.display());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(100);

    let tx_clone = tx.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let mut interest = false;
                for path in event.paths {
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
                    let _ = tx_clone.try_send(());
                }
            }
        })?;

    watcher.watch(Path::new("."), notify::RecursiveMode::Recursive)?;
    println!("Watching for changes... (Press Ctrl+C to stop)");

    println!("Performing initial sync...");
    if let Err(e) = do_sync(api, db, current_dir, workspace_id, password, false).await {
        eprintln!("Initial sync failed: {:?}", e);
    }

    loop {
        if rx.recv().await.is_none() {
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        while rx.try_recv().is_ok() {}

        println!("Changes detected! Syncing with server...");
        if let Err(e) = do_sync(api, db, current_dir, workspace_id, password, false).await {
            eprintln!("Auto-sync failed: {:?}", e);
        } else {
            println!("Sync complete.");
        }
    }

    Ok(())
}
