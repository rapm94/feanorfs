use anyhow::Result;
use feanorfs_common::{FileState, LandedPath, SyncResponse};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

use super::super::diff::AgentDiff;
use super::publish::inject_land_failure;
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::fs_util::atomic_write;
use crate::snapshot::SnapshotEngine;

pub(super) struct MaterializeInput<'a, 'ctx> {
    pub(super) ctx: &'a SyncCtx<'ctx>,
    pub(super) name: &'a str,
    pub(super) agent_path: &'a Path,
    pub(super) gate_local: &'a HashMap<String, FileState>,
}

pub(super) async fn materialize_land(
    input: MaterializeInput<'_, '_>,
    diff: &AgentDiff,
) -> Result<Vec<LandedPath>> {
    let mut landed = Vec::new();
    let mut landed_states = HashMap::new();
    for change in &diff.our_changes {
        let main_path = input.ctx.base.join(&change.path);
        if main_path.exists() && !change.deleted {
            if let Some(gate) = input.gate_local.get(&change.path) {
                let current = match fs::read(&main_path).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!("failed to read {}: {error}", change.path);
                        landed.push(LandedPath {
                            path: change.path.clone(),
                            action: "diverted: failed to read folder file".to_string(),
                        });
                        continue;
                    }
                };
                let (current_hash, _) = seal(&current, input.ctx.password_str(), &change.path)?;
                if current_hash != gate.hash
                    || u64::try_from(current.len()).unwrap_or(u64::MAX) != gate.size
                {
                    landed.push(LandedPath {
                        path: change.path.clone(),
                        action: "diverted: folder changed during land".to_string(),
                    });
                    continue;
                }
            }
        }
        if change.deleted {
            if main_path.exists() {
                fs::remove_file(&main_path).await?;
            }
            input.ctx.db.delete_cache_entry(&change.path).await?;
            landed.push(LandedPath {
                path: change.path.clone(),
                action: "deleted".to_string(),
            });
            landed_states.insert(change.path.clone(), change.clone());
        } else {
            let bytes = fs::read(input.agent_path.join(&change.path)).await?;
            atomic_write(input.ctx.base, &change.path, &bytes).await?;
            landed.push(LandedPath {
                path: change.path.clone(),
                action: "updated".to_string(),
            });
            landed_states.insert(change.path.clone(), change.clone());
        }
    }
    inject_land_failure(input.ctx.base, input.name, "after-materialize").await?;
    if !landed_states.is_empty() {
        let upload_response = SyncResponse {
            upload_required: landed_states.keys().cloned().collect(),
            download_required: Vec::new(),
            delete_local: Vec::new(),
        };
        let mut local_after =
            crate::local::scan_local_directory(input.ctx.base, input.ctx.db, input.ctx.password())
                .await?;
        for change in landed_states.values().filter(|change| change.deleted) {
            local_after.insert(change.path.clone(), change.clone());
        }
        crate::sync_pass::process_uploads(input.ctx, &upload_response, &local_after).await?;
        SnapshotEngine::new(input.ctx)
            .record_last_synced(&local_after, "land")
            .await?;
    }
    Ok(landed)
}
