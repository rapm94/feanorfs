use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use super::{CacheEntry, ClientDb};
use crate::state::{
    self, AccessEntryV1, CacheEntryV1, ConflictRecordV1, ConflictResolutionV1, DurableState,
};

impl ClientDb {
    pub async fn new<P: AsRef<Path>>(fs_dir: P) -> Result<Self> {
        let fs_dir = fs_dir.as_ref();
        state::check_no_legacy_db(fs_dir)?;
        Ok(Self {
            state: DurableState::new(fs_dir)?,
        })
    }

    pub async fn get_cache_entries(&self) -> Result<HashMap<String, CacheEntry>> {
        self.state.with_read(|state| {
            Ok(state
                .local_files
                .iter()
                .map(|(path, entry)| {
                    (
                        path.clone(),
                        CacheEntry {
                            path: path.clone(),
                            plaintext_hash: entry.plaintext_hash.clone(),
                            encrypted_hash: entry.encrypted_hash.clone(),
                            size: feanorfs_common::file_size_from_db(entry.size),
                            mtime: entry.mtime,
                            server_mtime: entry.server_mtime,
                            mode: u32::try_from(entry.mode).unwrap_or(0),
                            hydrated: entry.hydrated,
                            deleted_at: entry.deleted_at,
                        },
                    )
                })
                .collect())
        })
    }

    pub async fn upsert_cache_entry(&self, entry: &CacheEntry) -> Result<()> {
        let stored = cache_to_v1(entry);
        self.state.with_write(|state| {
            state.local_files.insert(entry.path.clone(), stored);
            Ok(())
        })
    }

    pub async fn delete_cache_entry(&self, path: &str) -> Result<()> {
        let path = path.to_string();
        self.state.with_write(|state| {
            state.local_files.remove(&path);
            Ok(())
        })
    }

    pub async fn delete_cache_entries(&self, paths: &[String]) -> Result<()> {
        self.state.with_write(|state| {
            for path in paths {
                state.local_files.remove(path);
            }
            Ok(())
        })
    }

    pub async fn set_deleted_at(&self, path: &str, deleted_at: i64) -> Result<()> {
        let path = path.to_string();
        self.state.with_write(|state| {
            if let Some(entry) = state.local_files.get_mut(&path) {
                entry.deleted_at = Some(deleted_at);
            }
            Ok(())
        })
    }

    pub async fn set_cache_server_mtime(&self, path: &str, server_mtime: i64) -> Result<()> {
        let path = path.to_string();
        self.state.with_write(|state| {
            if let Some(entry) = state.local_files.get_mut(&path) {
                entry.server_mtime = server_mtime;
            }
            Ok(())
        })
    }

    pub async fn bulk_upsert_cache_entries(&self, entries: &[CacheEntry]) -> Result<()> {
        self.state.with_write(|state| {
            for entry in entries {
                state
                    .local_files
                    .insert(entry.path.clone(), cache_to_v1(entry));
            }
            Ok(())
        })
    }

    pub async fn drop_legacy_snapshot_tables(&self) -> Result<()> {
        Ok(())
    }

    #[doc(hidden)]
    pub async fn open_for_migration<P: AsRef<Path>>(fs_dir: P) -> Result<Self> {
        let fs_dir = fs_dir.as_ref();
        fs::create_dir_all(fs_dir)?;
        Ok(Self {
            state: DurableState::new(fs_dir)?,
        })
    }

