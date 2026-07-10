// allow: SIZE_OK — migration state machine + DTO readers group under one
// domain; splitting would create artificial indirection.

use anyhow::{bail, Context, Result};
use sqlx::Row;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use feanorfs_agent_core::{
    MigrationAccessEntry, MigrationCacheEntry, MigrationConflictRecord,
    MigrationConflictResolution, MigrationLocalState,
};

use super::journal::{
    self, archive_cache_db, fingerprint_component, fingerprint_component_if_exists, reset_store,
    save_journal, Fault, MigrationJournal, StorePhase,
};

pub(crate) async fn migrate_cache_store(
    root: &Path,
    db_path: &Path,
    key: &str,
    journal: &mut MigrationJournal,
    fault: Fault,
) -> Result<()> {
    let mut phase = journal
        .stores
        .get(key)
        .map(|s| s.phase)
        .unwrap_or(StorePhase::Discovered);
    if phase == StorePhase::Archived {
        if db_path.exists() {
            reset_store(key, journal);
            phase = StorePhase::Discovered;
        } else {
            return Ok(());
        }
    }
    if !db_path.exists() {
        if phase == StorePhase::Verified || phase == StorePhase::Imported {
            fault.inject("before_archive")?;
            archive_cache_db(db_path, key, root, fault)?;
        }
        return Ok(());
    }
    let stored = journal.stores.get(key).cloned().unwrap_or_default();
    if !stored.db_fingerprint.is_empty() {
        if fingerprint_component(db_path)? != stored.db_fingerprint {
            bail!("cache DB fingerprint changed for {key}");
        }
        let wal_p = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        if wal_p.exists()
            && !stored.wal_fingerprint.is_empty()
            && fingerprint_component(&wal_p)? != stored.wal_fingerprint
        {
            bail!("WAL fingerprint changed for {key}");
        }
        let shm_p = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        if shm_p.exists()
            && !stored.shm_fingerprint.is_empty()
            && fingerprint_component(&shm_p)? != stored.shm_fingerprint
        {
            bail!("SHM fingerprint changed for {key}");
        }
    }

    let mut dto;
    if phase == StorePhase::Discovered {
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(db_path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .context("connect")?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&pool)
            .await?;
        dto = read_cache_dto(&pool).await?;
        normalize_dto(&mut dto);
        pool.close().await;
        {
            let e = journal.stores.entry(key.to_string()).or_default();
            e.db_fingerprint = fingerprint_component(db_path)?;
            e.wal_fingerprint = fingerprint_component_if_exists(&PathBuf::from(format!(
                "{}-wal",
                db_path.to_string_lossy()
            )))?;
            e.shm_fingerprint = fingerprint_component_if_exists(&PathBuf::from(format!(
                "{}-shm",
                db_path.to_string_lossy()
            )))?;
        }
        save_journal(root, journal)?;
    } else {
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(db_path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .context("connect")?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&pool)
            .await?;
        dto = read_cache_dto(&pool).await?;
        normalize_dto(&mut dto);
        pool.close().await;
    }

    let fs_dir = db_path.parent().context("parent")?;
    if phase == StorePhase::Discovered {
        let target = feanorfs_agent_core::ClientDb::open_for_migration(fs_dir).await?;
        let mut existing = target.export_for_migration().await?;
        normalize_dto(&mut existing);
        if dto_is_empty(&existing) || existing == dto {
            if existing != dto {
                fault.inject("before_target_write")?;
                target.replace_from_migration(&dto).await?;
                fault.inject("after_target_write")?;
            }
            {
                let e = journal.stores.entry(key.to_string()).or_default();
                e.phase = StorePhase::Imported;
            }
            save_journal(root, journal)?;
            phase = StorePhase::Imported;
        } else {
            bail!("divergent SQLite and JSON state for {key}");
        }
    }
    if phase == StorePhase::Imported {
        let verify = feanorfs_agent_core::ClientDb::open_for_migration(fs_dir).await?;
        let mut exported = verify.export_for_migration().await?;
        normalize_dto(&mut exported);
        if exported != dto {
            bail!("verify failed for {key}");
        }
        {
            let e = journal.stores.entry(key.to_string()).or_default();
            e.phase = StorePhase::Verified;
        }
        save_journal(root, journal)?;
        phase = StorePhase::Verified;
    }
    if phase == StorePhase::Verified {
        fault.inject("before_archive")?;
        archive_cache_db(db_path, key, root, fault)?;
    }
    Ok(())
}

fn dto_is_empty(dto: &MigrationLocalState) -> bool {
    dto.local_files.is_empty()
        && dto.file_access_log.is_empty()
        && dto.last_session.is_empty()
        && dto.conflict_registry.is_empty()
        && dto.conflict_resolutions.is_empty()
}

pub(crate) fn normalize_dto(dto: &mut MigrationLocalState) {
    dto.file_access_log.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.sibling_path.cmp(&b.sibling_path))
    });
    dto.conflict_resolutions.sort_by(|a, b| {
        a.resolved_at
            .cmp(&b.resolved_at)
            .then_with(|| a.path.cmp(&b.path))
    });
}

