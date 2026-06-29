use anyhow::{Context, Result};
use feanorfs_common::{normalize_path, FileState};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    pub workspace_id: String,
    pub encryption_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_password: Option<String>,
}

/// Global client config stored at ~/.feanorfs/global.json.
/// Cached by `feanorfs connect` so that `init` and other commands
/// don't need an explicit server URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_password: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: String,
    pub plaintext_hash: String,
    pub encrypted_hash: String,
    pub size: u64,
    pub mtime: i64,
    pub server_mtime: i64,
    pub hydrated: bool,
}

pub struct ClientDb {
    pool: SqlitePool,
}

impl ClientDb {
    pub async fn new<P: AsRef<Path>>(fs_dir: P) -> Result<Self> {
        let db_path = fs_dir.as_ref().join("local_cache.db");
        let db_url = format!(
            "sqlite:{}",
            db_path.to_str().context("Invalid database path")?
        );

        fs::create_dir_all(&fs_dir)?;
        if !db_path.exists() {
            fs::File::create(&db_path)?;
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await?;

        let db = Self { pool };
        db.init_schema().await?;
        Ok(db)
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS local_files (
                path TEXT PRIMARY KEY,
                plaintext_hash TEXT NOT NULL,
                encrypted_hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                server_mtime INTEGER NOT NULL DEFAULT 0,
                hydrated INTEGER NOT NULL DEFAULT 1
            );",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agent_snapshots (
                agent_name TEXT NOT NULL,
                path TEXT NOT NULL,
                base_hash TEXT NOT NULL,
                base_size INTEGER NOT NULL,
                base_mtime INTEGER NOT NULL,
                PRIMARY KEY (agent_name, path)
            );",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS file_access_log (
                path TEXT NOT NULL,
                sibling_path TEXT NOT NULL,
                weight REAL NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (path, sibling_path)
            );",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS last_session (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_cache_entries(&self) -> Result<HashMap<String, CacheEntry>> {
        let rows = sqlx::query(
            "SELECT path, plaintext_hash, encrypted_hash, size, mtime, server_mtime, hydrated FROM local_files"
        )
        .fetch_all(&self.pool)
        .await?;

