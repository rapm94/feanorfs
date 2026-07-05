use crate::db::Db;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;

#[derive(Debug, Default, Serialize)]
pub struct GcStats {
    pub blobs_deleted: u64,
    pub tombstones_purged: u64,
    pub bytes_freed: u64,
}

pub async fn run_gc(
    db: &Db,
    storage_dir: &Path,
    grace: Duration,
    tombstone_retention: Duration,
) -> Result<GcStats> {
    let live = db.get_referenced_hashes().await?;
    let live_set: HashSet<String> = live.into_iter().collect();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let tombstone_cutoff = now - tombstone_retention.as_millis() as i64;
    let tombstones_purged = db.purge_old_tombstones(tombstone_cutoff).await? as u64;

    let blobs_dir = storage_dir.join("blobs");
    let mut stats = GcStats {
        tombstones_purged,
        ..Default::default()
    };

    if !blobs_dir.is_dir() {
        return Ok(stats);
    }

    let mut entries = fs::read_dir(&blobs_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(hash) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if live_set.contains(hash) {
            continue;
        }
        let meta = fs::metadata(&path).await?;
        let age_ok = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|e| e >= grace);
        if !age_ok {
            continue;
        }
        let size = meta.len();
        fs::remove_file(&path).await?;
        stats.blobs_deleted += 1;
        stats.bytes_freed += size;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use feanorfs_common::FileState;
    use tempfile::TempDir;

    #[tokio::test]
    async fn gc_removes_orphan_blob_after_grace() {
        let data = TempDir::new().unwrap();
        let db_path = data.path().join("db.sqlite");
        let blobs = data.path().join("blobs");
        tokio::fs::create_dir_all(&blobs).await.unwrap();

        let db = Db::new(&db_path).await.unwrap();
        let live_hash = "a".repeat(64);
        db.upsert_file(
            "ws",
            &FileState {
                path: "live.txt".into(),
                hash: live_hash.clone(),
                size: 1,
                mtime: 1,
                deleted: false,
            },
        )
        .await
        .unwrap();

        tokio::fs::write(blobs.join(&live_hash), b"x")
            .await
            .unwrap();
        let orphan_hash = "b".repeat(64);
        tokio::fs::write(blobs.join(&orphan_hash), b"orphan")
            .await
            .unwrap();

        let stats = run_gc(&db, data.path(), Duration::ZERO, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(stats.blobs_deleted, 1);
        assert!(blobs.join(&live_hash).exists());
        assert!(!blobs.join(&orphan_hash).exists());
    }

    #[tokio::test]
    async fn gc_keeps_orphan_in_grace_period() {
        let data = TempDir::new().unwrap();
        let db_path = data.path().join("db.sqlite");
        let blobs = data.path().join("blobs");
        tokio::fs::create_dir_all(&blobs).await.unwrap();
        let db = Db::new(&db_path).await.unwrap();
        let orphan_hash = "c".repeat(64);
        tokio::fs::write(blobs.join(&orphan_hash), b"fresh")
            .await
            .unwrap();

        let stats = run_gc(
            &db,
            data.path(),
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
        assert_eq!(stats.blobs_deleted, 0);
        assert!(blobs.join(&orphan_hash).exists());
    }
}
