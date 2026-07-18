use anyhow::Result;

use super::ClientDb;
use crate::state::{ConflictRecordV1, ConflictResolutionV1};

impl ClientDb {
    pub async fn list_pending_conflict_paths(&self) -> Result<Vec<String>> {
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .values()
                .filter(|record| record.status == "pending")
                .map(|record| record.path.clone())
                .collect())
        })
    }

    pub async fn list_conflict_records(&self) -> Result<Vec<feanorfs_common::ConflictRecord>> {
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .values()
                .filter(|record| record.status == "pending")
                .map(public_record)
                .collect())
        })
    }

    pub async fn get_conflict_record(
        &self,
        path: &str,
    ) -> Result<Option<feanorfs_common::ConflictRecord>> {
        let path = path.to_string();
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .get(&path)
                .filter(|record| record.status == "pending")
                .map(public_record))
        })
    }

    pub async fn upsert_conflict(
        &self,
        path: &str,
        kind: &feanorfs_common::ConflictKind,
        conflict_dir: &str,
        opened_at: i64,
        status: &str,
    ) -> Result<()> {
        let path = path.to_string();
        let record = ConflictRecordV1 {
            path: path.clone(),
            kind: *kind,
            conflict_dir: conflict_dir.to_string(),
            opened_at,
            status: status.to_string(),
        };
        self.state.with_write(|state| {
            state.conflict_registry.insert(path, record);
            Ok(())
        })
    }

    pub async fn resolve_conflict_path(&self, path: &str) -> Result<()> {
        let path = path.to_string();
        self.state.with_write(|state| {
            state.conflict_registry.remove(&path);
            Ok(())
        })
    }

    pub async fn resolve_conflict_paths_with_history(
        &self,
        paths: &[String],
        method: &str,
        resolver: &str,
    ) -> Result<()> {
        let paths = paths.to_vec();
        let method = method.to_string();
        let resolver = resolver.to_string();
        let resolved_at = chrono::Utc::now().timestamp_millis();
        self.state.with_write(|state| {
            for path in paths {
                state.conflict_registry.remove(&path);
                state.conflict_resolutions.push(ConflictResolutionV1 {
                    path,
                    method: method.clone(),
                    source_file_hash: None,
                    resolved_at,
                    resolver: resolver.clone(),
                });
            }
            Ok(())
        })
    }

    pub async fn count_pending_in_dir(&self, conflict_dir: &str) -> Result<u32> {
        let conflict_dir = conflict_dir.to_string();
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .values()
                .filter(|record| record.conflict_dir == conflict_dir && record.status == "pending")
                .count() as u32)
        })
    }

    pub async fn record_conflict_resolution(
        &self,
        path: &str,
        method: &str,
        source_file_hash: Option<&str>,
        resolver: &str,
    ) -> Result<()> {
        let record = ConflictResolutionV1 {
            path: path.to_string(),
            method: method.to_string(),
            source_file_hash: source_file_hash.map(String::from),
            resolved_at: chrono::Utc::now().timestamp_millis(),
            resolver: resolver.to_string(),
        };
        self.state.with_write(|state| {
            state.conflict_resolutions.push(record);
            Ok(())
        })
    }

    pub async fn list_conflict_resolutions(
        &self,
    ) -> Result<Vec<feanorfs_common::ConflictResolution>> {
        self.state.with_read(|state| {
            let mut records = state
                .conflict_resolutions
                .iter()
                .map(|record| feanorfs_common::ConflictResolution {
                    path: record.path.clone(),
                    method: record.method.clone(),
                    source_file_hash: record.source_file_hash.clone(),
                    resolved_at: record.resolved_at,
                    resolver: record.resolver.clone(),
                })
                .collect::<Vec<_>>();
            records.sort_by_key(|record| std::cmp::Reverse(record.resolved_at));
            Ok(records)
        })
    }
}

fn public_record(record: &ConflictRecordV1) -> feanorfs_common::ConflictRecord {
    feanorfs_common::ConflictRecord {
        path: record.path.clone(),
        kind: record.kind,
        conflict_dir: record.conflict_dir.clone(),
        opened_at: record.opened_at,
        status: record.status.clone(),
    }
}
