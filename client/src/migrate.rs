use crate::api::ApiClient;
use crate::commands::do_pull_only_with_config;
use crate::local::{load_config, save_config, validate_e2ee_key, ClientDb, Config};
use anyhow::{Context, Result};
use feanorfs_common::LegacyPolicy;
use serde::{Deserialize, Serialize};
use std::path::Path;

const JOURNAL_PATH: &str = ".feanorfs/migration-v3.json";

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum MigrationPhase {
    Pulling,
    Resealing,
    Resealed,
    HeadPublished,
    Stamped,
}

#[derive(Deserialize, Serialize)]
struct MigrationJournal {
    workspace_id: String,
    old_key: String,
    target_key: String,
    fence_token: String,
    phase: MigrationPhase,
    resealed: u32,
}

/// Migrates one legacy workspace to encrypted format-v3 snapshots.
pub async fn migrate_workspace(base: &Path, rekey: bool) -> Result<()> {
    crate::migrate_sqlite::migrate_workspace_stores(base).await?;

    let mut config = load_config(base)?;
    if config.format_version >= 3 {
        remove_journal(base).await?;
        println!("Workspace is already format v3.");
        return Ok(());
    }
    let db = crate::open_client_db(base).await?;
    let api = crate::open_api_client(base, &config).await?;
    // Preserve a CA-verified endpoint migration performed while opening the
    // client before later format/key updates write this configuration.
    config = load_config(base)?;
    let existing = load_journal(base).await?;
    if existing.is_none() && api.workspace_format(&config.workspace_id).await? >= 3 {
        let mut resumed = config.clone();
        resumed.format_version = 3;
        do_pull_only_with_config(&api, &db, base, &resumed, false).await?;
        api.set_workspace_format(&resumed.workspace_id, 3).await?;
        db.drop_legacy_snapshot_tables().await?;
        save_config(base, &resumed)?;
        println!("Migration complete. This client now uses format v3.");
        return Ok(());
    }

    let mut journal = match existing {
        Some(journal) => {
            anyhow::ensure!(
                journal.workspace_id == config.workspace_id,
                "migration journal belongs to another workspace"
            );
            journal
        }
        None => {
            let old_key = config
                .encryption_password
                .clone()
                .context("no encryption key configured")?;
            let target_key = if rekey {
                feanorfs_common::generate_password()?
            } else {
                old_key.clone()
            };
            if rekey {
                validate_e2ee_key(&target_key, 3)?;
            }
            let journal = MigrationJournal {
                workspace_id: config.workspace_id.clone(),
                old_key,
                target_key,
                fence_token: feanorfs_common::generate_password()?,
                phase: MigrationPhase::Pulling,
                resealed: 0,
            };
            write_journal(base, &journal).await?;
            journal
        }
    };
    if journal.target_key != journal.old_key {
        ensure_no_agent_workspaces(base).await?;
    }
    let api = crate::open_api_client(base, &config)
        .await?
        .with_migration_token(journal.fence_token.clone());
    api.begin_migration(&config.workspace_id).await?;

    if matches!(journal.phase, MigrationPhase::Pulling) {
        println!("Pulling latest from mirror...");
        let mut source = config.clone();
        source.encryption_password = Some(journal.old_key.clone());
        do_pull_only_with_config(&api, &db, base, &source, false).await?;
        ensure_hydrated(&db).await?;
        journal.phase = MigrationPhase::Resealing;
        write_journal(base, &journal).await?;
    }

    let mut target = config.clone();
    target.encryption_password = Some(journal.target_key.clone());
    target.format_version = 2;
    if matches!(journal.phase, MigrationPhase::Resealing) {
        let entries = db.get_cache_entries().await?;
        journal.resealed = 0;
        for (path, entry) in entries {
            if entry.deleted_at.is_none() {
                db.delete_cache_entry(&path).await?;
                journal.resealed += 1;
            }
        }
        println!("Pushing re-sealed blobs...");
        reseal_files(&api, &db, base, &target).await?;
        migration_failpoint(base, "after_reseal_upload").await?;
        journal.phase = MigrationPhase::Resealed;
        write_journal(base, &journal).await?;
    }

    if matches!(journal.phase, MigrationPhase::Resealed) {
        let ctx = feanorfs_agent_core::SyncCtx::from_config(&api, &db, base, &target)?;
        let files = feanorfs_agent_core::conflicts::load_server_view(&ctx).await?;
        let snapshots = feanorfs_agent_core::SnapshotEngine::new(&ctx);
        if journal.target_key == journal.old_key {
            snapshots.publish_server_view(&files, "migrate").await?;
        } else {
            snapshots.publish_rekeyed_view(&files, "migrate").await?;
        }
        journal.phase = MigrationPhase::HeadPublished;
        write_journal(base, &journal).await?;
    }

    if matches!(journal.phase, MigrationPhase::HeadPublished) {
        if journal.target_key != journal.old_key {
            clear_snapshot_refs(base).await?;
        }
        api.set_workspace_format(&config.workspace_id, 3).await?;
        journal.phase = MigrationPhase::Stamped;
        write_journal(base, &journal).await?;
    }

    db.drop_legacy_snapshot_tables().await?;
    config.encryption_password = Some(journal.target_key.clone());
    config.format_version = 3;
    save_config(base, &config)?;
    remove_journal(base).await?;

    if journal.target_key != journal.old_key {
        println!("New encryption key (save this and share it with other machines):");
        println!("{}", journal.target_key);
    }
    println!(
        "Migration complete. Workspace is now format v3. Re-sealed {} file(s).",
        journal.resealed
    );
    Ok(())
}

