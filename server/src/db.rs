use sqlx::{sqlite::SqlitePoolOptions, SqlitePool, Row};
use std::path::Path;
use anyhow::{Context, Result};
use fs_sync_common::FileState;

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
            );"
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_workspace_files(&self, workspace_id: &str) -> Result<Vec<FileState>> {
        let rows = sqlx::query(
            "SELECT path, hash, size, mtime, deleted FROM files WHERE workspace_id = ?"
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;

        let files = rows
            .into_iter()
            .map(|r| FileState {
                path: r.get::<String, _>("path"),
                hash: r.get::<String, _>("hash"),
                size: r.get::<i64, _>("size") as u64,
                mtime: r.get::<i64, _>("mtime"),
                deleted: r.get::<bool, _>("deleted"),
            })
            .collect();

        Ok(files)
    }

    pub async fn upsert_file(&self, workspace_id: &str, file: &FileState) -> Result<()> {
        let size = file.size as i64;
        sqlx::query(
            "INSERT INTO files (workspace_id, path, hash, size, mtime, deleted, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(workspace_id, path) DO UPDATE SET
                hash = excluded.hash,
                size = excluded.size,
                mtime = excluded.mtime,
                deleted = excluded.deleted,
                updated_at = CURRENT_TIMESTAMP"
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
}
