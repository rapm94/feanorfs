use anyhow::{Context, Result};
use std::path::Path;

pub(crate) mod cache;
pub(crate) mod hub;
pub(crate) mod journal;
#[cfg(test)]
mod tests;

use cache::migrate_cache_store;
use hub::migrate_hub_store;
use journal::{load_journal, Fault};

pub async fn migrate_workspace_stores(root: &Path) -> Result<()> {
    migrate_workspace_stores_with_fault(root, Fault::None).await
}

pub(crate) async fn migrate_workspace_stores_with_fault(root: &Path, fault: Fault) -> Result<()> {
    let feanorfs = root.join(".feanorfs");
    let lock_path = feanorfs.join("metadata-migration.lock");
    std::fs::create_dir_all(&feanorfs)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .context("open migration lock")?;
    fs2::FileExt::lock_exclusive(&lock_file).context("acquire migration lock")?;
    let mut journal = load_journal(root)?;

    let main_db = feanorfs.join("local_cache.db");
    if main_db.exists() || journal.stores.contains_key("main") {
        migrate_cache_store(root, &main_db, "main", &mut journal, fault).await?;
    }

    let agents_dir = feanorfs.join("agents");
    if agents_dir.exists() {
        let mut agent_keys: Vec<_> = std::fs::read_dir(&agents_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join(".feanorfs").join("local_cache.db").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for key in journal.stores.keys() {
            if key.starts_with("agents/") {
                let n = key.strip_prefix("agents/").unwrap_or(key);
                if !agent_keys.contains(&n.to_string()) {
                    agent_keys.push(n.to_string());
                }
            }
        }
        agent_keys.sort();
        agent_keys.dedup();
        for name in agent_keys {
            let db = agents_dir
                .join(&name)
                .join(".feanorfs")
                .join("local_cache.db");
            migrate_cache_store(root, &db, &format!("agents/{name}"), &mut journal, fault).await?;
        }
    }

    let hub_db = feanorfs.join("hub-data").join("db.sqlite");
    if hub_db.exists() || journal.stores.contains_key("hub") {
        migrate_hub_store(root, &hub_db, "hub", &mut journal, fault).await?;
    }

    drop(lock_file);
    Ok(())
}
