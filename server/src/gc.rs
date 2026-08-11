use crate::db::{Db, GcLiveSet, GC_HASH_BATCH_SIZE};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::sync::RwLock;

#[derive(Debug, Default, Serialize)]
pub struct GcStats {
    pub blobs_deleted: u64,
    pub tombstones_purged: u64,
    pub bytes_freed: u64,
}

fn retention_cutoff_ms(now_ms: i64, retention: Duration) -> i64 {
    let Ok(retention_ms) = i64::try_from(retention.as_millis()) else {
        return i64::MIN;
    };
    now_ms.saturating_sub(retention_ms)
}

async fn sweep_blob_batch(
    live_set: &mut GcLiveSet,
    blobs_dir: &Path,
    hashes: &mut Vec<String>,
    grace: Duration,
    stats: &mut GcStats,
) -> Result<()> {
    let live_hashes = live_set.live_hashes(hashes).await?;
    for hash in hashes.drain(..) {
        if live_hashes.contains(&hash) {
            continue;
        }
        let path = blobs_dir.join(&hash);
        let meta = fs::metadata(&path).await?;
        let age_ok = meta
            .modified()
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|elapsed| elapsed >= grace);
        if !age_ok {
            continue;
        }
        let size = meta.len();
        if let Err(error) = fs::remove_file(&path).await {
            tracing::warn!("failed to remove orphan blob {}: {error}", path.display());
            continue;
        }
        stats.blobs_deleted += 1;
        stats.bytes_freed += size;
    }
    Ok(())
}

