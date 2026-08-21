use anyhow::Result;

use super::ClientDb;
use crate::conflict_artifacts::read_identity_sidecar;
use crate::state::{
    ConflictRecordStatus, ConflictRecordV1, ConflictResolutionV1, ResolutionMethod,
};
use std::path::Path;

impl ClientDb {
    pub async fn list_pending_conflict_paths(&self) -> Result<Vec<String>> {
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .values()
                .filter(|record| record.status.is_pending())
                .map(|record| record.path.clone())
                .collect())
        })
    }

    pub async fn list_conflict_records(&self) -> Result<Vec<feanorfs_common::ConflictRecord>> {
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .values()
                .filter(|record| record.status.is_pending())
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
                .filter(|record| record.status.is_pending())
                .map(public_record))
        })
    }

    pub async fn upsert_conflict(
        &self,
        path: &str,
        kind: &feanorfs_common::ConflictKind,
        conflict_dir: &str,
        opened_at: i64,
        status: ConflictRecordStatus,
    ) -> Result<()> {
        let path = path.to_string();
        let record = ConflictRecordV1 {
            path: path.clone(),
            kind: *kind,
            conflict_dir: conflict_dir.to_string(),
            opened_at,
            status,
            conflict_fingerprint: None,
        };
        self.state.with_write(|state| {
            state.conflict_registry.insert(path, record);
            Ok(())
        })
    }

    /// Records one fingerprinted pending conflict: persists the exact
    /// identity/fingerprint beside its artifacts as ONE fingerprint-keyed
    /// sidecar per conflict (never overwriting a different conflict's
    /// sidecar in the same directory) and stores a pending record bound to
    /// that fingerprint.
    pub async fn upsert_conflict_fingerprinted(
        &self,
        path: &str,
        kind: &feanorfs_common::ConflictKind,
        conflict_dir: &str,
        opened_at: i64,
        identity: &feanorfs_common::ConflictIdentity,
        fingerprint: &str,
    ) -> Result<()> {
        let dir = Path::new(conflict_dir).to_path_buf();
        crate::conflict_artifacts::write_identity_sidecar(&dir, identity, fingerprint).await?;
        let path = path.to_string();
        let record = ConflictRecordV1 {
            path: path.clone(),
            kind: *kind,
            conflict_dir: conflict_dir.to_string(),
            opened_at,
            status: ConflictRecordStatus::Pending,
            conflict_fingerprint: Some(fingerprint.to_string()),
        };
        self.state.with_write(|state| {
            state.conflict_registry.insert(path, record);
            Ok(())
        })
    }

    /// Whether a pending record is fingerprinted: the record carries a
    /// fingerprint, the fingerprint-keyed identity sidecar exists beside its
    /// artifacts, the sidecar's identity path matches the record's path, and
    /// the recomputed fingerprint matches. Legacy path-only records, corrupt
    /// sidecars, and sidecars that do not match the record are never
    /// automatic candidates.
    pub async fn is_conflict_fingerprinted(&self, path: &str) -> Result<bool> {
        let path = path.to_string();
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .get(&path)
                .filter(|record| record.status == ConflictRecordStatus::Pending)
                .is_some_and(sidecar_matches_record))
        })
    }

    /// Pending paths that are eligible for automatic prepare/apply: pending
    /// status plus a fingerprint-keyed identity sidecar that matches the
    /// record beside its artifacts.
    pub async fn list_automatic_candidates(&self) -> Result<Vec<String>> {
        self.state.with_read(|state| {
            Ok(state
                .conflict_registry
                .values()
                .filter(|record| record.status == ConflictRecordStatus::Pending)
                .filter(|record| sidecar_matches_record(record))
                .map(|record| record.path.clone())
                .collect())
        })
    }

    /// Migrates legacy path-only records to explicit `legacy_unfingerprinted`
    /// status: every pending record whose conflict directory has no valid,
    /// record-matching fingerprint-keyed identity sidecar is rewritten
    /// durably. Returns the number migrated.
    pub async fn migrate_legacy_conflict_statuses(&self) -> Result<usize> {
        self.state.with_write(|state| {
            let mut migrated = 0usize;
            for record in state.conflict_registry.values_mut() {
                if record.status != ConflictRecordStatus::Pending {
                    continue;
                }
                if !sidecar_matches_record(record) {
                    record.status = ConflictRecordStatus::LegacyUnfingerprinted;
                    migrated = migrated.saturating_add(1);
                }
            }
            Ok(migrated)
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
        method: ResolutionMethod,
        resolver: &str,
    ) -> Result<()> {
        let paths = paths.to_vec();
        let resolver = resolver.to_string();
        let resolved_at = chrono::Utc::now().timestamp_millis();
        self.state.with_write(|state| {
            for path in paths {
                state.conflict_registry.remove(&path);
                state.conflict_resolutions.push(ConflictResolutionV1 {
                    path,
                    method,
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
                .filter(|record| record.conflict_dir == conflict_dir && record.status.is_pending())
                .count() as u32)
        })
    }

    pub async fn record_conflict_resolution(
        &self,
        path: &str,
        method: ResolutionMethod,
        source_file_hash: Option<&str>,
        resolver: &str,
    ) -> Result<()> {
        let record = ConflictResolutionV1 {
            path: path.to_string(),
            method,
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
                    method: record.method.as_db_str().to_string(),
                    source_file_hash: record.source_file_hash.clone(),
                    resolved_at: record.resolved_at,
                    resolver: record.resolver.clone(),
                })
                .collect::<Vec<_>>();
            // Stable ascending sort plus reverse keeps most-recent-first order
            // and breaks equal-millisecond ties by latest insertion first.
            records.sort_by_key(|record| record.resolved_at);
            records.reverse();
            Ok(records)
        })
    }
}

/// Whether the record is bound to a fingerprint-keyed identity sidecar that
/// verifies against BOTH the record's stored fingerprint AND its path: the
/// sidecar exists for the fingerprint, its recomputed fingerprint matches,
/// and its identity path matches the record's path. Any mismatch fails
/// closed — the record is treated as legacy manual-only.
fn sidecar_matches_record(record: &ConflictRecordV1) -> bool {
    let Some(fingerprint) = record.conflict_fingerprint.as_deref() else {
        return false;
    };
    read_identity_sidecar(Path::new(&record.conflict_dir), fingerprint)
        .is_some_and(|(identity, _)| identity.path == record.path)
}

/// Effective public status: a pending record with a record-matching
/// fingerprint-keyed identity sidecar displays as `pending`; every other
/// pending record (path-only, corrupt sidecar, sidecar/record mismatch)
/// displays as `legacy_unfingerprinted`; unknown statuses stay `unknown`.
fn effective_status(record: &ConflictRecordV1) -> &'static str {
    match record.status {
        ConflictRecordStatus::Pending => {
            if sidecar_matches_record(record) {
                "pending"
            } else {
                "legacy_unfingerprinted"
            }
        }
        ConflictRecordStatus::LegacyUnfingerprinted => "legacy_unfingerprinted",
        ConflictRecordStatus::Unknown => "unknown",
    }
}

fn public_record(record: &ConflictRecordV1) -> feanorfs_common::ConflictRecord {
    feanorfs_common::ConflictRecord {
        path: record.path.clone(),
        kind: record.kind,
        conflict_dir: record.conflict_dir.clone(),
        opened_at: record.opened_at,
        status: effective_status(record).to_string(),
    }
}
