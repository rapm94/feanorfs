use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use super::{
    HubDb, HubStateV1, LegacyFileV1, ManifestV1, MigrationHubFence, MigrationHubFile,
    MigrationHubManifest, MigrationHubState, MigrationHubWorkspace, WorkspaceMetaV1,
};
use crate::durable::DurableJson;

impl HubDb {
    #[doc(hidden)]
    pub fn open_for_migration(data_dir: &Path) -> Result<Self> {
        let storage_dir = data_dir.to_path_buf();
        std::fs::create_dir_all(storage_dir.join("blobs")).context("create blobs directory")?;
        let state = DurableJson::open(&storage_dir, "hub_state.json", HubStateV1::default())?;
        Ok(Self { state, storage_dir })
    }

    #[doc(hidden)]
    pub fn replace_from_migration(&self, source: &MigrationHubState) -> Result<()> {
        self.state.with_write(|state| {
            state.workspaces.clear();
            for (workspace_id, workspace) in &source.workspaces {
                let manifests = workspace
                    .manifests
                    .iter()
                    .map(|(snapshot_id, manifest)| {
                        (
                            snapshot_id.clone(),
                            ManifestV1 {
                                hashes: manifest.hashes.clone(),
                                created_at_ms: manifest.created_at_ms,
                            },
                        )
                    })
                    .collect();
                let files = workspace
                    .files
                    .iter()
                    .map(|(path, file)| {
                        (
                            path.clone(),
                            LegacyFileV1 {
                                hash: file.hash.clone(),
                                size: feanorfs_common::file_size_to_db(file.size),
                                mtime: file.mtime,
                                mode: file.mode,
                                deleted: file.deleted,
                            },
                        )
                    })
                    .collect();
                state.workspaces.insert(
                    workspace_id.clone(),
                    WorkspaceMetaV1 {
                        format_version: workspace.format_version,
                        head: workspace.head.clone(),
                        manifests,
                        files,
                        migration_fence: workspace
                            .migration_fence
                            .as_ref()
                            .map(|fence| fence.token.clone()),
                        fence_locked_at: workspace
                            .migration_fence
                            .as_ref()
                            .map_or(0, |fence| fence.locked_at),
                    },
                );
            }
            Ok(())
        })
    }

    #[doc(hidden)]
    pub fn export_for_migration(&self) -> Result<MigrationHubState> {
        self.state.with_read(|state| {
            let workspaces = state
                .workspaces
                .iter()
                .map(|(workspace_id, workspace)| {
                    let manifests = workspace
                        .manifests
                        .iter()
                        .map(|(snapshot_id, manifest)| {
                            (
                                snapshot_id.clone(),
                                MigrationHubManifest {
                                    hashes: manifest.hashes.clone(),
                                    created_at_ms: manifest.created_at_ms,
                                },
                            )
                        })
                        .collect::<BTreeMap<_, _>>();
                    let files = workspace
                        .files
                        .iter()
                        .map(|(path, file)| {
                            (
                                path.clone(),
                                MigrationHubFile {
                                    hash: file.hash.clone(),
                                    size: feanorfs_common::file_size_from_db(file.size),
                                    mtime: file.mtime,
                                    mode: file.mode,
                                    deleted: file.deleted,
                                },
                            )
                        })
                        .collect::<BTreeMap<_, _>>();
                    (
                        workspace_id.clone(),
                        MigrationHubWorkspace {
                            format_version: workspace.format_version,
                            head: workspace.head.clone(),
                            manifests,
                            files,
                            migration_fence: workspace.migration_fence.as_ref().map(|token| {
                                MigrationHubFence {
                                    token: token.clone(),
                                    locked_at: workspace.fence_locked_at,
                                }
                            }),
                        },
                    )
                })
                .collect();
            Ok(MigrationHubState { workspaces })
        })
    }
}