        let mut files = HashMap::new();
        for r in rows {
            let path = r.get::<String, _>("path");
            let entry = CacheEntry {
                path: path.clone(),
                plaintext_hash: r.get::<String, _>("plaintext_hash"),
                encrypted_hash: r.get::<String, _>("encrypted_hash"),
                size: u64::try_from(r.get::<i64, _>("size")).unwrap_or(0),
                mtime: r.get::<i64, _>("mtime"),
                server_mtime: r.get::<i64, _>("server_mtime"),
                hydrated: r.get::<i32, _>("hydrated") != 0,
            };
            files.insert(path, entry);
        }
        Ok(files)
    }

    pub async fn upsert_cache_entry(&self, entry: &CacheEntry) -> Result<()> {
        let size = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let hydrated = if entry.hydrated { 1 } else { 0 };
        sqlx::query(
            "INSERT OR REPLACE INTO local_files (path, plaintext_hash, encrypted_hash, size, mtime, server_mtime, hydrated)
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&entry.path)
        .bind(&entry.plaintext_hash)
        .bind(&entry.encrypted_hash)
        .bind(size)
        .bind(entry.mtime)
        .bind(entry.server_mtime)
        .bind(hydrated)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_cache_entry(&self, path: &str) -> Result<()> {
        sqlx::query("DELETE FROM local_files WHERE path = ?")
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_agent_snapshot(
        &self,
        entries: &[feanorfs_common::AgentSnapshotEntry],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for entry in entries {
            sqlx::query(
                "INSERT OR REPLACE INTO agent_snapshots
                    (agent_name, path, base_hash, base_size, base_mtime)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(entry.agent_name.clone())
            .bind(&entry.path)
            .bind(&entry.base_hash)
            .bind(i64::try_from(entry.base_size).unwrap_or(i64::MAX))
            .bind(entry.base_mtime)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_agent_snapshot(
        &self,
        agent_name: &str,
    ) -> Result<Vec<feanorfs_common::AgentSnapshotEntry>> {
        let rows = sqlx::query(
            "SELECT agent_name, path, base_hash, base_size, base_mtime
             FROM agent_snapshots WHERE agent_name = ?",
        )
        .bind(agent_name)
        .fetch_all(&self.pool)
        .await?;

        let entries = rows
            .into_iter()
            .map(|r| feanorfs_common::AgentSnapshotEntry {
                agent_name: r.get::<String, _>("agent_name"),
                path: r.get::<String, _>("path"),
                base_hash: r.get::<String, _>("base_hash"),
                base_size: u64::try_from(r.get::<i64, _>("base_size")).unwrap_or(0),
                base_mtime: r.get::<i64, _>("base_mtime"),
            })
            .collect();
        Ok(entries)
    }

    pub async fn drop_agent_snapshot(&self, agent_name: &str) -> Result<()> {
        sqlx::query("DELETE FROM agent_snapshots WHERE agent_name = ?")
            .bind(agent_name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_agent_snapshots(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT DISTINCT agent_name FROM agent_snapshots")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("agent_name"))
            .collect())
    }

    pub async fn record_access_pair(
        &self,
        path: &str,
        sibling: &str,
        weight_delta: f64,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO file_access_log (path, sibling_path, weight, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(path, sibling_path) DO UPDATE SET
                weight = file_access_log.weight + excluded.weight,
                updated_at = excluded.updated_at",
        )
        .bind(path)
        .bind(sibling)
        .bind(weight_delta)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_predictive_siblings(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<(String, f64)>> {
        let rows = sqlx::query(
            "SELECT sibling_path, weight FROM file_access_log
             WHERE path = ? AND sibling_path != ?
             ORDER BY weight DESC LIMIT ?",
        )
        .bind(path)
        .bind(path)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("sibling_path"),
                    r.get::<f64, _>("weight"),
                )
            })
            .collect())
    }

    pub async fn decay_access_log(&self, factor: f64) -> Result<()> {
        sqlx::query("UPDATE file_access_log SET weight = weight * ?")
            .bind(factor)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_session_key(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO last_session (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_session_key(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM last_session WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// Updates the server-side mtime for a cached file without touching any
    /// other column. Used after a successful upload so the next sync diff
    /// sees the client view as equal to the server view, avoiding a needless
    /// re-download of the freshly-uploaded blob.
    pub async fn set_cache_server_mtime(&self, path: &str, server_mtime: i64) -> Result<()> {
        sqlx::query("UPDATE local_files SET server_mtime = ? WHERE path = ?")
            .bind(server_mtime)
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

pub fn load_config(base_path: &Path) -> Result<Config> {
    let config_path = base_path.join(".feanorfs").join("config.json");
    let content = fs::read_to_string(&config_path)
        .context("Could not read config file. Make sure you have initialized the client.")?;
    let config: Config = serde_json::from_str(&content)?;
    Ok(config)
}

pub fn save_config(base_path: &Path, config: &Config) -> Result<()> {
    let fs_dir = base_path.join(".feanorfs");
    fs::create_dir_all(&fs_dir)?;
    let config_path = fs_dir.join("config.json");
    let content = serde_json::to_string_pretty(config)?;
    fs::write(config_path, content)?;
    Ok(())
}

fn global_config_dir() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(std::path::PathBuf::from(home).join(".feanorfs"))
}

pub fn load_global_config() -> Result<GlobalConfig> {
    let path = global_config_dir()?.join("global.json");
    let content = fs::read_to_string(&path).context(
        "No server connection found. Run 'feanorfs connect <URL>' first, or pass the URL directly to 'init'.",
    )?;
    let config: GlobalConfig = serde_json::from_str(&content)?;
    Ok(config)
}

pub fn save_global_config(config: &GlobalConfig) -> Result<()> {
    let dir = global_config_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("global.json");
    let content = serde_json::to_string_pretty(config)?;
    fs::write(path, content)?;
    Ok(())
}

/// Scans the local filesystem directory, matching against .gitignore, caching file hashes.
pub async fn scan_local_directory(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
) -> Result<HashMap<String, FileState>> {
    // 1. Load cached files from SQLite DB
    let mut cached_entries = db.get_cache_entries().await?;
    let mut cache_hits = std::collections::HashSet::new();

    // 2. Scan physical files on disk
    let mut disk_files = HashMap::new();
    let walker = WalkBuilder::new(base_path)
        .hidden(false) // don't skip hidden files entirely, but we skip .git and .feanorfs manually
        .build();

    let password_str = password.unwrap_or(feanorfs_common::LEGACY_DEFAULT_PASSWORD);

    for result in walker {
        let Ok(entry) = result else { continue };

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let abs_path = entry.path();
        let Ok(rel_path) = abs_path.strip_prefix(base_path) else {
            continue;
        };

        let Some(rel_path_str) = rel_path.to_str() else {
            continue;
        };

        let normalized = normalize_path(rel_path_str);

        // Skip our control directories
        if normalized.starts_with(".feanorfs")
            || normalized.starts_with(".git")
            || normalized.contains("/.git/")
            || normalized.contains("/.feanorfs/")
        {
            continue;
        }

        let Ok(metadata) = fs::metadata(abs_path) else {
            continue;
        };

        let size = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);

        // Determine if we need to rehash/re-encrypt
        let (plaintext_hash, encrypted_hash, final_size, final_mtime, final_server_mtime, hydrated) =
            if let Some(cached) = cached_entries.get(&normalized) {
                if cached.hydrated && cached.size == size && cached.mtime == mtime {
                    cache_hits.insert(normalized.clone());
                    (
                        cached.plaintext_hash.clone(),
                        cached.encrypted_hash.clone(),
                        size,
                        mtime,
                        cached.server_mtime,
                        true,
                    )
                } else if !cached.hydrated && size == 0 {
                    cache_hits.insert(normalized.clone());
                    (
                        cached.plaintext_hash.clone(),
                        cached.encrypted_hash.clone(),
                        cached.size,
                        cached.mtime,
                        cached.server_mtime,
                        false,
                    )
                } else {
                    // Cache miss (modified file or placeholder that has local modifications)
                    let bytes = fs::read(abs_path)?;
                    let ph = feanorfs_common::hash_bytes(&bytes);
                    let encrypted_bytes =
                        feanorfs_common::crypt_bytes(&bytes, password_str, &normalized);
                    let eh = feanorfs_common::hash_bytes(&encrypted_bytes);
                    (ph, eh, size, mtime, mtime, true)
                }
            } else {
                // New file
                let bytes = fs::read(abs_path)?;
                let ph = feanorfs_common::hash_bytes(&bytes);
                let encrypted_bytes =
                    feanorfs_common::crypt_bytes(&bytes, password_str, &normalized);
                let eh = feanorfs_common::hash_bytes(&encrypted_bytes);
                (ph, eh, size, mtime, mtime, true)
            };

        let disk_entry = CacheEntry {
            path: normalized.clone(),
            plaintext_hash,
            encrypted_hash: encrypted_hash.clone(),
            size: final_size,
            mtime: final_mtime,
            server_mtime: final_server_mtime,
            hydrated,
        };

        let file_state = FileState {
            path: normalized.clone(),
            hash: encrypted_hash, // the sync engine uses encrypted hash
            size: final_size,
            mtime: final_server_mtime,
            deleted: false,
        };

        disk_files.insert(normalized, (disk_entry, file_state));
    }

    // 3. Find deleted files
    // Files that are in the cached local DB but no longer present on disk
    let mut final_files = HashMap::new();
    for (path, cached) in cached_entries.drain() {
        if !disk_files.contains_key(&path) {
            // Mark as deleted!
            let file_state = FileState {
                path: path.clone(),
                hash: cached.encrypted_hash.clone(),
                size: cached.size,
                mtime: chrono::Utc::now().timestamp_millis(), // Update time to current time so registers deletion
                deleted: true,
            };
            final_files.insert(path, file_state);
        }
    }

    for (path, (disk_entry, disk_file)) in disk_files {
        if !cache_hits.contains(&path) {
            db.upsert_cache_entry(&disk_entry).await?;
        }
        final_files.insert(path, disk_file);
    }

    Ok(final_files)
}
