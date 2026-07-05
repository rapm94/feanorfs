use crate::api::ApiClient;
use crate::fs_util::atomic_write;
use crate::local::{CacheEntry, ClientDb};
use anyhow::Result;
use feanorfs_common::crypt_bytes;
use std::path::Path;
use tokio::fs;

const DEFAULT_NEIGHBORS: usize = 5;
const DECAY_FACTOR: f64 = 0.95;

#[derive(Debug, Default, serde::Serialize)]
pub struct PrefetchReport {
    pub inspected: Vec<String>,
    pub hydrated: Vec<String>,
    pub skipped: Vec<String>,
}

/// Record that `path` was accessed, bumping co-occurrence weights against
/// the recently accessed paths stored in the `last_session` table. Maintains
/// a rolling list of the last 5 accessed paths so cross-weights accumulate
/// across calls.
pub async fn record_access_with_recent(db: &ClientDb, path: &str) -> Result<()> {
    let recent: Vec<String> = match db.get_session_key("recent_paths").await? {
        Some(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!("recent_paths session state is corrupt, resetting: {e}");
            Vec::new()
        }),
        None => Vec::new(),
    };

    record_access(db, path, &recent).await?;

    let mut updated = vec![path.to_string()];
    updated.extend(recent.into_iter().filter(|p| p != path).take(4));
    db.set_session_key("recent_paths", &serde_json::to_string(&updated)?)
        .await?;

    Ok(())
}

/// Record that `path` was accessed (opened/cat'd/hydrated). Bumps the
/// co-occurrence weight of every sibling the user has touched recently.
pub async fn record_access(db: &ClientDb, path: &str, recent: &[String]) -> Result<()> {
    for sibling in recent {
        if sibling == path {
            continue;
        }
        db.record_access_pair(path, sibling, 1.0).await?;
    }
    Ok(())
}

/// Background task: for every hydrated placeholder currently on disk,
/// look up the top-N co-occurring siblings and fetch them. Skips siblings
/// that are already hydrated or absent from the cache. Mutates nothing
/// outside the local filesystem and the access log.
pub async fn prefetch_related(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    password: Option<&str>,
    seed_paths: &[String],
) -> Result<PrefetchReport> {
    let password_str = password.unwrap_or(feanorfs_common::LEGACY_DEFAULT_PASSWORD);
    let mut report = PrefetchReport::default();
    let cache = db.get_cache_entries().await?;

    for seed in seed_paths {
        report.inspected.push(seed.clone());
        if let Some(seed_entry) = cache.get(seed) {
            if seed_entry.hydrated {
                report.skipped.push(seed.clone());
                continue;
            }
            if hydrate_one(base, api, db, password_str, seed_entry).await? {
                report.hydrated.push(seed.clone());
            }
        }

        let siblings = db.get_predictive_siblings(seed, DEFAULT_NEIGHBORS).await?;
        for (sibling_path, _weight) in siblings {
            if let Some(entry) = cache.get(&sibling_path) {
                if entry.hydrated {
                    continue;
                }
                if hydrate_one(base, api, db, password_str, entry).await? {
                    report.hydrated.push(sibling_path);
                }
            }
        }
    }

    db.decay_access_log(DECAY_FACTOR).await?;
    Ok(report)
}

async fn hydrate_one(
    base: &Path,
    api: &ApiClient,
    db: &ClientDb,
    password: &str,
    entry: &CacheEntry,
) -> Result<bool> {
    let Ok(encrypted) = api.download_file(&entry.encrypted_hash).await else {
        return Ok(false);
    };
    let computed_hash = feanorfs_common::hash_bytes(&encrypted);
    if computed_hash != entry.encrypted_hash {
        tracing::warn!(
            "Integrity check failed for predictive prefetch of {}: expected {}, computed {}",
            entry.path,
            entry.encrypted_hash,
            computed_hash
        );
        return Ok(false);
    }
    let plain = crypt_bytes(&encrypted, password, &entry.path);
    atomic_write(base, &entry.path, &plain).await?;

    let full = base.join(&entry.path);
    let actual_mtime = fs::metadata(&full)
        .await?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(entry.server_mtime);

    let plaintext_hash = feanorfs_common::hash_bytes(&plain);

    db.upsert_cache_entry(&CacheEntry {
        path: entry.path.clone(),
        plaintext_hash,
        encrypted_hash: entry.encrypted_hash.clone(),
        size: plain.len() as u64,
        mtime: actual_mtime,
        server_mtime: entry.server_mtime,
        hydrated: true,
    })
    .await?;

    Ok(true)
}
