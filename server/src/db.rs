use anyhow::{Context, Result};
use feanorfs_common::FileState;
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use std::path::Path;

fn file_size_from_db(size: i64) -> u64 {
    u64::try_from(size).unwrap_or_else(|_| {
        tracing::warn!(
            "stored file size {} exceeds u64::MAX, saturating to u64::MAX",
            size
        );
        u64::MAX
    })
}

fn file_size_to_db(size: u64) -> i64 {
    i64::try_from(size).unwrap_or_else(|_| {
        tracing::warn!(
            "file size {} exceeds i64::MAX, saturating to i64::MAX",
            size
        );
        i64::MAX
    })
}

pub struct Db {
    pool: SqlitePool,
}

impl Db {
    pub async fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let db_url = format!(
            "sqlite:{}",
            db_path.as_ref().to_str().context("Invalid database path")?
        );

        // Ensure database file exists
        if !db_path.as_ref().exists() {
            if let Some(parent) = db_path.as_ref().parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(&db_path)?;
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&db_url)
            .await?;

        let db = Self { pool };
        db.init_schema().await?;
        Ok(db)
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS files (
                workspace_id TEXT NOT NULL,
                path TEXT NOT NULL,
                hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                deleted BOOLEAN NOT NULL DEFAULT 0,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (workspace_id, path)
            );",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_workspace_files(&self, workspace_id: &str) -> Result<Vec<FileState>> {
        let rows = sqlx::query(
            "SELECT path, hash, size, mtime, deleted FROM files WHERE workspace_id = ?",
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
            })
            .collect();

        Ok(files)
    }

    pub async fn upsert_file(&self, workspace_id: &str, file: &FileState) -> Result<()> {
        let size = file_size_to_db(file.size);
        sqlx::query(
            "INSERT INTO files (workspace_id, path, hash, size, mtime, deleted, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(workspace_id, path) DO UPDATE SET
                hash = excluded.hash,
                size = excluded.size,
                mtime = excluded.mtime,
                deleted = excluded.deleted,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(workspace_id)
        .bind(&file.path)
        .bind(&file.hash)
        .bind(size)
        .bind(file.mtime)
        .bind(file.deleted)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_workspaces(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT workspace_id FROM files WHERE deleted = 0 ORDER BY workspace_id",
        )
        .fetch_all(&self.pool)
        .await?;

        let workspaces = rows
            .into_iter()
            .map(|r| r.get::<String, _>("workspace_id"))
            .collect();

        Ok(workspaces)
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
