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
                size: r.get::<i64, _>("size") as u64,
                mtime: r.get::<i64, _>("mtime"),
                server_mtime: r.get::<i64, _>("server_mtime"),
                hydrated: r.get::<i32, _>("hydrated") != 0,
            };
            files.insert(path, entry);
        }
        Ok(files)
    }

    pub async fn upsert_cache_entry(&self, entry: &CacheEntry) -> Result<()> {
        let size = entry.size as i64;
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

    // 2. Scan physical files on disk
    let mut disk_files = HashMap::new();
    let walker = WalkBuilder::new(base_path)
        .hidden(false) // don't skip hidden files entirely, but we skip .git and .feanorfs manually
        .build();

    let password_str = password.unwrap_or("default-secret-key");

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let abs_path = entry.path();
        let rel_path = match abs_path.strip_prefix(base_path) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let rel_path_str = match rel_path.to_str() {
            Some(s) => s,
            None => continue,
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

        let metadata = match fs::metadata(abs_path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let size = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Determine if we need to rehash/re-encrypt
        let (plaintext_hash, encrypted_hash, final_size, final_mtime, final_server_mtime, hydrated) =
            if let Some(cached) = cached_entries.get(&normalized) {
                if cached.hydrated && cached.size == size && cached.mtime == mtime {
                    // Cache hit! Skip hashing and encryption
                    (
                        cached.plaintext_hash.clone(),
                        cached.encrypted_hash.clone(),
                        size,
                        mtime,
                        cached.server_mtime,
                        true,
                    )
                } else if !cached.hydrated && size == 0 {
                    // Unhydrated placeholder that has not been modified (size is still 0)
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

    // Insert active disk files
    for (path, (disk_entry, disk_file)) in disk_files {
        // Update client database with current scanned state
        db.upsert_cache_entry(&disk_entry).await?;
        final_files.insert(path, disk_file);
    }

    Ok(final_files)
}