    #[doc(hidden)]
    pub async fn replace_from_migration(
        &self,
        dto: &crate::state::MigrationLocalState,
    ) -> Result<()> {
        self.state.with_write(|state| {
            state.local_files.clear();
            state.file_access_log.clear();
            state.last_session.clear();
            state.conflict_registry.clear();
            state.conflict_resolutions.clear();

            state
                .local_files
                .extend(dto.local_files.iter().map(|(path, entry)| {
                    (
                        path.clone(),
                        CacheEntryV1 {
                            plaintext_hash: entry.plaintext_hash.clone(),
                            encrypted_hash: entry.encrypted_hash.clone(),
                            size: feanorfs_common::file_size_to_db(entry.size),
                            mtime: entry.mtime,
                            server_mtime: entry.server_mtime,
                            mode: i32::try_from(entry.mode).unwrap_or(0),
                            hydrated: entry.hydrated,
                            deleted_at: entry.deleted_at,
                        },
                    )
                }));
            state
                .file_access_log
                .extend(dto.file_access_log.iter().map(|entry| AccessEntryV1 {
                    path: entry.path.clone(),
                    sibling_path: entry.sibling_path.clone(),
                    weight: entry.weight,
                    updated_at: entry.updated_at,
                }));
            state.last_session.clone_from(&dto.last_session);
            state
                .conflict_registry
                .extend(dto.conflict_registry.iter().map(|(path, record)| {
                    (
                        path.clone(),
                        ConflictRecordV1 {
                            path: record.path.clone(),
                            kind: record.kind,
                            conflict_dir: record.conflict_dir.clone(),
                            opened_at: record.opened_at,
                            status: record.status.clone(),
                        },
                    )
                }));
            state
                .conflict_resolutions
                .extend(
                    dto.conflict_resolutions
                        .iter()
                        .map(|record| ConflictResolutionV1 {
                            path: record.path.clone(),
                            method: record.method.clone(),
                            source_file_hash: record.source_file_hash.clone(),
                            resolved_at: record.resolved_at,
                            resolver: record.resolver.clone(),
                        }),
                );
            Ok(())
        })
    }

    #[doc(hidden)]
    pub async fn export_for_migration(&self) -> Result<crate::state::MigrationLocalState> {
        self.state.with_read(|state| {
            let local_files = state
                .local_files
                .iter()
                .map(|(path, entry)| {
                    (
                        path.clone(),
                        crate::state::MigrationCacheEntry {
                            plaintext_hash: entry.plaintext_hash.clone(),
                            encrypted_hash: entry.encrypted_hash.clone(),
                            size: feanorfs_common::file_size_from_db(entry.size),
                            mtime: entry.mtime,
                            server_mtime: entry.server_mtime,
                            mode: u32::try_from(entry.mode).unwrap_or(0),
                            hydrated: entry.hydrated,
                            deleted_at: entry.deleted_at,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let file_access_log = state
                .file_access_log
                .iter()
                .map(|entry| crate::state::MigrationAccessEntry {
                    path: entry.path.clone(),
                    sibling_path: entry.sibling_path.clone(),
                    weight: entry.weight,
                    updated_at: entry.updated_at,
                })
                .collect();
            let conflict_registry = state
                .conflict_registry
                .iter()
                .map(|(path, record)| {
                    (
                        path.clone(),
                        crate::state::MigrationConflictRecord {
                            path: record.path.clone(),
                            kind: record.kind,
                            conflict_dir: record.conflict_dir.clone(),
                            opened_at: record.opened_at,
                            status: record.status.clone(),
                        },
                    )
                })
                .collect();
            let conflict_resolutions = state
                .conflict_resolutions
                .iter()
                .map(|record| crate::state::MigrationConflictResolution {
                    path: record.path.clone(),
                    method: record.method.clone(),
                    source_file_hash: record.source_file_hash.clone(),
                    resolved_at: record.resolved_at,
                    resolver: record.resolver.clone(),
                })
                .collect();
            Ok(crate::state::MigrationLocalState {
                local_files,
                file_access_log,
                last_session: state.last_session.clone(),
                conflict_registry,
                conflict_resolutions,
            })
        })
    }
}

fn cache_to_v1(entry: &CacheEntry) -> CacheEntryV1 {
    CacheEntryV1 {
        plaintext_hash: entry.plaintext_hash.clone(),
        encrypted_hash: entry.encrypted_hash.clone(),
        size: feanorfs_common::file_size_to_db(entry.size),
        mtime: entry.mtime,
        server_mtime: entry.server_mtime,
        mode: i32::try_from(entry.mode).unwrap_or(0),
        hydrated: entry.hydrated,
        deleted_at: entry.deleted_at,
    }
}
