use anyhow::{ensure, Context, Result};
use feanorfs_common::{
    canonical_manifest_hashes, file_size_from_db, file_size_to_db, is_valid_hash, FileState,
    MANIFEST_MAX_ENTRIES,
};
use sqlx::{
    pool::PoolConnection,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Connection, QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool,
};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

pub struct Db {
    pool: SqlitePool,
}

const MAX_WORKSPACE_MANIFESTS: i64 = 10_000;
const MAX_GLOBAL_MANIFESTS: i64 = 100_000;
const MAX_WORKSPACE_MANIFEST_STORAGE_BYTES: i64 = 1024 * 1024 * 1024;
const MAX_GLOBAL_MANIFEST_STORAGE_BYTES: i64 = 8 * 1024 * 1024 * 1024;
pub(crate) const MAX_STORED_MANIFEST_BYTES: usize = MANIFEST_MAX_ENTRIES * 65;
pub(crate) const GC_MANIFEST_PAGE_SIZE: i64 = 512;
pub(crate) const GC_HASH_BATCH_SIZE: usize = 500;
const GC_EXPIRED_BATCH_SIZE: usize = GC_HASH_BATCH_SIZE / 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadSwap {
    Swapped,
    Conflict(Option<String>),
    MissingManifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestWrite {
    Stored,
    Unchanged,
    Conflict,
    Capacity,
}

pub(crate) struct GcLiveSet {
    connection: PoolConnection<Sqlite>,
}

impl GcLiveSet {
    pub(crate) async fn live_hashes(&mut self, hashes: &[String]) -> Result<HashSet<String>> {
        ensure!(
            hashes.len() <= GC_HASH_BATCH_SIZE,
            "GC hash lookup batch exceeds {GC_HASH_BATCH_SIZE} entries"
        );
        if hashes.is_empty() {
            return Ok(HashSet::new());
        }

        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT hash FROM temp.gc_live_hashes WHERE hash IN (");
        {
            let mut separated = query.separated(", ");
            for hash in hashes {
                separated.push_bind(hash);
            }
            separated.push_unseparated(")");
        }
        Ok(query
            .build_query_scalar::<String>()
            .fetch_all(&mut *self.connection)
            .await?
            .into_iter()
            .collect())
    }
}

async fn insert_gc_live_hashes(connection: &mut SqliteConnection, hashes: &[String]) -> Result<()> {
    for chunk in hashes.chunks(GC_HASH_BATCH_SIZE) {
        let mut query =
            QueryBuilder::<Sqlite>::new("INSERT OR IGNORE INTO temp.gc_live_hashes (hash) ");
        query.push_values(chunk, |mut row, hash| {
            row.push_bind(hash);
        });
        query.build().execute(&mut *connection).await?;
    }
    Ok(())
}

async fn insert_expired_manifests(
    connection: &mut SqliteConnection,
    manifests: &[(String, String)],
) -> Result<()> {
    for chunk in manifests.chunks(GC_EXPIRED_BATCH_SIZE) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "INSERT OR IGNORE INTO temp.gc_expired_manifests (workspace_id, snapshot_id) ",
        );
        query.push_values(chunk, |mut row, manifest| {
            row.push_bind(&manifest.0).push_bind(&manifest.1);
        });
        query.build().execute(&mut *connection).await?;
    }
    Ok(())
}

fn manifest_storage_available(
    workspace_count: i64,
    workspace_bytes: i64,
    global_count: i64,
    global_bytes: i64,
    new_bytes: usize,
) -> bool {
    let Ok(new_bytes) = i64::try_from(new_bytes) else {
        return false;
    };
    (0..MAX_WORKSPACE_MANIFESTS).contains(&workspace_count)
        && (0..MAX_GLOBAL_MANIFESTS).contains(&global_count)
        && workspace_bytes >= 0
        && global_bytes >= 0
        && workspace_bytes
            .checked_add(new_bytes)
            .is_some_and(|total| total <= MAX_WORKSPACE_MANIFEST_STORAGE_BYTES)
        && global_bytes
            .checked_add(new_bytes)
            .is_some_and(|total| total <= MAX_GLOBAL_MANIFEST_STORAGE_BYTES)
}