pub(crate) async fn read_cache_dto(pool: &sqlx::SqlitePool) -> Result<MigrationLocalState> {
    Ok(MigrationLocalState {
        local_files: if journal::table_exists(pool, "local_files").await? {
            read_files(pool).await?
        } else {
            BTreeMap::new()
        },
        file_access_log: if journal::table_exists(pool, "file_access_log").await? {
            read_access(pool).await?
        } else {
            vec![]
        },
        last_session: if journal::table_exists(pool, "last_session").await? {
            read_sessions(pool).await?
        } else {
            BTreeMap::new()
        },
        conflict_registry: if journal::table_exists(pool, "conflict_registry").await? {
            read_conflicts(pool).await?
        } else {
            BTreeMap::new()
        },
        conflict_resolutions: if journal::table_exists(pool, "conflict_resolutions").await? {
            read_resolutions(pool).await?
        } else {
            vec![]
        },
    })
}

async fn read_files(pool: &sqlx::SqlitePool) -> Result<BTreeMap<String, MigrationCacheEntry>> {
    let (hd, hm) = (
        journal::col_exists(pool, "local_files", "deleted_at").await?,
        journal::col_exists(pool, "local_files", "mode").await?,
    );
    let q = match (hd, hm) {
        (true, true) => "SELECT path,plaintext_hash,encrypted_hash,size,mtime,server_mtime,mode,hydrated,deleted_at FROM local_files",
        (true, false) => "SELECT path,plaintext_hash,encrypted_hash,size,mtime,server_mtime,0 AS mode,hydrated,deleted_at FROM local_files",
        (false, true) => "SELECT path,plaintext_hash,encrypted_hash,size,mtime,server_mtime,mode,hydrated,CAST(NULL AS INTEGER) AS deleted_at FROM local_files",
        (false, false) => "SELECT path,plaintext_hash,encrypted_hash,size,mtime,server_mtime,0 AS mode,hydrated,CAST(NULL AS INTEGER) AS deleted_at FROM local_files",
    };
    let rows = sqlx::query(q).fetch_all(pool).await?;
    let mut m = BTreeMap::new();
    for r in rows {
        let p: String = r.get("path");
        m.insert(
            p,
            MigrationCacheEntry {
                plaintext_hash: r.get("plaintext_hash"),
                encrypted_hash: r.get("encrypted_hash"),
                size: feanorfs_common::file_size_from_db(r.get::<i64, _>("size")),
                mtime: r.get("mtime"),
                server_mtime: r.get("server_mtime"),
                mode: u32::try_from(r.get::<i32, _>("mode")).unwrap_or(0),
                hydrated: r.get::<i32, _>("hydrated") != 0,
                deleted_at: r.get::<Option<i64>, _>("deleted_at"),
            },
        );
    }
    Ok(m)
}

async fn read_access(pool: &sqlx::SqlitePool) -> Result<Vec<MigrationAccessEntry>> {
    Ok(
        sqlx::query("SELECT path,sibling_path,weight,updated_at FROM file_access_log")
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| MigrationAccessEntry {
                path: r.get("path"),
                sibling_path: r.get("sibling_path"),
                weight: r.get("weight"),
                updated_at: r.get("updated_at"),
            })
            .collect(),
    )
}

async fn read_sessions(pool: &sqlx::SqlitePool) -> Result<BTreeMap<String, String>> {
    Ok(sqlx::query("SELECT key,value FROM last_session")
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| (r.get("key"), r.get("value")))
        .collect())
}

async fn read_conflicts(
    pool: &sqlx::SqlitePool,
) -> Result<BTreeMap<String, MigrationConflictRecord>> {
    let rows = sqlx::query(
        "SELECT path,kind,conflict_dir,opened_at,COALESCE(status,'pending') AS status FROM conflict_registry",
    )
    .fetch_all(pool)
    .await?;
    let mut m = BTreeMap::new();
    for r in rows {
        let ks: String = r.get("kind");
        let kind =
            feanorfs_common::ConflictKind::from_db_str(&ks).context("invalid conflict kind")?;
        let p: String = r.get("path");
        m.insert(
            p.clone(),
            MigrationConflictRecord {
                path: p,
                kind,
                conflict_dir: r.get("conflict_dir"),
                opened_at: r.get("opened_at"),
                status: r.get("status"),
            },
        );
    }
    Ok(m)
}

async fn read_resolutions(pool: &sqlx::SqlitePool) -> Result<Vec<MigrationConflictResolution>> {
    Ok(sqlx::query(
        "SELECT path,method,source_file_hash,resolved_at,resolver FROM conflict_resolutions",
    )
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| MigrationConflictResolution {
        path: r.get("path"),
        method: r.get("method"),
        source_file_hash: r.get("source_file_hash"),
        resolved_at: r.get("resolved_at"),
        resolver: r.get("resolver"),
    })
    .collect())
}
