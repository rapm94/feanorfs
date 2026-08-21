use anyhow::{Context, Result};
use feanorfs_common::hub_contract::{ManifestWriteOutcome, MigrationWriteOutcome};
use std::path::Path;

use super::{FormatWrite, HubDb, HubStateV1, LegacyFileV1, ManifestV1, CURRENT_HUB_SCHEMA};
use crate::durable::{self, DurableJson};

impl HubDb {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let storage_dir = data_dir.to_path_buf();
        std::fs::create_dir_all(storage_dir.join("blobs")).context("create blobs directory")?;
        let state = DurableJson::open(&storage_dir, "hub_state.json", HubStateV1::default())?;
        state.with_read(|state| {
            if state.schema_version == 0 {
                return Err(crate::agent::continuous::unsupported_schema_failure(
                    anyhow::anyhow!("hub_state.json has invalid schema version 0"),
                ));
            }
            if state.schema_version > CURRENT_HUB_SCHEMA {
                return Err(crate::agent::continuous::unsupported_schema_failure(
                    anyhow::anyhow!(
                        "hub_state.json schema version {} is newer than supported (max {})",
                        state.schema_version,
                        CURRENT_HUB_SCHEMA
                    ),
                ));
            }
            Ok(())
        })?;
        Ok(Self { state, storage_dir })
    }

    pub fn blobs_dir(&self) -> std::path::PathBuf {
        self.storage_dir.join("blobs")
    }

    pub fn list_workspaces(&self) -> Result<Vec<String>> {
        self.state.with_read(|state| {
            let mut workspaces = state
                .workspaces
                .iter()
                .filter(|(_, workspace)| {
                    workspace.head.is_some() || workspace.files.values().any(|file| !file.deleted)
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            workspaces.sort();
            Ok(workspaces)
        })
    }

    pub fn get_files(&self, workspace_id: &str) -> Result<Vec<(String, LegacyFileV1)>> {
        self.state.with_read(|state| {
            Ok(state
                .workspaces
                .get(workspace_id)
                .map(|workspace| {
                    workspace
                        .files
                        .iter()
                        .map(|(path, file)| (path.clone(), file.clone()))
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_file(
        &self,
        workspace_id: &str,
        path: &str,
        hash: &str,
        size: i64,
        mtime: i64,
        mode: u32,
        deleted: bool,
    ) -> Result<()> {
        let workspace_id = workspace_id.to_string();
        let path = path.to_string();
        let file = LegacyFileV1 {
            hash: hash.to_string(),
            size,
            mtime,
            mode,
            deleted,
        };
        self.state.with_write(|state| {
            state
                .workspaces
                .entry(workspace_id)
                .or_default()
                .files
                .insert(path, file);
            Ok(())
        })
    }

    pub fn tombstone_existing_file(
        &self,
        workspace_id: &str,
        path: &str,
        hash: &str,
        mtime: i64,
    ) -> Result<bool> {
        let hash = hash.to_string();
        self.state.with_write(|state| {
            let Some(file) = state
                .workspaces
                .get_mut(workspace_id)
                .and_then(|workspace| workspace.files.get_mut(path))
            else {
                return Ok(false);
            };
            *file = LegacyFileV1 {
                hash,
                size: 0,
                mtime,
                mode: 0,
                deleted: true,
            };
            Ok(true)
        })
    }

    pub fn get_format(&self, workspace_id: &str) -> Result<u32> {
        self.state.with_read(|state| {
            Ok(state
                .workspaces
                .get(workspace_id)
                .map(|workspace| workspace.format_version)
                .unwrap_or(2))
        })
    }

    pub fn set_format(&self, workspace_id: &str, version: u32) -> Result<FormatWrite> {
        let workspace_key = workspace_id.to_string();
        self.state.with_write(|state| {
            let workspace = state.workspaces.entry(workspace_key).or_default();
            if version >= 3
                && !workspace.head.as_ref().is_some_and(|head| {
                    workspace.manifests.get(head).is_some_and(|manifest| {
                        feanorfs_common::canonical_manifest_hash_list(head, &manifest.hashes)
                            .is_ok()
                    })
                })
            {
                return Ok(FormatWrite::MissingManifestHead);
            }
            workspace.format_version = version;
            if version >= 3 {
                workspace.files.clear();
                workspace.migration_fence = None;
                workspace.fence_locked_at = 0;
            }
            Ok(FormatWrite::Applied)
        })
    }

    pub fn get_head(&self, workspace_id: &str) -> Result<Option<String>> {
        self.state.with_read(|state| {
            Ok(state
                .workspaces
                .get(workspace_id)
                .and_then(|workspace| workspace.head.clone()))
        })
    }

    pub fn swap_head(
        &self,
        workspace_id: &str,
        expected: Option<&str>,
        new: &str,
    ) -> Result<Option<String>> {
        let workspace_id = workspace_id.to_string();
        let expected = expected.map(str::to_string);
        let new = new.to_string();
        self.state.with_write(|state| {
            let workspace = state.workspaces.entry(workspace_id).or_default();
            let current = workspace.head.clone();
            if expected == current {
                workspace.head = Some(new);
            }
            Ok(current)
        })
    }

    pub fn manifest_exists(&self, workspace_id: &str, snapshot_id: &str) -> Result<bool> {
        self.state.with_read(|state| {
            Ok(state.workspaces.get(workspace_id).is_some_and(|workspace| {
                workspace
                    .manifests
                    .get(snapshot_id)
                    .is_some_and(|manifest| {
                        feanorfs_common::canonical_manifest_hash_list(snapshot_id, &manifest.hashes)
                            .is_ok()
                    })
            }))
        })
    }

    pub(crate) fn store_manifest(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        hashes: Vec<String>,
    ) -> Result<ManifestWriteOutcome> {
        let workspace_id = workspace_id.to_string();
        let snapshot_id = snapshot_id.to_string();
        self.state.with_write(|state| {
            let manifests = &mut state.workspaces.entry(workspace_id).or_default().manifests;
            if let Some(existing) = manifests.get(&snapshot_id) {
                return Ok(if existing.hashes == hashes {
                    ManifestWriteOutcome::Unchanged
                } else {
                    ManifestWriteOutcome::Conflict
                });
            }
            manifests.insert(
                snapshot_id,
                ManifestV1 {
                    hashes,
                    created_at_ms: chrono::Utc::now().timestamp_millis(),
                },
            );
            Ok(ManifestWriteOutcome::Stored)
        })
    }

    pub fn begin_migration(
        &self,
        workspace_id: &str,
        token: &str,
    ) -> Result<MigrationWriteOutcome> {
        let workspace_id = workspace_id.to_string();
        let token = token.to_string();
        self.state.with_write(|state| {
            let workspace = state.workspaces.entry(workspace_id).or_default();
            if workspace.format_version >= 3 {
                return Ok(MigrationWriteOutcome::Acquired);
            }
            match &workspace.migration_fence {
                Some(existing) if *existing == token => Ok(MigrationWriteOutcome::Acquired),
                Some(_) => Ok(MigrationWriteOutcome::LockedByOther),
                None => {
                    workspace.migration_fence = Some(token);
                    workspace.fence_locked_at = chrono::Utc::now().timestamp_millis();
                    Ok(MigrationWriteOutcome::Acquired)
                }
            }
        })
    }

    pub fn get_migration_fence(&self, workspace_id: &str) -> Result<Option<String>> {
        self.state.with_read(|state| {
            Ok(state
                .workspaces
                .get(workspace_id)
                .and_then(|workspace| workspace.migration_fence.clone()))
        })
    }

    pub fn blob_path(&self, hash: &str) -> std::path::PathBuf {
        self.blobs_dir().join(hash)
    }

    pub fn store_blob(&self, hash: &str, data: &[u8]) -> Result<()> {
        let path = self.blob_path(hash);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        durable::atomic_overwrite(&path, data)
    }

    pub fn blob_exists(&self, hash: &str) -> bool {
        self.blob_path(hash).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_legacy_rows_can_be_retired_but_not_invented() {
        let directory = tempfile::tempdir().unwrap();
        let db = HubDb::open(directory.path()).unwrap();
        let hash = "a".repeat(64);
        db.upsert_file("ws", ".jj/repo/store", &hash, 1, 1, 0, false)
            .unwrap();
        assert!(db
            .tombstone_existing_file("ws", ".jj/repo/store", &hash, 2)
            .unwrap());
        let file = db
            .get_files("ws")
            .unwrap()
            .into_iter()
            .find(|(path, _)| path == ".jj/repo/store")
            .unwrap()
            .1;
        assert!(file.deleted);
        assert_eq!(file.size, 0);
        assert!(!db
            .tombstone_existing_file("ws", "../invented", &hash, 2)
            .unwrap());
    }
}