impl Db {
    pub async fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        // Ensure database file exists
        if !db_path.as_ref().exists() {
            if let Some(parent) = db_path.as_ref().parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(&db_path)?;
        }

        let options = SqliteConnectOptions::new()
            .filename(db_path.as_ref())
            .busy_timeout(Duration::from_secs(5))
            // SQLx otherwise queues up to 50 rows from SQLite's worker. Some
            // rows contain bounded-but-large manifests, so keep backpressure
            // at one row for every streamed query.
            .row_buffer_size(1);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        let db = Self { pool };
        db.init_schema().await?;
        Ok(db)
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&self.pool)
            .await?;
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS files (
                workspace_id TEXT NOT NULL,
                path TEXT NOT NULL,
                hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                mode INTEGER NOT NULL DEFAULT 0,
                deleted BOOLEAN NOT NULL DEFAULT 0,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (workspace_id, path)
            );",
        )
        .execute(&self.pool)
        .await?;
        self.migrate_files_mode().await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS heads (
                workspace_id TEXT PRIMARY KEY,
                snapshot_id TEXT NOT NULL,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS snapshot_manifests (
                workspace_id TEXT NOT NULL,
                snapshot_id TEXT NOT NULL,
                manifest BLOB NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY (workspace_id, snapshot_id)
            );",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS snapshot_manifests_retention
             ON snapshot_manifests (workspace_id, created_at_ms DESC, snapshot_id DESC)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS workspace_formats (
                workspace_id TEXT PRIMARY KEY,
                format_version INTEGER NOT NULL
            );",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS migration_fences (
                workspace_id TEXT PRIMARY KEY,
                token TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_workspace_files(&self, workspace_id: &str) -> Result<Vec<FileState>> {
        let rows = sqlx::query(
            "SELECT path, hash, size, mtime, mode, deleted FROM files WHERE workspace_id = ?",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;

        let files = rows
            .into_iter()
            .map(|r| FileState {
                path: r.get::<String, _>("path"),
                hash: r.get::<String, _>("hash"),
                size: file_size_from_db(r.get::<i64, _>("size")),
                mtime: r.get::<i64, _>("mtime"),
                deleted: r.get::<bool, _>("deleted"),
                mode: u32::try_from(r.get::<i64, _>("mode")).unwrap_or(0),
            })
            .collect();

        Ok(files)
    }

    pub async fn upsert_file(&self, workspace_id: &str, file: &FileState) -> Result<()> {
        let size = file_size_to_db(file.size);
        sqlx::query(
            "INSERT INTO files (workspace_id, path, hash, size, mtime, mode, deleted, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(workspace_id, path) DO UPDATE SET
                hash = excluded.hash,
                size = excluded.size,
                mtime = excluded.mtime,
                mode = excluded.mode,
                deleted = excluded.deleted,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(workspace_id)
        .bind(&file.path)
        .bind(&file.hash)
        .bind(size)
        .bind(file.mtime)
        .bind(i64::from(file.mode))
        .bind(file.deleted)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Replaces only an already-persisted legacy row with a tombstone.
    /// This is used to retire unsafe paths accepted by older releases without
    /// reopening insertion of new non-portable metadata.
    pub async fn tombstone_existing_file(
        &self,
        workspace_id: &str,
        path: &str,
        hash: &str,
        mtime: i64,
    ) -> Result<bool> {
        let updated = sqlx::query(
            "UPDATE files SET hash = ?, size = 0, mtime = ?, mode = 0,
                    deleted = 1, updated_at = CURRENT_TIMESTAMP
             WHERE workspace_id = ? AND path = ?",
        )
        .bind(hash)
        .bind(mtime)
        .bind(workspace_id)
        .bind(path)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    pub async fn get_workspaces(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT workspace_id FROM heads
             UNION
             SELECT DISTINCT workspace_id FROM files WHERE deleted = 0
             ORDER BY workspace_id",
        )
        .fetch_all(&self.pool)
        .await?;

        let workspaces = rows
            .into_iter()
            .map(|r| r.get::<String, _>("workspace_id"))
            .collect();

        Ok(workspaces)
    }

    async fn migrate_files_mode(&self) -> Result<()> {
        let columns = sqlx::query("PRAGMA table_info(files)")
            .fetch_all(&self.pool)
            .await?;
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "mode")
        {
            sqlx::query("ALTER TABLE files ADD COLUMN mode INTEGER NOT NULL DEFAULT 0")
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn get_head(&self, workspace_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT snapshot_id FROM heads WHERE workspace_id = ?")
            .bind(workspace_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.get::<String, _>("snapshot_id")))
    }

    pub async fn swap_head(
        &self,
        workspace_id: &str,
        expected: Option<&str>,
        new: &str,
    ) -> Result<HeadSwap> {
        if !is_valid_hash(new) || expected.is_some_and(|id| !is_valid_hash(id)) {
            anyhow::bail!("invalid snapshot id for head swap");
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let manifest = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT manifest FROM snapshot_manifests
             WHERE workspace_id = ? AND snapshot_id = ?",
        )
        .bind(workspace_id)
        .bind(new)
        .fetch_optional(&mut *transaction)
        .await?;
        let manifested = manifest
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .is_some_and(|text| canonical_manifest_hashes(new, text).is_ok());
        if !manifested {
            transaction.rollback().await?;
            return Ok(HeadSwap::MissingManifest);
        }
        let current = sqlx::query("SELECT snapshot_id FROM heads WHERE workspace_id = ?")
            .bind(workspace_id)
            .fetch_optional(&mut *transaction)
            .await?
            .map(|row| row.get::<String, _>("snapshot_id"));
        if current.as_deref() != expected {
            transaction.rollback().await?;
            return Ok(HeadSwap::Conflict(current));
        }
        sqlx::query(
            "INSERT INTO heads (workspace_id, snapshot_id, updated_at)
             VALUES (?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(workspace_id) DO UPDATE SET
                snapshot_id = excluded.snapshot_id,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(workspace_id)
        .bind(new)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(HeadSwap::Swapped)
    }

    pub async fn get_referenced_hashes(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT DISTINCT hash FROM files WHERE deleted = 0")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("hash"))
            .collect())
    }

    pub async fn upsert_manifest(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        manifest: &[u8],
    ) -> Result<()> {
        ensure!(
            manifest.len() <= crate::MAX_MANIFEST_BYTES,
            "manifest exceeds encoded body limit"
        );
        let text = std::str::from_utf8(manifest).context("manifest must be UTF-8")?;
        let hashes = canonical_manifest_hashes(snapshot_id, text)?;
        match self
            .store_canonical_manifest(workspace_id, snapshot_id, &hashes)
            .await?
        {
            ManifestWrite::Stored | ManifestWrite::Unchanged => Ok(()),
            ManifestWrite::Conflict => anyhow::bail!("snapshot manifest is immutable"),
            ManifestWrite::Capacity => anyhow::bail!(
                "manifest storage capacity reached; run hub GC before publishing another snapshot"
            ),
        }
    }

    pub async fn store_canonical_manifest(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        hashes: &[String],
    ) -> Result<ManifestWrite> {
        ensure!(
            is_valid_hash(snapshot_id),
            "invalid snapshot id for manifest"
        );
        ensure!(!hashes.is_empty(), "manifest must not be empty");
        ensure!(
            hashes.len() <= MANIFEST_MAX_ENTRIES,
            "manifest exceeds {MANIFEST_MAX_ENTRIES} object entries"
        );
        ensure!(
            hashes.iter().all(|hash| is_valid_hash(hash))
                && hashes.windows(2).all(|pair| pair[0] < pair[1]),
            "manifest object ids must be canonical, sorted, and unique"
        );
        ensure!(
            hashes
                .binary_search_by(|hash| hash.as_str().cmp(snapshot_id))
                .is_ok(),
            "manifest does not contain its snapshot root"
        );
        let mut canonical = hashes.join("\n").into_bytes();
        canonical.push(b'\n');
        ensure!(
            canonical.len() <= crate::MAX_MANIFEST_BYTES,
            "manifest exceeds encoded body limit"
        );
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT manifest FROM snapshot_manifests
             WHERE workspace_id = ? AND snapshot_id = ?",
        )
        .bind(workspace_id)
        .bind(snapshot_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(existing) = existing {
            let existing =
                std::str::from_utf8(&existing).context("stored manifest is not UTF-8")?;
            let unchanged = canonical_manifest_hashes(snapshot_id, existing)? == hashes;
            transaction.rollback().await?;
            return Ok(if unchanged {
                ManifestWrite::Unchanged
            } else {
                ManifestWrite::Conflict
            });
        }

        let workspace_usage = sqlx::query(
            "SELECT COUNT(*) AS count, COALESCE(SUM(length(manifest)), 0) AS bytes
             FROM snapshot_manifests WHERE workspace_id = ?",
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await?;
        let global_usage = sqlx::query(
            "SELECT COUNT(*) AS count, COALESCE(SUM(length(manifest)), 0) AS bytes
             FROM snapshot_manifests",
        )
        .fetch_one(&mut *transaction)
        .await?;
        if !manifest_storage_available(
            workspace_usage.get("count"),
            workspace_usage.get("bytes"),
            global_usage.get("count"),
            global_usage.get("bytes"),
            canonical.len(),
        ) {
            transaction.rollback().await?;
            return Ok(ManifestWrite::Capacity);
        }
        sqlx::query(
            "INSERT INTO snapshot_manifests (workspace_id, snapshot_id, manifest, created_at_ms)
             VALUES (?, ?, ?, ?)",
        )
        .bind(workspace_id)
        .bind(snapshot_id)
        .bind(&canonical)
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ManifestWrite::Stored)
    }

    pub async fn manifest_exists(&self, workspace_id: &str, snapshot_id: &str) -> Result<bool> {
        let manifest = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT manifest FROM snapshot_manifests
             WHERE workspace_id = ? AND snapshot_id = ?",
        )
        .bind(workspace_id)
        .bind(snapshot_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(manifest
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .is_some_and(|text| canonical_manifest_hashes(snapshot_id, text).is_ok()))
    }

    pub async fn workspace_format(&self, workspace_id: &str) -> Result<u32> {
        let row =
            sqlx::query("SELECT format_version FROM workspace_formats WHERE workspace_id = ?")
                .bind(workspace_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .and_then(|row| u32::try_from(row.get::<i64, _>("format_version")).ok())
            .unwrap_or(2))
    }

    pub async fn begin_migration(&self, workspace_id: &str, token: &str) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        let format = sqlx::query_scalar::<_, i64>(
            "SELECT format_version FROM workspace_formats WHERE workspace_id = ?",
        )
        .bind(workspace_id)
        .fetch_optional(&mut *transaction)
        .await?
        .unwrap_or(2);
        if format >= 3 {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO migration_fences (workspace_id, token) VALUES (?, ?)
             ON CONFLICT(workspace_id) DO NOTHING",
        )
        .bind(workspace_id)
        .bind(token)
        .execute(&mut *transaction)
        .await?;
        let current = sqlx::query_scalar::<_, String>(
            "SELECT token FROM migration_fences WHERE workspace_id = ?",
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await?;
        anyhow::ensure!(current == token, "workspace migration is already locked");
        transaction.commit().await?;
        Ok(())
    }

    pub async fn migration_token(&self, workspace_id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT token FROM migration_fences WHERE workspace_id = ?")
                .bind(workspace_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn set_workspace_format(&self, workspace_id: &str, version: u32) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        if version >= 3 {
            let head_manifest = sqlx::query(
                "SELECT h.snapshot_id, m.manifest
                 FROM heads h
                 JOIN snapshot_manifests m
                   ON m.workspace_id = h.workspace_id
                  AND m.snapshot_id = h.snapshot_id
                 WHERE h.workspace_id = ?",
            )
            .bind(workspace_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let valid = head_manifest.is_some_and(|row| {
                let snapshot_id = row.get::<String, _>("snapshot_id");
                let manifest = row.get::<Vec<u8>, _>("manifest");
                std::str::from_utf8(&manifest)
                    .ok()
                    .is_some_and(|text| canonical_manifest_hashes(&snapshot_id, text).is_ok())
            });
            anyhow::ensure!(valid, "format v3 requires a manifested snapshot head");
        }
        sqlx::query(
            "INSERT INTO workspace_formats (workspace_id, format_version) VALUES (?, ?)
             ON CONFLICT(workspace_id) DO UPDATE SET format_version = excluded.format_version",
        )
        .bind(workspace_id)
        .bind(i64::from(version))
        .execute(&mut *transaction)
        .await?;
        if version >= 3 {
            sqlx::query("DELETE FROM files WHERE workspace_id = ?")
                .bind(workspace_id)
                .execute(&mut *transaction)
                .await?;
            sqlx::query("DELETE FROM migration_fences WHERE workspace_id = ?")
                .bind(workspace_id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn prepare_gc_live_set(
        &self,
        cutoff_ms: i64,
        keep_last: usize,
    ) -> Result<GcLiveSet> {
        let mut connection = self.pool.acquire().await?;
        // TEMP tables are connection-local. Closing instead of pooling this
        // connection guarantees that a completed or failed GC leaves no stale
        // mark state for a later run.
        connection.close_on_drop();
        sqlx::query("PRAGMA temp_store = FILE")
            .execute(&mut *connection)
            .await?;
        sqlx::query("PRAGMA temp.cache_size = -8192")
            .execute(&mut *connection)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS temp.gc_live_hashes")
            .execute(&mut *connection)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS temp.gc_expired_manifests")
            .execute(&mut *connection)
            .await?;
        sqlx::query(
            "CREATE TEMP TABLE gc_live_hashes (
                hash TEXT PRIMARY KEY
             ) WITHOUT ROWID",
        )
        .execute(&mut *connection)
        .await?;
        sqlx::query(
            "CREATE TEMP TABLE gc_expired_manifests (
                workspace_id TEXT NOT NULL,
                snapshot_id TEXT NOT NULL,
                PRIMARY KEY (workspace_id, snapshot_id)
             ) WITHOUT ROWID",
        )
        .execute(&mut *connection)
        .await?;

        // Keep one stable database view while building the disk-backed mark
        // set and pruning manifests. The publication lock held by run_gc is
        // still required to cover the later filesystem sweep.
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        let missing_head = sqlx::query_as::<_, (String, String)>(
            "SELECT h.workspace_id, h.snapshot_id
             FROM heads AS h
             LEFT JOIN snapshot_manifests AS m
               ON m.workspace_id = h.workspace_id
              AND m.snapshot_id = h.snapshot_id
             WHERE m.snapshot_id IS NULL
             ORDER BY h.workspace_id, h.snapshot_id
             LIMIT 1",
        )
        .fetch_optional(&mut *transaction)
        .await?;
        ensure!(
            missing_head.is_none(),
            "one or more workspace heads have no reachability manifest"
        );

        // Legacy flat-file references can also be arbitrarily numerous. Let
        // SQLite deduplicate them directly instead of first collecting a Rust
        // Vec or HashSet.
        sqlx::query(
            "INSERT OR IGNORE INTO temp.gc_live_hashes (hash)
             SELECT hash FROM files WHERE deleted = 0",
        )
        .execute(&mut *transaction)
        .await?;

        let mut cursor: Option<(String, i64, String)> = None;
        let mut current_workspace: Option<String> = None;
        let mut workspace_rank = 0usize;
        loop {
            // Fetch only bounded metadata pages. A kept manifest BLOB is read
            // separately by primary key, so SQLite never needs to sort or
            // SQLx-buffer the aggregate encoded manifest storage.
            let page = if let Some((workspace_id, created_at_ms, snapshot_id)) = &cursor {
                sqlx::query_as::<_, (String, String, i64, i64, String, i64)>(
                    "SELECT m.workspace_id, m.snapshot_id, m.created_at_ms,
                            length(m.manifest), typeof(m.manifest),
                            EXISTS(
                                SELECT 1 FROM heads AS h
                                WHERE h.workspace_id = m.workspace_id
                                  AND h.snapshot_id = m.snapshot_id
                            )
                     FROM snapshot_manifests AS m
                          INDEXED BY snapshot_manifests_retention
                     WHERE m.workspace_id > ?
                        OR (m.workspace_id = ? AND m.created_at_ms < ?)
                        OR (m.workspace_id = ? AND m.created_at_ms = ?
                            AND m.snapshot_id < ?)
                     ORDER BY m.workspace_id ASC, m.created_at_ms DESC,
                              m.snapshot_id DESC
                     LIMIT ?",
                )
                .bind(workspace_id)
                .bind(workspace_id)
                .bind(created_at_ms)
                .bind(workspace_id)
                .bind(created_at_ms)
                .bind(snapshot_id)
                .bind(GC_MANIFEST_PAGE_SIZE)
                .fetch_all(&mut *transaction)
                .await?
            } else {
                sqlx::query_as::<_, (String, String, i64, i64, String, i64)>(
                    "SELECT m.workspace_id, m.snapshot_id, m.created_at_ms,
                            length(m.manifest), typeof(m.manifest),
                            EXISTS(
                                SELECT 1 FROM heads AS h
                                WHERE h.workspace_id = m.workspace_id
                                  AND h.snapshot_id = m.snapshot_id
                            )
                     FROM snapshot_manifests AS m
                          INDEXED BY snapshot_manifests_retention
                     ORDER BY m.workspace_id ASC, m.created_at_ms DESC,
                              m.snapshot_id DESC
                     LIMIT ?",
                )
                .bind(GC_MANIFEST_PAGE_SIZE)
                .fetch_all(&mut *transaction)
                .await?
            };
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            let mut expired = Vec::new();
            for (
                workspace_id,
                snapshot_id,
                created_at_ms,
                manifest_length,
                manifest_type,
                is_head,
            ) in page
            {
                cursor = Some((workspace_id.clone(), created_at_ms, snapshot_id.clone()));
                if current_workspace.as_deref() != Some(workspace_id.as_str()) {
                    current_workspace = Some(workspace_id.clone());
                    workspace_rank = 0;
                }
                let keep = is_head != 0 || workspace_rank < keep_last || created_at_ms >= cutoff_ms;
                workspace_rank = workspace_rank.saturating_add(1);
                if !keep {
                    expired.push((workspace_id, snapshot_id));
                    continue;
                }

                ensure!(
                    manifest_type == "blob",
                    "stored retained manifest {workspace_id}/{snapshot_id} is not a BLOB"
                );
                let manifest_length = usize::try_from(manifest_length).with_context(|| {
                    format!(
                        "stored retained manifest {workspace_id}/{snapshot_id} has invalid length"
                    )
                })?;
                ensure!(
                    manifest_length <= MAX_STORED_MANIFEST_BYTES,
                    "stored retained manifest {workspace_id}/{snapshot_id} exceeds canonical size"
                );
                let manifest = sqlx::query_scalar::<_, Vec<u8>>(
                    "SELECT manifest FROM snapshot_manifests
                     WHERE workspace_id = ? AND snapshot_id = ?",
                )
                .bind(&workspace_id)
                .bind(&snapshot_id)
                .fetch_one(&mut *transaction)
                .await
                .with_context(|| format!("read retained manifest {workspace_id}/{snapshot_id}"))?;
                ensure!(
                    manifest.len() == manifest_length,
                    "stored retained manifest {workspace_id}/{snapshot_id} changed during GC"
                );
                let text = std::str::from_utf8(&manifest).with_context(|| {
                    format!("stored retained manifest {workspace_id}/{snapshot_id} is not UTF-8")
                })?;
                let hashes = canonical_manifest_hashes(&snapshot_id, text).with_context(|| {
                    format!("stored retained manifest {workspace_id}/{snapshot_id} is invalid")
                })?;
                insert_gc_live_hashes(&mut transaction, &hashes).await?;
            }
            insert_expired_manifests(&mut transaction, &expired).await?;
            if page_len < GC_MANIFEST_PAGE_SIZE as usize {
                break;
            }
        }

        let expired_head = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                SELECT 1
                FROM temp.gc_expired_manifests AS expired
                JOIN heads AS h
                  ON h.workspace_id = expired.workspace_id
                 AND h.snapshot_id = expired.snapshot_id
             )",
        )
        .fetch_one(&mut *transaction)
        .await?;
        ensure!(expired_head == 0, "workspace head changed during GC");
        sqlx::query(
            "DELETE FROM snapshot_manifests
             WHERE EXISTS (
                 SELECT 1 FROM temp.gc_expired_manifests AS expired
                 WHERE expired.workspace_id = snapshot_manifests.workspace_id
                   AND expired.snapshot_id = snapshot_manifests.snapshot_id
             )",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(GcLiveSet { connection })
    }

    pub async fn purge_old_tombstones(&self, older_than_ms: i64) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM files WHERE deleted = 1 AND updated_at < datetime(? / 1000, 'unixepoch')",
        )
        .bind(older_than_ms)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod manifest_capacity_tests {
    use super::*;

    #[test]
    fn manifest_storage_capacity_is_bounded_per_workspace_and_globally() {
        assert!(
            MAX_WORKSPACE_MANIFEST_STORAGE_BYTES
                >= i64::try_from(MAX_STORED_MANIFEST_BYTES).unwrap() * 50,
            "the workspace quota must hold the default retained maximum-size manifests"
        );
        assert!(manifest_storage_available(
            0,
            0,
            0,
            0,
            crate::MAX_MANIFEST_BYTES,
        ));
        assert!(manifest_storage_available(
            0,
            MAX_WORKSPACE_MANIFEST_STORAGE_BYTES - 1,
            0,
            0,
            1,
        ));
        assert!(!manifest_storage_available(
            0,
            MAX_WORKSPACE_MANIFEST_STORAGE_BYTES,
            0,
            0,
            1,
        ));
        assert!(!manifest_storage_available(
            0,
            0,
            0,
            MAX_GLOBAL_MANIFEST_STORAGE_BYTES,
            1,
        ));
        assert!(!manifest_storage_available(
            MAX_WORKSPACE_MANIFESTS,
            0,
            0,
            0,
            1,
        ));
        assert!(!manifest_storage_available(
            0,
            0,
            MAX_GLOBAL_MANIFESTS,
            0,
            1,
        ));
        assert!(!manifest_storage_available(-1, 0, 0, 0, 1));
        assert!(!manifest_storage_available(
            i64::MAX,
            i64::MAX,
            i64::MAX,
            i64::MAX,
            usize::MAX,
        ));
    }
}