pub async fn run_gc(
    db: &Db,
    storage_dir: &Path,
    grace: Duration,
    tombstone_retention: Duration,
    snapshot_retention: Duration,
    snapshot_keep_last: usize,
    publication_lock: &RwLock<()>,
) -> Result<GcStats> {
    // Publication must remain excluded from the database mark through the
    // final filesystem deletion. Otherwise a newly published head could refer
    // to an old blob after it was marked dead but before it was swept.
    let _publication_guard = publication_lock.write().await;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let tombstone_cutoff = retention_cutoff_ms(now, tombstone_retention);
    let snapshot_cutoff = retention_cutoff_ms(now, snapshot_retention);
    let mut live_set = db
        .prepare_gc_live_set(snapshot_cutoff, snapshot_keep_last)
        .await?;
    let tombstones_purged = db.purge_old_tombstones(tombstone_cutoff).await?;

    let blobs_dir = storage_dir.join("blobs");
    let mut stats = GcStats {
        tombstones_purged,
        ..Default::default()
    };

    if !blobs_dir.is_dir() {
        return Ok(stats);
    }

    let mut hashes = Vec::with_capacity(GC_HASH_BATCH_SIZE);
    let mut entries = fs::read_dir(&blobs_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(hash) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        hashes.push(hash.to_owned());
        if hashes.len() == GC_HASH_BATCH_SIZE {
            sweep_blob_batch(&mut live_set, &blobs_dir, &mut hashes, grace, &mut stats).await?;
        }
    }
    if !hashes.is_empty() {
        sweep_blob_batch(&mut live_set, &blobs_dir, &mut hashes, grace, &mut stats).await?;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, GC_MANIFEST_PAGE_SIZE, MAX_STORED_MANIFEST_BYTES};
    use feanorfs_common::FileState;
    use std::collections::HashSet;
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
                mode: 0,
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

        let stats = run_gc(
            &db,
            data.path(),
            Duration::ZERO,
            Duration::from_secs(3600),
            Duration::from_secs(3600),
            50,
            &RwLock::new(()),
        )
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
            Duration::from_secs(3600),
            50,
            &RwLock::new(()),
        )
        .await
        .unwrap();
        assert_eq!(stats.blobs_deleted, 0);
        assert!(blobs.join(&orphan_hash).exists());
    }

    #[tokio::test]
    async fn gc_does_not_use_another_workspaces_same_id_manifest_for_a_head() {
        let data = TempDir::new().unwrap();
        let db_path = data.path().join("db.sqlite");
        let db = Db::new(&db_path).await.unwrap();
        let snapshot = "a".repeat(64);
        let first_child = "b".repeat(64);
        let second_child = "c".repeat(64);
        db.upsert_manifest(
            "first",
            &snapshot,
            format!("{snapshot}\n{first_child}\n").as_bytes(),
        )
        .await
        .unwrap();
        db.swap_head("first", None, &snapshot).await.unwrap();
        db.upsert_manifest(
            "second",
            &snapshot,
            format!("{snapshot}\n{second_child}\n").as_bytes(),
        )
        .await
        .unwrap();

        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path),
        )
        .await
        .unwrap();
        sqlx::query(
            "DELETE FROM snapshot_manifests WHERE workspace_id = 'first' AND snapshot_id = ?",
        )
        .bind(&snapshot)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        assert!(db.prepare_gc_live_set(i64::MAX, 0).await.is_err());
    }

    #[tokio::test]
    async fn gc_keeps_complete_retained_manifest_closure() {
        let data = TempDir::new().unwrap();
        let db = Db::new(data.path().join("db.sqlite")).await.unwrap();
        let blobs = data.path().join("blobs");
        tokio::fs::create_dir_all(&blobs).await.unwrap();
        let snapshot = "1".repeat(64);
        let tree = "2".repeat(64);
        let file = "3".repeat(64);
        let orphan = "4".repeat(64);
        for hash in [&snapshot, &tree, &file, &orphan] {
            tokio::fs::write(blobs.join(hash), hash.as_bytes())
                .await
                .unwrap();
        }
        db.upsert_manifest(
            "ws",
            &snapshot,
            format!("{snapshot}\n{tree}\n{file}\n").as_bytes(),
        )
        .await
        .unwrap();
        db.swap_head("ws", None, &snapshot).await.unwrap();

        let stats = run_gc(
            &db,
            data.path(),
            Duration::ZERO,
            Duration::from_secs(3600),
            Duration::ZERO,
            0,
            &RwLock::new(()),
        )
        .await
        .unwrap();

        assert_eq!(stats.blobs_deleted, 1);
        assert!(blobs.join(snapshot).exists());
        assert!(blobs.join(tree).exists());
        assert!(blobs.join(file).exists());
        assert!(!blobs.join(orphan).exists());
    }

    #[tokio::test]
    async fn gc_keyset_pages_retain_an_old_head_and_prune_every_expired_row() {
        let data = TempDir::new().unwrap();
        let db_path = data.path().join("db.sqlite");
        let db = Db::new(&db_path).await.unwrap();
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .busy_timeout(Duration::from_secs(5)),
        )
        .await
        .unwrap();
        let mut transaction = pool.begin().await.unwrap();
        for index in 0..=GC_MANIFEST_PAGE_SIZE {
            let snapshot = format!("{index:064x}");
            sqlx::query(
                "INSERT INTO snapshot_manifests
                    (workspace_id, snapshot_id, manifest, created_at_ms)
                 VALUES ('ws', ?, ?, ?)",
            )
            .bind(&snapshot)
            .bind(format!("{snapshot}\n").into_bytes())
            // Force the snapshot-id tie-breaker to carry the keyset cursor
            // across the page boundary.
            .bind(0_i64)
            .execute(&mut *transaction)
            .await
            .unwrap();
        }
        transaction.commit().await.unwrap();

        let head = format!("{:064x}", 0);
        let newest = format!("{:064x}", GC_MANIFEST_PAGE_SIZE);
        db.swap_head("ws", None, &head).await.unwrap();
        let mut live_set = db.prepare_gc_live_set(i64::MAX, 0).await.unwrap();
        let candidates = vec![head.clone(), newest];
        let live = live_set.live_hashes(&candidates).await.unwrap();
        assert_eq!(live, HashSet::from([head]));
        drop(live_set);

        let remaining = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM snapshot_manifests WHERE workspace_id = 'ws'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(remaining, 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn retained_oversized_manifest_fails_before_any_gc_mutation() {
        let data = TempDir::new().unwrap();
        let db_path = data.path().join("db.sqlite");
        let blobs = data.path().join("blobs");
        tokio::fs::create_dir_all(&blobs).await.unwrap();
        let db = Db::new(&db_path).await.unwrap();
        let expired = "e".repeat(64);
        let head = "f".repeat(64);
        db.upsert_manifest("ws", &expired, format!("{expired}\n").as_bytes())
            .await
            .unwrap();
        db.upsert_manifest("ws", &head, format!("{head}\n").as_bytes())
            .await
            .unwrap();
        db.swap_head("ws", None, &head).await.unwrap();
        db.upsert_file(
            "legacy",
            &FileState {
                path: "deleted.txt".into(),
                hash: "d".repeat(64),
                size: 0,
                mtime: 0,
                deleted: true,
                mode: 0,
            },
        )
        .await
        .unwrap();
        let orphan = "a".repeat(64);
        tokio::fs::write(blobs.join(&orphan), b"orphan")
            .await
            .unwrap();

        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .busy_timeout(Duration::from_secs(5)),
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE snapshot_manifests
             SET manifest = zeroblob(?), created_at_ms = 0
             WHERE workspace_id = 'ws' AND snapshot_id = ?",
        )
        .bind(i64::try_from(MAX_STORED_MANIFEST_BYTES + 1).unwrap())
        .bind(&head)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE snapshot_manifests SET created_at_ms = 0
             WHERE workspace_id = 'ws' AND snapshot_id = ?",
        )
        .bind(&expired)
        .execute(&pool)
        .await
        .unwrap();

        let error = run_gc(
            &db,
            data.path(),
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            0,
            &RwLock::new(()),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("exceeds canonical size"));
        assert!(blobs.join(orphan).exists());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM snapshot_manifests")
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM files WHERE deleted = 1")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn expired_oversized_manifest_is_pruned_without_loading_it() {
        let data = TempDir::new().unwrap();
        let db_path = data.path().join("db.sqlite");
        let db = Db::new(&db_path).await.unwrap();
        let snapshot = "b".repeat(64);
        db.upsert_manifest("ws", &snapshot, format!("{snapshot}\n").as_bytes())
            .await
            .unwrap();
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .busy_timeout(Duration::from_secs(5)),
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE snapshot_manifests
             SET manifest = zeroblob(?), created_at_ms = 0
             WHERE workspace_id = 'ws' AND snapshot_id = ?",
        )
        .bind(i64::try_from(MAX_STORED_MANIFEST_BYTES + 1).unwrap())
        .bind(&snapshot)
        .execute(&pool)
        .await
        .unwrap();

        drop(db.prepare_gc_live_set(i64::MAX, 0).await.unwrap());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM snapshot_manifests")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn gc_sweeps_in_bounded_hash_batches() {
        let data = TempDir::new().unwrap();
        let db = Db::new(data.path().join("db.sqlite")).await.unwrap();
        let blobs = data.path().join("blobs");
        tokio::fs::create_dir_all(&blobs).await.unwrap();
        let hashes = (0..=GC_HASH_BATCH_SIZE)
            .map(|index| format!("{index:064x}"))
            .collect::<Vec<_>>();
        let root = hashes[0].clone();
        db.store_canonical_manifest("ws", &root, &hashes)
            .await
            .unwrap();
        db.swap_head("ws", None, &root).await.unwrap();
        for hash in &hashes {
            tokio::fs::write(blobs.join(hash), b"live").await.unwrap();
        }
        let orphan = "f".repeat(64);
        tokio::fs::write(blobs.join(&orphan), b"orphan")
            .await
            .unwrap();

        let stats = run_gc(
            &db,
            data.path(),
            Duration::ZERO,
            Duration::from_secs(3600),
            Duration::ZERO,
            0,
            &RwLock::new(()),
        )
        .await
        .unwrap();
        assert_eq!(stats.blobs_deleted, 1);
        assert!(!blobs.join(orphan).exists());
        assert!(hashes.iter().all(|hash| blobs.join(hash).exists()));
    }

    #[test]
    fn extreme_retention_cutoff_saturates() {
        assert_eq!(retention_cutoff_ms(123, Duration::MAX), i64::MIN);
        assert_eq!(
            retention_cutoff_ms(i64::MIN, Duration::from_millis(1)),
            i64::MIN
        );
    }
}
