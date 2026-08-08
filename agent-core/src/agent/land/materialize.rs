use anyhow::Result;
use feanorfs_common::{FileState, LandedPath, SyncResponse};
use std::collections::HashMap;

use super::super::diff::AgentDiff;
use super::publish::inject_land_failure;
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::snapshot::SnapshotEngine;

pub(super) struct MaterializeInput<'a, 'ctx> {
    pub(super) ctx: &'a SyncCtx<'ctx>,
    pub(super) name: &'a str,
    pub(super) gate_local: &'a HashMap<String, FileState>,
}

pub(super) async fn materialize_land(
    input: MaterializeInput<'_, '_>,
    diff: &AgentDiff,
) -> Result<Vec<LandedPath>> {
    let mut landed = Vec::new();
    let mut landed_states = HashMap::new();
    let main_read_root = crate::workspace_read::WorkspaceReadRoot::open(input.ctx.base)?;
    for change in &diff.our_changes {
        let main_path = input.ctx.base.join(&change.path);
        if main_path.exists() && !change.deleted {
            if let Some(gate) = input.gate_local.get(&change.path) {
                let current_hash = if crate::large_file::uses_chunk_transport(gate.size) {
                    match crate::large_file::fingerprint(
                        input.ctx.base,
                        input.ctx.password_str(),
                        &change.path,
                    ) {
                        Ok(fingerprint) => fingerprint.encrypted_hash,
                        Err(error) => {
                            tracing::warn!("failed to fingerprint {}: {error}", change.path);
                            landed.push(LandedPath {
                                path: change.path.clone(),
                                action: "diverted: failed to read folder file".to_string(),
                            });
                            continue;
                        }
                    }
                } else {
                    let current = match main_read_root
                        .read_regular_stable(&change.path, crate::large_file::CHUNK_THRESHOLD_BYTES)
                        .await
                    {
                        Ok((bytes, _)) => bytes,
                        Err(error) => {
                            tracing::warn!("failed to read {}: {error}", change.path);
                            landed.push(LandedPath {
                                path: change.path.clone(),
                                action: "diverted: failed to read folder file".to_string(),
                            });
                            continue;
                        }
                    };
                    seal(&current, input.ctx.password_str(), &change.path)?.0
                };
                if current_hash != gate.hash {
                    landed.push(LandedPath {
                        path: change.path.clone(),
                        action: "diverted: folder changed during land".to_string(),
                    });
                    continue;
                }
            }
        }
        landed_states.insert(change.path.clone(), change.clone());
    }

    let response = SyncResponse {
        upload_required: Vec::new(),
        download_required: landed_states
            .values()
            .filter(|state| !state.deleted)
            .cloned()
            .collect(),
        delete_local: landed_states
            .values()
            .filter(|state| state.deleted)
            .map(|state| state.path.clone())
            .collect(),
    };
    crate::sync_pass::process_downloads(input.ctx, &response, input.gate_local, false).await?;
    crate::sync_pass::process_delete_local(&response, input.ctx.base, input.ctx.db).await?;
    for change in landed_states.values() {
        landed.push(LandedPath {
            path: change.path.clone(),
            action: if change.deleted { "deleted" } else { "updated" }.to_string(),
        });
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
        crate::sync_pass::process_uploads(input.ctx, &upload_response, &local_after, false).await?;
        SnapshotEngine::new(input.ctx)
            .record_last_synced(&local_after, "land")
            .await?;
    }
    Ok(landed)
}
