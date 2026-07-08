use anyhow::{bail, Context, Result};
use feanorfs_common::{file_size_from_db, file_size_to_db, normalize_path, FileState};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

static WARNED_LEGACY_PASSWORD_SCAN: OnceLock<()> = OnceLock::new();

fn default_format_version() -> u32 {
    1
}

/// Built-in denylist for derivative build/cache dirs (DX-3).
pub const DEFAULT_IGNORES: &[&str] = &[
    "target/",
    "node_modules/",
    ".DS_Store",
    "*.swp",
    "*~",
    ".venv/",
    "__pycache__/",
    "dist/",
    "build/",
    ".next/",
    ".cache/",
];

/// Normalize path to NFC for cross-platform consistency (DX-16).
#[must_use]
pub fn normalize_path_nfc(path: &str) -> String {
    normalize_path(&path.nfc().collect::<String>())
}

/// Validate E2EE key for format v2 workspaces (SEC-7).
pub fn validate_e2ee_key(key: &str, format_version: u32) -> Result<()> {
    if format_version < 2 {
        return Ok(());
    }
    if key.len() != 64 || !key.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        bail!(
            "Encryption key must be exactly 64 lowercase hex characters (generated keys only). \
             Human passphrases are not accepted on format v2 workspaces."
        );
    }
    Ok(())
}

pub fn build_workspace_walker(base_path: &Path, no_default_ignores: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(base_path);
    builder
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false);

    if !no_default_ignores {
        let mut igb = ignore::gitignore::GitignoreBuilder::new(base_path);
        for pattern in DEFAULT_IGNORES {
            let _ = igb.add_line(None, pattern);
        }
        if let Ok(content) = fs::read_to_string(base_path.join(".feanorfsignore")) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let _ = igb.add_line(None, line);
            }
        }
        if let Ok(ig) = igb.build() {
            let base = base_path.to_path_buf();
            builder.filter_entry(move |e| {
                if !e.file_type().is_some_and(|ft| ft.is_file()) {
                    return true;
                }
                let Ok(rel) = e.path().strip_prefix(&base) else {
                    return true;
                };
                let Some(s) = rel.to_str() else {
                    return true;
                };
                !ig.matched(s, false).is_ignore()
            });
        }
    }
    builder
}