async fn ensure_hydrated(db: &ClientDb) -> Result<()> {
    let entries = db.get_cache_entries().await?;
    let dehydrated: Vec<_> = entries
        .into_iter()
        .filter(|(_, entry)| !entry.hydrated && entry.deleted_at.is_none())
        .map(|(path, _)| path)
        .collect();
    anyhow::ensure!(
        dehydrated.is_empty(),
        "cannot migrate with unhydrated placeholders; hydrate: {}",
        dehydrated.join(", ")
    );
    Ok(())
}

async fn reseal_files(api: &ApiClient, db: &ClientDb, base: &Path, config: &Config) -> Result<()> {
    let password = config
        .encryption_password
        .as_deref()
        .context("no migration target key")?;
    let files = crate::local::scan_local_directory(base, db, Some(password)).await?;
    for state in files.values().filter(|state| !state.deleted) {
        let content = tokio::fs::read(base.join(&state.path)).await?;
        let packed = feanorfs_common::pack_bytes(&content, password, &state.path)?;
        anyhow::ensure!(
            feanorfs_common::hash_bytes(&packed) == state.hash,
            "worktree changed during migration: {}",
            state.path
        );
        api.upload_file(&config.workspace_id, state, packed).await?;
    }
    Ok(())
}

async fn ensure_no_agent_workspaces(base: &Path) -> Result<()> {
    let agents = base.join(".feanorfs/agents");
    match tokio::fs::read_dir(agents).await {
        Ok(mut entries) => anyhow::ensure!(
            entries.next_entry().await?.is_none(),
            "clean or land agent workspaces before rekeying"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn clear_snapshot_refs(base: &Path) -> Result<()> {
    for relative in [".feanorfs/refs/workspace", ".feanorfs/refs/last-synced"] {
        match tokio::fs::remove_file(base.join(relative)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn load_journal(base: &Path) -> Result<Option<MigrationJournal>> {
    match tokio::fs::read(base.join(JOURNAL_PATH)).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn write_journal(base: &Path, journal: &MigrationJournal) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(journal)?;
    crate::fs_util::atomic_write(base, JOURNAL_PATH, &bytes).await
}

async fn remove_journal(base: &Path) -> Result<()> {
    match tokio::fs::remove_file(base.join(JOURNAL_PATH)).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn migration_failpoint(base: &Path, point: &str) -> Result<()> {
    let path = base.join(".feanorfs/migration-failpoint");
    if tokio::fs::read_to_string(path)
        .await
        .is_ok_and(|configured| configured.trim() == point)
    {
        anyhow::bail!("injected migration failure after re-seal upload");
    }
    Ok(())
}

pub fn legacy_policy_for_config(config: &Config) -> LegacyPolicy {
    feanorfs_agent_core::legacy_policy_for_config(config)
}
