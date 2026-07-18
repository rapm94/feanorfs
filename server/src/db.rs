use anyhow::{Context, Result};
use feanorfs_common::{file_size_from_db, file_size_to_db, is_valid_hash, FileState};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

pub struct Db {
    pool: SqlitePool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadSwap {
    Swapped,
    Conflict(Option<String>),
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
            .busy_timeout(Duration::from_secs(5));
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
        if !is_valid_hash(snapshot_id) {
            anyhow::bail!("invalid snapshot id for manifest");
        }
        let text = std::str::from_utf8(manifest).context("manifest must be UTF-8")?;
        if text.lines().any(|hash| !is_valid_hash(hash)) {
            anyhow::bail!("manifest contains invalid object id");
        }
        sqlx::query(
            "INSERT INTO snapshot_manifests (workspace_id, snapshot_id, manifest, created_at_ms)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(workspace_id, snapshot_id) DO UPDATE SET
                manifest = excluded.manifest,
                created_at_ms = excluded.created_at_ms",
        )
        .bind(workspace_id)
        .bind(snapshot_id)
        .bind(manifest)
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn manifest_exists(&self, workspace_id: &str, snapshot_id: &str) -> Result<bool> {
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                SELECT 1 FROM snapshot_manifests
                WHERE workspace_id = ? AND snapshot_id = ?
             )",
        )
        .bind(workspace_id)
        .bind(snapshot_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(exists != 0)
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
            let has_manifested_head = sqlx::query_scalar::<_, i64>(
                "SELECT EXISTS(
                    SELECT 1 FROM heads h
                    JOIN snapshot_manifests m
                      ON m.workspace_id = h.workspace_id
                     AND m.snapshot_id = h.snapshot_id
                    WHERE h.workspace_id = ?
                 )",
            )
            .bind(workspace_id)
            .fetch_one(&mut *transaction)
            .await?
                != 0;
            anyhow::ensure!(
                has_manifested_head,
                "format v3 requires a manifested snapshot head"
            );
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

    pub async fn retained_manifest_hashes(
        &self,
        cutoff_ms: i64,
        keep_last: usize,
    ) -> Result<HashSet<String>> {
        let head_rows = sqlx::query("SELECT snapshot_id FROM heads")
            .fetch_all(&self.pool)
            .await?;
        let heads: HashSet<String> = head_rows
            .into_iter()
            .map(|row| row.get::<String, _>("snapshot_id"))
            .collect();
        let rows = sqlx::query(
            "SELECT workspace_id, snapshot_id, manifest, created_at_ms
             FROM snapshot_manifests
             ORDER BY workspace_id, created_at_ms DESC, snapshot_id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut ranks: HashMap<String, usize> = HashMap::new();
        let mut retained = HashSet::new();
        let mut manifested_heads = HashSet::new();
        let mut expired = Vec::new();
        for row in rows {
            let workspace_id = row.get::<String, _>("workspace_id");
            let snapshot_id = row.get::<String, _>("snapshot_id");
            let created_at_ms = row.get::<i64, _>("created_at_ms");
            let rank = ranks.entry(workspace_id.clone()).or_default();
            let is_head = heads.contains(&snapshot_id);
            let keep = is_head || *rank < keep_last || created_at_ms >= cutoff_ms;
            *rank += 1;
            if keep {
                if is_head {
                    manifested_heads.insert(snapshot_id.clone());
                }
                retained.insert(snapshot_id);
                let manifest = row.get::<Vec<u8>, _>("manifest");
                let text =
                    std::str::from_utf8(&manifest).context("stored manifest is not UTF-8")?;
                retained.extend(text.lines().map(ToString::to_string));
            } else {
                expired.push((workspace_id, snapshot_id));
            }
        }
        if manifested_heads != heads {
            anyhow::bail!("one or more workspace heads have no reachability manifest");
        }
        let mut transaction = self.pool.begin().await?;
        for (workspace_id, snapshot_id) in expired {
            sqlx::query(
                "DELETE FROM snapshot_manifests WHERE workspace_id = ? AND snapshot_id = ?",
            )
            .bind(workspace_id)
            .bind(snapshot_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(retained)
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