/// Symlink paths under the workspace (DX-19).
pub fn collect_symlink_warnings(base_path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for result in build_workspace_walker(base_path, false).build() {
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_symlink()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(base_path) else {
            continue;
        };
        let Some(s) = rel.to_str() else {
            continue;
        };
        let normalized = normalize_path_nfc(s);
        if feanorfs_common::is_safe_rel_path(&normalized) {
            out.push(normalized);
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    pub workspace_id: String,
    pub encryption_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_password: Option<String>,
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    /// When true, sync uses an embedded hub at `.feanorfs/hub-data` (CONN-2).
    #[serde(default)]
    pub hub_local: bool,
}

/// Sentinel URL written for embedded local hubs (CONN-2).
pub const LOCAL_HUB_URL: &str = "feanorfs+local://hub";

impl Config {
    #[must_use]
    pub fn is_local_hub(&self) -> bool {
        self.hub_local || self.server_url == LOCAL_HUB_URL
    }

    #[must_use]
    pub fn hub_data_dir(&self, workspace: &Path) -> PathBuf {
        workspace.join(".feanorfs").join("hub-data")
    }
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
    pub deleted_at: Option<i64>,
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
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&self.pool)
            .await?;
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(&self.pool)
            .await?;
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

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS last_synced_files (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0
            );",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS conflict_registry (
                path TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                conflict_dir TEXT NOT NULL,
                opened_at INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending'
            );",
        )
        .execute(&self.pool)
        .await?;

        self.migrate_local_files_deleted_at().await?;
        self.migrate_conflict_resolutions().await?;

        Ok(())
    }

    async fn migrate_conflict_resolutions(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS conflict_resolutions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL,
                method TEXT NOT NULL,
                source_file_hash TEXT,
                resolved_at INTEGER NOT NULL,
                resolver TEXT NOT NULL
            );",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn migrate_local_files_deleted_at(&self) -> Result<()> {
        let rows = sqlx::query("PRAGMA table_info(local_files)")
            .fetch_all(&self.pool)
            .await?;
        let has_col = rows
            .iter()
            .any(|r| r.get::<String, _>("name") == "deleted_at");
        if !has_col {
            sqlx::query("ALTER TABLE local_files ADD COLUMN deleted_at INTEGER")
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn load_last_synced_files(&self) -> Result<HashMap<String, FileState>> {
        let rows = sqlx::query("SELECT path, hash, size, mtime, deleted FROM last_synced_files")
            .fetch_all(&self.pool)
            .await?;
        let mut out = HashMap::new();
        for r in rows {
            let path = r.get::<String, _>("path");
            out.insert(
                path.clone(),
                FileState {
                    path,
                    hash: r.get::<String, _>("hash"),
                    size: file_size_from_db(r.get::<i64, _>("size")),
                    mtime: r.get::<i64, _>("mtime"),
                    deleted: r.get::<i32, _>("deleted") != 0,
                },
            );
        }
        Ok(out)
    }

    pub async fn replace_last_synced_files(
        &self,
        states: &HashMap<String, FileState>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM last_synced_files")
            .execute(&mut *tx)
            .await?;
        for f in states.values() {
            sqlx::query(
                "INSERT INTO last_synced_files (path, hash, size, mtime, deleted)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&f.path)
            .bind(&f.hash)
            .bind(file_size_to_db(f.size))
            .bind(f.mtime)
            .bind(if f.deleted { 1 } else { 0 })
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_pending_conflict_paths(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT path FROM conflict_registry WHERE status = 'pending'")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("path"))
            .collect())
    }

    pub async fn list_conflict_records(&self) -> Result<Vec<feanorfs_common::ConflictRecord>> {
        let rows = sqlx::query(
            "SELECT path, kind, conflict_dir, opened_at, status FROM conflict_registry WHERE status = 'pending'",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let kind = feanorfs_common::ConflictKind::from_db_str(
                    &r.get::<String, _>("kind"),
                )?;
                Ok(feanorfs_common::ConflictRecord {
                    path: r.get("path"),
                    kind,
                    conflict_dir: r.get("conflict_dir"),
                    opened_at: r.get("opened_at"),
                    status: r.get("status"),
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub async fn get_conflict_record(
        &self,
        path: &str,
    ) -> Result<Option<feanorfs_common::ConflictRecord>> {
        let row = sqlx::query(
            "SELECT path, kind, conflict_dir, opened_at, status FROM conflict_registry WHERE path = ? AND status = 'pending'",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let kind =
                feanorfs_common::ConflictKind::from_db_str(&r.get::<String, _>("kind"))?;
            Ok(feanorfs_common::ConflictRecord {
                path: r.get("path"),
                kind,
                conflict_dir: r.get("conflict_dir"),
                opened_at: r.get("opened_at"),
                status: r.get("status"),
            })
        })
        .transpose()
    }

    pub async fn upsert_conflict(
        &self,
        path: &str,
        kind: &feanorfs_common::ConflictKind,
        conflict_dir: &str,
        opened_at: i64,
        status: &str,
    ) -> Result<()> {
        let kind_str = kind.as_db_str();
        sqlx::query(
            "INSERT OR REPLACE INTO conflict_registry (path, kind, conflict_dir, opened_at, status)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(path)
        .bind(kind_str)
        .bind(conflict_dir)
        .bind(opened_at)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn resolve_conflict_path(&self, path: &str) -> Result<()> {
        sqlx::query("DELETE FROM conflict_registry WHERE path = ?")
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_conflict_resolution(
        &self,
        path: &str,
        method: &str,
        source_file_hash: Option<&str>,
        resolver: &str,
    ) -> Result<()> {
        let resolved_at = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO conflict_resolutions (path, method, source_file_hash, resolved_at, resolver)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(path)
        .bind(method)
        .bind(source_file_hash)
        .bind(resolved_at)
        .bind(resolver)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_conflict_resolutions(
        &self,
    ) -> Result<Vec<feanorfs_common::ConflictResolution>> {
        let rows = sqlx::query(
            "SELECT path, method, source_file_hash, resolved_at, resolver FROM conflict_resolutions ORDER BY resolved_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| feanorfs_common::ConflictResolution {
                path: r.get("path"),
                method: r.get("method"),
                source_file_hash: r.get("source_file_hash"),
                resolved_at: r.get("resolved_at"),
                resolver: r.get("resolver"),
            })
            .collect())
    }

    pub async fn count_pending_in_dir(&self, conflict_dir: &str) -> Result<u32> {
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM conflict_registry WHERE conflict_dir = ? AND status = 'pending'",
        )
        .bind(conflict_dir)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("cnt") as u32)
    }

    pub async fn upsert_last_synced_files(
        &self,
        updates: &HashMap<String, FileState>,
        exclude_paths: &HashSet<String>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (path, file) in updates {
            if exclude_paths.contains(path) {
                continue;
            }
            sqlx::query(
                "INSERT OR REPLACE INTO last_synced_files (path, hash, size, mtime, deleted)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&file.path)
            .bind(&file.hash)
            .bind(file_size_to_db(file.size))
            .bind(file.mtime)
            .bind(if file.deleted { 1 } else { 0 })
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_deleted_at(&self, path: &str, deleted_at: i64) -> Result<()> {
        sqlx::query("UPDATE local_files SET deleted_at = ? WHERE path = ?")
            .bind(deleted_at)
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_cache_entries(&self) -> Result<HashMap<String, CacheEntry>> {
        let rows = sqlx::query(
            "SELECT path, plaintext_hash, encrypted_hash, size, mtime, server_mtime, hydrated, deleted_at FROM local_files"
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
                size: file_size_from_db(r.get::<i64, _>("size")),
                mtime: r.get::<i64, _>("mtime"),
                server_mtime: r.get::<i64, _>("server_mtime"),
                hydrated: r.get::<i32, _>("hydrated") != 0,
                deleted_at: r.get::<Option<i64>, _>("deleted_at"),
            };
            files.insert(path, entry);
        }
        Ok(files)
    }

    pub async fn upsert_cache_entry(&self, entry: &CacheEntry) -> Result<()> {
        let size = file_size_to_db(entry.size);
        let hydrated = if entry.hydrated { 1 } else { 0 };
        sqlx::query(
            "INSERT OR REPLACE INTO local_files (path, plaintext_hash, encrypted_hash, size, mtime, server_mtime, hydrated, deleted_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&entry.path)
        .bind(&entry.plaintext_hash)
        .bind(&entry.encrypted_hash)
        .bind(size)
        .bind(entry.mtime)
        .bind(entry.server_mtime)
        .bind(hydrated)
        .bind(entry.deleted_at)
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
            .bind(file_size_to_db(entry.base_size))
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
                base_size: file_size_from_db(r.get::<i64, _>("base_size")),
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

    pub async fn update_agent_snapshot_base(
        &self,
        agent_name: &str,
        path: &str,
        state: &feanorfs_common::FileState,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE agent_snapshots SET base_hash = ?, base_size = ?, base_mtime = ?
             WHERE agent_name = ? AND path = ?",
        )
        .bind(&state.hash)
        .bind(file_size_to_db(state.size))
        .bind(state.mtime)
        .bind(agent_name)
        .bind(path)
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
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME or USERPROFILE environment variable not set")?;
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

/// Scans the local filesystem directory, caching file hashes.
/// Honors built-in default ignores and `.feanorfsignore` (DX-3/DX-4).
pub async fn scan_local_directory(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
) -> Result<HashMap<String, FileState>> {
    scan_local_directory_with_opts(base_path, db, password, false).await
}

pub async fn scan_local_directory_with_opts(
    base_path: &Path,
    db: &ClientDb,
    password: Option<&str>,
    no_default_ignores: bool,
) -> Result<HashMap<String, FileState>> {
    let mut cached_entries = db.get_cache_entries().await?;
    let mut cache_hits = std::collections::HashSet::new();

    let mut disk_files = HashMap::new();
    let walker = build_workspace_walker(base_path, no_default_ignores).build();

    let password_str = password.unwrap_or_else(|| {
        WARNED_LEGACY_PASSWORD_SCAN.get_or_init(|| {
            tracing::warn!(
                "No E2EE password set. Using insecure legacy default for directory scan."
            );
        });
        feanorfs_common::LEGACY_DEFAULT_PASSWORD
    });

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

        let normalized = normalize_path_nfc(rel_path_str);

        if !feanorfs_common::is_safe_rel_path(&normalized) {
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
                } else if !cached.hydrated {
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
                    let bytes = fs::read(abs_path)?;
                    let meta_after = fs::metadata(abs_path)?;
                    let size_after = meta_after.len();
                    let mtime_after = meta_after
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                        .unwrap_or(0);
                    if size_after != size || mtime_after != mtime {
                        // Partial write during scan (DX-13a) — reuse cache for
                        // this pass so we do not tombstone a live file.
                        cache_hits.insert(normalized.clone());
                        (
                            cached.plaintext_hash.clone(),
                            cached.encrypted_hash.clone(),
                            cached.size,
                            cached.mtime,
                            cached.server_mtime,
                            cached.hydrated,
                        )
                    } else {
                        let ph = feanorfs_common::hash_bytes(&bytes);
                        let encrypted_bytes =
                            feanorfs_common::pack_bytes(&bytes, password_str, &normalized)?;
                        let eh = feanorfs_common::hash_bytes(&encrypted_bytes);
                        (ph, eh, size, mtime, mtime, true)
                    }
                }
            } else {
                let bytes = fs::read(abs_path)?;
                let meta_after = fs::metadata(abs_path)?;
                let size_after = meta_after.len();
                let mtime_after = meta_after
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                    .unwrap_or(0);
                if size_after != size || mtime_after != mtime {
                    continue;
                }
                let ph = feanorfs_common::hash_bytes(&bytes);
                let encrypted_bytes =
                    feanorfs_common::pack_bytes(&bytes, password_str, &normalized)?;
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
            deleted_at: None,
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

    let mut final_files = HashMap::new();
    for (path, cached) in cached_entries.drain() {
        if !disk_files.contains_key(&path) {
            let tombstone_mtime = cached
                .deleted_at
                .unwrap_or_else(|| cached.server_mtime.max(cached.mtime).saturating_add(1));
            if cached.deleted_at.is_none() {
                db.set_deleted_at(&path, tombstone_mtime).await?;
            }
            let file_state = FileState {
                path: path.clone(),
                hash: cached.encrypted_hash.clone(),
                size: cached.size,
                mtime: tombstone_mtime,
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
