use crate::api::ApiClient;
use crate::local::ClientDb;
use anyhow::Result;
use feanorfs_common::{FileState, SyncResponse};
use std::collections::BTreeMap;
use std::path::Path;

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
    if password.is_none() {
        tracing::warn!(
            "No E2EE password set; using insecure legacy default for predictive hydration."
        );
    }
    let work = async {
        let mut report = PrefetchReport::default();
        let ctx = feanorfs_agent_core::sync_pass::build_ctx_or_fallback(
            api,
            db,
            base,
            "",
            Some(password_str),
        )?;
        let _sync_guard = feanorfs_agent_core::lock::SyncLock::acquire(base)?;
        let local_files = crate::local::scan_local_directory(base, db, Some(password_str)).await?;
        let cache = db.get_cache_entries().await?;
        let mut candidates = BTreeMap::new();

        for seed in seed_paths {
            report.inspected.push(seed.clone());
            if let Some(seed_entry) = cache.get(seed) {
                if seed_entry.hydrated || seed_entry.deleted_at.is_some() {
                    report.skipped.push(seed.clone());
                } else if local_files
                    .get(seed)
                    .is_some_and(|state| !state.deleted && state.hash == seed_entry.encrypted_hash)
                {
                    candidates.entry(seed.clone()).or_insert_with(|| FileState {
                        path: seed_entry.path.clone(),
                        hash: seed_entry.encrypted_hash.clone(),
                        size: seed_entry.size,
                        mtime: seed_entry.server_mtime,
                        deleted: false,
                        mode: seed_entry.mode,
                    });
                }
            }

            let siblings = db.get_predictive_siblings(seed, DEFAULT_NEIGHBORS).await?;
            for (sibling_path, _weight) in siblings {
                let Some(entry) = cache.get(&sibling_path) else {
                    continue;
                };
                if entry.hydrated || entry.deleted_at.is_some() {
                    continue;
                }
                if local_files
                    .get(&sibling_path)
                    .is_some_and(|state| !state.deleted && state.hash == entry.encrypted_hash)
                {
                    candidates.entry(sibling_path).or_insert_with(|| FileState {
                        path: entry.path.clone(),
                        hash: entry.encrypted_hash.clone(),
                        size: entry.size,
                        mtime: entry.server_mtime,
                        deleted: false,
                        mode: entry.mode,
                    });
                }
            }
        }

        if !candidates.is_empty() {
            let hydrated = candidates.keys().cloned().collect::<Vec<_>>();
            let response = SyncResponse {
                upload_required: Vec::new(),
                download_required: candidates.into_values().collect(),
                delete_local: Vec::new(),
            };
            feanorfs_agent_core::sync_pass::process_downloads(&ctx, &response, &local_files, false)
                .await?;
            report.hydrated = hydrated;
        }
        Ok::<_, anyhow::Error>(report)
    }
    .await;

    let decay = db.decay_access_log(DECAY_FACTOR).await;
    match work {
        Ok(report) => {
            decay?;
            Ok(report)
        }
        Err(error) => {
            if let Err(decay_error) = decay {
                tracing::warn!("predictive access-log decay also failed: {decay_error:#}");
            }
            Err(error)
        }
    }
}
