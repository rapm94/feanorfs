// allow: SIZE_OK — hub migration state machine and DTO reader group under one
// domain; splitting would create artificial indirection.

use anyhow::{bail, Context, Result};
use sqlx::Row;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use feanorfs_agent_core::{
    LocalHub, MigrationHubFence, MigrationHubFile, MigrationHubManifest, MigrationHubState,
};

use super::journal::{
    self, archive_cache_db, fingerprint_component, fingerprint_component_if_exists, reset_store,
    save_journal, Fault, MigrationJournal, StorePhase,
};

pub(crate) async fn migrate_hub_store(
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
        if phase == StorePhase::Verified {
            fault.inject("before_archive")?;
            archive_cache_db(db_path, key, root, fault)?;
        }
        return Ok(());
    }
    let stored = journal.stores.get(key).cloned().unwrap_or_default();
    if !stored.db_fingerprint.is_empty() && fingerprint_component(db_path)? != stored.db_fingerprint
    {
        bail!("hub fingerprint changed for {key}");
    }
    let hub_dir = db_path.parent().context("hub parent")?;

    if phase == StorePhase::Discovered {
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(db_path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .context("connect hub")?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&pool)
            .await?;
        let dto = read_hub_dto(&pool).await?;
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
        let target = LocalHub::open_for_migration(hub_dir.to_path_buf()).await?;
        let existing = target.migration_db().export_for_migration()?;
        if existing.workspaces.is_empty() || existing == dto {
            if existing != dto {
                fault.inject("before_target_write")?;
                target.migration_db().replace_from_migration(&dto)?;
                fault.inject("after_target_write")?;
            }
            {
                let e = journal.stores.entry(key.to_string()).or_default();
                e.phase = StorePhase::Imported;
            }
            save_journal(root, journal)?;
            phase = StorePhase::Imported;
        } else {
            bail!("divergent hub state for {key}");
        }
    }
    if phase == StorePhase::Imported {
        let verify = LocalHub::open_for_migration(hub_dir.to_path_buf()).await?;
        let exported = verify.migration_db().export_for_migration()?;
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(db_path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .context("connect hub")?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&pool)
            .await?;
        let dto = read_hub_dto(&pool).await?;
        pool.close().await;
        if exported != dto {
            bail!("hub verify failed for {key}");
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

pub(crate) async fn read_hub_dto(pool: &sqlx::SqlitePool) -> Result<MigrationHubState> {
    let mut s = MigrationHubState {
        workspaces: BTreeMap::new(),
    };
    if journal::table_exists(pool, "files").await? {
        let hm = journal::col_exists(pool, "files", "mode").await?;
        let q = if hm {
            "SELECT workspace_id,path,hash,size,mtime,mode,deleted FROM files"
        } else {
            "SELECT workspace_id,path,hash,size,mtime,0 AS mode,deleted FROM files"
        };
        for r in sqlx::query(q).fetch_all(pool).await? {
            let ws: String = r.get("workspace_id");
            let p: String = r.get("path");
            s.workspaces.entry(ws).or_default().files.insert(
                p,
                MigrationHubFile {
                    hash: r.get("hash"),
                    size: feanorfs_common::file_size_from_db(r.get::<i64, _>("size")),
                    mtime: r.get("mtime"),
                    mode: r.get::<i32, _>("mode") as u32,
                    deleted: r.get::<i32, _>("deleted") != 0,
                },
            );
        }
    }
    if journal::table_exists(pool, "heads").await? {
        for r in sqlx::query("SELECT workspace_id,snapshot_id FROM heads")
            .fetch_all(pool)
            .await?
        {
            let ws: String = r.get("workspace_id");
            s.workspaces.entry(ws).or_default().head = Some(r.get("snapshot_id"));
        }
    }
    if journal::table_exists(pool, "workspace_formats").await? {
        for r in sqlx::query("SELECT workspace_id,format_version FROM workspace_formats")
            .fetch_all(pool)
            .await?
        {
            let ws: String = r.get("workspace_id");
            s.workspaces.entry(ws).or_default().format_version =
                r.get::<i64, _>("format_version") as u32;
        }
    }
    for ws in s.workspaces.values_mut() {
        if ws.format_version == 0 {
            ws.format_version = 2;
        }
    }
    if journal::table_exists(pool, "snapshot_manifests").await? {
        for r in sqlx::query(
            "SELECT workspace_id,snapshot_id,manifest,created_at_ms FROM snapshot_manifests",
        )
        .fetch_all(pool)
        .await?
        {
            let ws: String = r.get("workspace_id");
            let sid: String = r.get("snapshot_id");
            let blob: Vec<u8> = r.get("manifest");
            let text = String::from_utf8(blob).context("manifest UTF-8")?;
            s.workspaces.entry(ws).or_default().manifests.insert(
                sid,
                MigrationHubManifest {
                    hashes: text
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(str::to_string)
                        .collect(),
                    created_at_ms: r.get::<Option<i64>, _>("created_at_ms").unwrap_or(0),
                },
            );
        }
    }
    if journal::table_exists(pool, "migration_fences").await? {
        for r in sqlx::query("SELECT workspace_id,token FROM migration_fences")
            .fetch_all(pool)
            .await?
        {
            let ws: String = r.get("workspace_id");
            s.workspaces.entry(ws).or_default().migration_fence = Some(MigrationHubFence {
                token: r.get("token"),
                locked_at: 0,
            });
        }
    }
    Ok(s)
}
