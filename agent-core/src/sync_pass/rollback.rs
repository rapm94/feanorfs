//! Recovery policy and terminal outcomes for interrupted materialization.

use crate::ctx::SyncCtx;
use crate::fs_util::{atomic_write_durable, file_mtime_ms};
use crate::local::CacheEntry;
use anyhow::{Context, Result};
use feanorfs_common::is_safe_rel_path;
use std::path::Path;
use tokio::fs;

use super::download::{
    MAX_MATERIALIZATION_DIRECTORY_PROOFS, MAX_MATERIALIZATION_JOURNAL_BYTES,
    MAX_MATERIALIZATION_STAGES,
};
#[cfg(not(unix))]
use super::materialize::cleanup_materialization_directories;
#[cfg(all(not(unix), not(windows)))]
use super::materialize::portable::same_file_identity;
#[cfg(unix)]
use super::materialize::unix::{
    inspect_backup_recovery, open_materialization_anchors, portable_component,
    remove_created_descendants_for_backup, remove_created_directories_at,
    remove_current_regular_no_follow, remove_recovered_publication_no_follow,
    restore_backup_no_follow, unlink_regular_at, BackupRecoveryState,
};
#[cfg(windows)]
use super::materialize::windows::{
    classify_windows_publication_recovery, derive_windows_restore_ancestors,
    remove_retained_published_file_async, remove_verified_published_file_async,
    restore_windows_backup, windows_backup_exists_for_download, WindowsPublicationRecovery,
};
use super::materialize::{
    fingerprint_local_path, materialization_mode_matches, portable_mode, sync_directory,
    validate_worktree_ancestors, JournalDownload, MaterializationAnchors, MaterializationBackup,
    MaterializationDirectoryProof, MaterializationJournal, PublishedDownload,
};
#[cfg(all(not(unix), not(windows)))]
use super::materialize::{remove_empty_destination_tree, sync_directory_chain, sync_file};

pub(crate) async fn remove_materialization_stage(stage: &Path, base: &Path) -> Result<()> {
    fs::remove_dir_all(stage)
        .await
        .with_context(|| format!("remove materialization stage {}", stage.display()))?;
    sync_directory(base).await
}

pub(crate) async fn write_materialization_journal(
    stage: &Path,
    journal: &MaterializationJournal,
) -> Result<()> {
    let bytes = serde_json::to_vec(journal)?;
    if bytes.len() > MAX_MATERIALIZATION_JOURNAL_BYTES {
        anyhow::bail!("materialization journal exceeds bounded size");
    }
    atomic_write_durable(stage, "journal.json", &bytes).await?;
    sync_directory(stage).await
}

async fn read_materialization_journal(stage: &Path) -> Result<MaterializationJournal> {
    use tokio::io::AsyncReadExt as _;

    let path = stage.join("journal.json");
    let metadata = fs::metadata(&path).await?;
    if metadata.len() > MAX_MATERIALIZATION_JOURNAL_BYTES as u64 {
        anyhow::bail!("materialization journal exceeds bounded size");
    }
    let file = fs::File::open(&path).await?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_MATERIALIZATION_JOURNAL_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_MATERIALIZATION_JOURNAL_BYTES {
        anyhow::bail!("materialization journal exceeds bounded size");
    }
    let journal: MaterializationJournal = serde_json::from_slice(&bytes)?;
    let download_paths = journal
        .downloads
        .iter()
        .map(|item| item.file.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut published_path_set = std::collections::BTreeSet::new();
    let published_paths_valid = journal.published_paths.len()
        <= MAX_MATERIALIZATION_DIRECTORY_PROOFS
        && journal.published_paths.iter().all(|path| {
            is_safe_rel_path(path)
                && download_paths.contains(path.as_str())
                && published_path_set.insert(path.as_str())
        });
    let publishing_path_valid = journal.publishing_path.as_ref().is_none_or(|path| {
        is_safe_rel_path(path)
            && download_paths.contains(path.as_str())
            && !published_path_set.contains(path.as_str())
    });
    let publication_progress_valid = if journal.publication_progress_recorded {
        published_paths_valid && publishing_path_valid
    } else {
        journal.published_paths.is_empty() && journal.publishing_path.is_none()
    };
    let created_directories_valid = journal.created_directories.len()
        <= MAX_MATERIALIZATION_DIRECTORY_PROOFS
        && journal.created_directories.iter().all(|proof| {
            let prefix = format!("{}/", proof.path);
            is_safe_rel_path(&proof.path)
                && proof
                    .identity
                    .as_ref()
                    .is_none_or(|identity| identity.file_index != 0)
                && journal
                    .downloads
                    .iter()
                    .any(|item| item.file.path.starts_with(&prefix))
        });
    if journal.original_paths.len() > feanorfs_common::MAX_TREE_OUTPUT_PATHS
        || journal.downloads.len() > feanorfs_common::MAX_TREE_OUTPUT_PATHS
        || journal.delete_paths.len() > feanorfs_common::MAX_TREE_OUTPUT_PATHS
        || !publication_progress_valid
        || journal.created_directories.len() > MAX_MATERIALIZATION_DIRECTORY_PROOFS
        || !journal
            .original_paths
            .iter()
            .all(|path| is_safe_rel_path(path))
        || !journal
            .downloads
            .iter()
            .all(|item| is_safe_rel_path(&item.file.path))
        || !journal
            .delete_paths
            .iter()
            .all(|path| is_safe_rel_path(path))
        || !created_directories_valid
    {
        anyhow::bail!("materialization journal contains invalid or excessive paths");
    }
    Ok(journal)
}

pub(crate) async fn recover_materialization_stages(ctx: &SyncCtx<'_>) -> Result<bool> {
    let mut entries = fs::read_dir(ctx.base).await?;
    let mut stages = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(".feanorfs-tmp-materialize-") && entry.file_type().await?.is_dir() {
            stages.push(entry.path());
            if stages.len() > MAX_MATERIALIZATION_STAGES {
                anyhow::bail!("too many interrupted materialization stages");
            }
        }
    }
    stages.sort();
    let recovered = !stages.is_empty();
    for stage in stages {
        let journal = match read_materialization_journal(&stage).await {
            Ok(journal) => journal,
            Err(error) => {
                quarantine_unreadable_materialization_stage(ctx, &stage, &error).await?;
                continue;
            }
        };
        match journal.phase.as_str() {
            "preparing" => {}
            "activating" => recover_activating_materialization(ctx, &stage, &journal).await?,
            "activated" => recover_activated_materialization(ctx, &stage, &journal).await?,
            phase => anyhow::bail!("unknown materialization journal phase {phase:?}"),
        }
        remove_materialization_stage(&stage, ctx.base)
            .await
            .with_context(|| format!("remove recovered materialization {}", stage.display()))?;
    }
    Ok(recovered)
}

async fn quarantine_unreadable_materialization_stage(
    ctx: &SyncCtx<'_>,
    stage: &Path,
    error: &anyhow::Error,
) -> Result<()> {
    let mut entries = fs::read_dir(stage).await?;
    if entries.next_entry().await?.is_none() {
        remove_materialization_stage(stage, ctx.base).await?;
        tracing::warn!(
            "removed empty journal-less materialization stage {}: {error:#}",
            stage.display()
        );
        return Ok(());
    }
    // `backup/` is created only when activation is about to move an original
    // worktree file. Without a readable journal those are the sole recovery
    // copies, so never hide the stage or let a later sync proceed over it.
    if fs::symlink_metadata(stage.join("backup")).await.is_ok() {
        return Err(anyhow::anyhow!("{error:#}")).with_context(|| {
            format!(
                "materialization recovery journal is unreadable and backups remain at {}",
                stage.display()
            )
        });
    }
    // A journal-less preparing/new-only stage has not removed an original.
    // Move it out of the active recovery namespace instead of deleting unknown
    // bytes; the walker still excludes the `.feanorfs-tmp-` quarantine name.
    let name = stage
        .file_name()
        .and_then(|name| name.to_str())
        .context("materialization stage name is not UTF-8")?;
    let suffix = name
        .strip_prefix(".feanorfs-tmp-materialize-")
        .context("invalid materialization stage name")?;
    let quarantine = ctx
        .base
        .join(format!(".feanorfs-tmp-recovery-materialize-{suffix}"));
    match fs::symlink_metadata(&quarantine).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => anyhow::bail!("materialization quarantine destination already exists"),
        Err(error) => return Err(error).context("inspect materialization quarantine destination"),
    }
    fs::rename(stage, &quarantine).await.with_context(|| {
        format!(
            "quarantine unreadable materialization stage {}",
            stage.display()
        )
    })?;
    sync_directory(ctx.base).await?;
    tracing::error!(
        "quarantined unreadable materialization preparation at {}: {error:#}",
        quarantine.display()
    );
    Ok(())
}

async fn recover_activating_materialization(
    ctx: &SyncCtx<'_>,
    stage: &Path,
    journal: &MaterializationJournal,
) -> Result<()> {
    #[cfg(unix)]
    {
        let anchors = open_materialization_anchors(ctx.base, stage).await?;
        for item in journal.downloads.iter().rev() {
            validate_worktree_ancestors(ctx.base, &item.file.path).await?;
            if verify_materialized_destination(ctx, item, false)
                .await
                .is_err()
            {
                continue;
            }
            if !remove_recovered_publication_no_follow(&anchors, &item.file.path).await? {
                let _ = remove_current_regular_no_follow(&anchors, &item.file.path).await?;
            }
        }
        for path in journal.original_paths.iter().rev() {
            let mut state = inspect_backup_recovery(&anchors, path).await?;
            if state == BackupRecoveryState::Missing
                || state == BackupRecoveryState::AlreadyRestored
            {
                continue;
            }
            if state == BackupRecoveryState::DestinationOccupied {
                remove_created_descendants_for_backup(&anchors, path, &journal.downloads).await?;
                state = inspect_backup_recovery(&anchors, path).await?;
            }
            match state {
                BackupRecoveryState::Missing | BackupRecoveryState::AlreadyRestored => {}
                BackupRecoveryState::DestinationMissing => {
                    let restored = restore_backup_no_follow(&anchors, path).await?;
                    restored.sync_all()?;
                }
                BackupRecoveryState::DestinationOccupied => {
                    return Err(crate::agent::continuous::retryable_volatility_failure(
                        format!(
                            "local path {path} changed during interrupted materialization recovery"
                        ),
                    ))
                }
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let recreate_ancestors =
            derive_windows_restore_ancestors(&journal.original_paths, &journal.downloads)?;
        let has_publication_progress = journal.publication_progress_recorded;
        for item in journal.downloads.iter().rev() {
            let staged = stage.join("new").join(&item.file.path);
            let destination = ctx.base.join(&item.file.path);
            let staged_exists = match fs::symlink_metadata(&staged).await {
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            let backup_exists =
                windows_backup_exists_for_download(stage, &item.file.path, &journal.original_paths)
                    .await?;
            let destination_exists = match fs::symlink_metadata(&destination).await {
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            if has_publication_progress {
                let published = journal
                    .published_paths
                    .iter()
                    .any(|path| path == &item.file.path);
                let publishing =
                    journal.publishing_path.as_deref() == Some(item.file.path.as_str());
                if published || publishing {
                    if !staged_exists || !destination_exists {
                        anyhow::bail!(
                            "interrupted materialization {} lost a recorded publication; refusing automatic recovery",
                            item.file.path
                        );
                    }
                    let removed = remove_verified_published_file_async(
                        &staged,
                        &destination,
                        item,
                        ctx.password_str(),
                    )
                    .await
                    .with_context(|| {
                        format!("remove interrupted materialization {}", item.file.path)
                    })?;
                    if !removed {
                        anyhow::bail!(
                            "interrupted materialization {} changed; refusing automatic recovery",
                            item.file.path
                        );
                    }
                    if let Some(parent) = destination.parent() {
                        let _ = sync_directory(parent).await;
                    }
                    continue;
                }

                // No durable publication progress exists for this item. A
                // staged copy with no destination is definitively pending;
                // leave it for the original-path backup restore below. Any
                // other missing-name combination with a backup is ambiguous.
                if staged_exists && !destination_exists {
                    continue;
                }
                if (!staged_exists || !destination_exists) && backup_exists {
                    anyhow::bail!(
                        "interrupted materialization {} is missing its staged publication; refusing automatic recovery",
                        item.file.path
                    );
                }
                continue;
            }

            // Journals written before per-item publication progress retain the
            // conservative identity/content heuristic. In particular, a
            // backup with both publication names absent remains ambiguous.
            match classify_windows_publication_recovery(
                staged_exists,
                destination_exists,
                backup_exists,
            ) {
                WindowsPublicationRecovery::NotPublished => continue,
                WindowsPublicationRecovery::Ambiguous => {
                    anyhow::bail!(
                        "interrupted materialization {} is missing its staged publication; refusing automatic recovery",
                        item.file.path
                    );
                }
                WindowsPublicationRecovery::Published => {}
            }
            let removed = remove_verified_published_file_async(
                &staged,
                &destination,
                item,
                ctx.password_str(),
            )
            .await
            .with_context(|| format!("remove interrupted materialization {}", item.file.path))?;
            if !removed {
                if backup_exists {
                    anyhow::bail!(
                        "interrupted materialization {} changed; refusing automatic recovery",
                        item.file.path
                    );
                }
                continue;
            }
            if let Some(parent) = destination.parent() {
                let _ = sync_directory(parent).await;
            }
        }
        // Remove only the exact transaction-created directories before trying
        // to restore backups. A user-created/replaced directory is skipped;
        // restore then fails closed rather than recursively deleting it.
        cleanup_materialization_directories(ctx.base, &journal.created_directories).await?;
        for path in journal.original_paths.iter().rev() {
            let readonly = journal
                .original_readonly
                .get(path)
                .copied()
                .unwrap_or(false);
            restore_windows_backup(ctx.base, stage, path, readonly, &recreate_ancestors)
                .await
                .with_context(|| format!("restore interrupted materialization {path}"))?;
        }
        Ok(())
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        for item in journal.downloads.iter().rev() {
            validate_worktree_ancestors(ctx.base, &item.file.path).await?;
            let staged = stage.join("new").join(&item.file.path);
            let destination = ctx.base.join(&item.file.path);
            let staged_exists = fs::symlink_metadata(&staged).await.is_ok();
            let destination_exists = match fs::symlink_metadata(&destination).await {
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            if !destination_exists {
                continue;
            }

            let backup_exists = fs::symlink_metadata(stage.join("backup").join(&item.file.path))
                .await
                .is_ok();
            let published = if staged_exists {
                let same_identity = same_file_identity(&staged, &destination).await?;
                same_identity
                    && verify_materialized_destination(ctx, item, false)
                        .await
                        .is_ok()
            } else {
                verify_materialized_destination(ctx, item, false)
                    .await
                    .is_ok()
            };
            if !published {
                if backup_exists {
                    anyhow::bail!(
                        "interrupted materialization {} changed; refusing automatic recovery",
                        item.file.path
                    );
                }
                // A completed rollback may leave the original destination in place
                // if deleting the now-disposable stage failed. Preserve it.
                continue;
            }
            fs::remove_file(&destination).await.with_context(|| {
                format!("remove interrupted materialization {}", item.file.path)
            })?;
            if let Some(parent) = destination.parent() {
                sync_directory(parent).await?;
            }
        }
        for path in journal.original_paths.iter().rev() {
            let backup = stage.join("backup").join(path);
            let original = ctx.base.join(path);
            let backup_exists = match fs::symlink_metadata(&backup).await {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => true,
                Ok(_) => anyhow::bail!("materialization backup {path} changed type"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            if !backup_exists {
                continue;
            }
            if fs::symlink_metadata(&original).await.is_ok()
                && same_file_identity(&backup, &original).await?
            {
                fs::remove_file(&backup).await?;
                if let Some(parent) = backup.parent() {
                    sync_directory(parent).await?;
                }
                continue;
            }
            match fs::symlink_metadata(&original).await {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    remove_empty_destination_tree(&original).await?;
                }
                Ok(_) => {
                    return Err(crate::agent::continuous::retryable_volatility_failure(
                        format!(
                            "local path {path} changed during interrupted materialization recovery"
                        ),
                    ))
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(parent) = original.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::hard_link(&backup, &original)
                .await
                .with_context(|| format!("restore interrupted materialization {path}"))?;
            if let Some(parent) = original.parent() {
                sync_directory_chain(parent, ctx.base).await?;
            }
            fs::remove_file(&backup).await?;
            if let Some(parent) = backup.parent() {
                sync_directory(parent).await?;
            }
            sync_file(&original).await?;
        }
        Ok(())
    }
}

async fn recover_activated_materialization(
    ctx: &SyncCtx<'_>,
    _stage: &Path,
    journal: &MaterializationJournal,
) -> Result<()> {
    let mut cache_entries = Vec::with_capacity(journal.downloads.len());
    for item in &journal.downloads {
        verify_recovered_destination(ctx, item).await?;
        let destination = ctx.base.join(&item.file.path);
        let actual_mtime = file_mtime_ms(&destination).await.unwrap_or(item.file.mtime);
        cache_entries.push(CacheEntry {
            path: item.file.path.clone(),
            plaintext_hash: item.plaintext_hash.clone(),
            encrypted_hash: item.file.hash.clone(),
            size: item.file.size,
            mtime: actual_mtime,
            server_mtime: item.file.mtime,
            mode: item.file.mode,
            hydrated: item.hydrated,
            deleted_at: None,
        });
    }
    ctx.db
        .apply_cache_changes(&cache_entries, &journal.delete_paths)
        .await?;
    for entry in &cache_entries {
        crate::upload_registry::remember(&ctx.state_dir()?, &entry.encrypted_hash);
    }
    Ok(())
}

async fn verify_recovered_destination(ctx: &SyncCtx<'_>, item: &JournalDownload) -> Result<()> {
    verify_materialized_destination(ctx, item, true).await
}

async fn verify_materialized_destination(
    ctx: &SyncCtx<'_>,
    item: &JournalDownload,
    check_mode: bool,
) -> Result<()> {
    let destination = ctx.base.join(&item.file.path);
    if !item.hydrated {
        let metadata = fs::symlink_metadata(&destination).await?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != 0
            || (check_mode
                && !materialization_mode_matches(portable_mode(&metadata), item.file.mode))
            || (check_mode && !metadata.permissions().readonly())
        {
            return Err(crate::agent::continuous::verification_failure(format!(
                "interrupted placeholder {} changed; refusing automatic recovery",
                item.file.path
            )));
        }
        return Ok(());
    }
    let actual = fingerprint_local_path(ctx, &item.file.path, item.file.size).await?;
    if actual.hash != item.file.hash
        || actual.size != item.file.size
        || (check_mode && !materialization_mode_matches(actual.mode, item.file.mode))
    {
        return Err(crate::agent::continuous::verification_failure(format!(
            "interrupted materialization {} changed; refusing automatic recovery",
            item.file.path
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn rollback_materialization(
    ctx: &SyncCtx<'_>,
    anchors: &MaterializationAnchors,
    stage: &Path,
    original_paths: &[String],
    published: Vec<PublishedDownload>,
    backups: &[MaterializationBackup],
    downloads: &[JournalDownload],
    created_directories: &[MaterializationDirectoryProof],
) -> Result<()> {
    #[cfg(not(windows))]
    let _ = (stage, original_paths);
    #[cfg(windows)]
    let recreate_ancestors = derive_windows_restore_ancestors(original_paths, downloads)?;
    for published in published.into_iter().rev() {
        #[cfg(not(windows))]
        validate_worktree_ancestors(ctx.base, &published.expected.file.path).await?;
        #[cfg(unix)]
        {
            match verify_materialized_destination(ctx, &published.expected, published.mode_applied)
                .await
            {
                Ok(()) => {}
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        )
                    }) =>
                {
                    remove_created_directories_at(&published.created_directories)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let parent = published
                .directory_chain
                .last()
                .context("missing anchored publication parent")?;
            let name = portable_component(
                published
                    .expected
                    .file
                    .path
                    .rsplit('/')
                    .next()
                    .context("empty materialization path")?,
            )?;
            unlink_regular_at(parent, &name, &published.file).with_context(|| {
                format!(
                    "remove materialized destination {} during rollback",
                    published.destination.display()
                )
            })?;
            remove_created_directories_at(&published.created_directories)?;
            continue;
        }
        #[cfg(windows)]
        {
            // The retained handle is the ownership proof.  Reopening or
            // deleting the destination pathname could target a user-created
            // replacement after publication, so verify and dispose the exact
            // published file handle instead.
            let created_directories = published.created_directories.clone();
            remove_retained_published_file_async(&published, ctx.password_str()).await?;
            // The retained publication handle keeps a delete-pending inode
            // alive on Windows. Close it before removing transaction-created
            // parent directories or restoring backups.
            drop(published.file);
            cleanup_materialization_directories(ctx.base, &created_directories).await?;
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            match fs::symlink_metadata(&published.destination).await {
                Ok(metadata) => {
                    if !metadata.is_file() || metadata.file_type().is_symlink() {
                        return Err(crate::agent::continuous::retryable_volatility_failure(
                            format!(
                                "local path {} changed during materialization rollback",
                                published.expected.file.path
                            ),
                        ));
                    }
                    verify_materialized_destination(
                        ctx,
                        &published.expected,
                        published.mode_applied,
                    )
                    .await?;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            }
            fs::remove_file(&published.destination)
                .await
                .with_context(|| {
                    format!(
                        "remove materialized destination {} during rollback",
                        published.destination.display()
                    )
                })?;
            if let Some(parent) = published.destination.parent() {
                sync_directory(parent).await?;
            }
        }
    }
    for item in backups.iter().rev() {
        #[cfg(unix)]
        {
            remove_created_descendants_for_backup(anchors, &item.path, downloads).await?;
            let restored = restore_backup_no_follow(anchors, &item.path).await?;
            restored.sync_all()?;
            continue;
        }
        #[cfg(windows)]
        {
            restore_windows_backup(
                ctx.base,
                stage,
                &item.path,
                item.readonly,
                &recreate_ancestors,
            )
            .await
            .with_context(|| {
                format!("restore materialization backup {}", item.original.display())
            })?;
            continue;
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let backup_metadata = match fs::symlink_metadata(&item.backup).await {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    metadata
                }
                Ok(_) => anyhow::bail!(
                    "materialization backup {} changed type",
                    item.backup.display()
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            };
            let _ = backup_metadata;
            match fs::symlink_metadata(&item.original).await {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    remove_empty_destination_tree(&item.original).await?;
                }
                Ok(_) => {
                    return Err(crate::agent::continuous::retryable_volatility_failure(
                        format!(
                            "local path {} changed during materialization rollback",
                            item.original.display()
                        ),
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(parent) = item.original.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::hard_link(&item.backup, &item.original)
                .await
                .with_context(|| {
                    format!("restore materialization backup {}", item.original.display())
                })?;
            if let Some(parent) = item.original.parent() {
                sync_directory_chain(parent, ctx.base).await?;
            }
            fs::remove_file(&item.backup).await.with_context(|| {
                format!("retire materialization backup {}", item.backup.display())
            })?;
            if let Some(parent) = item.backup.parent() {
                sync_directory(parent).await?;
            }
            sync_file(&item.original).await?;
        }
    }
    #[cfg(windows)]
    cleanup_materialization_directories(ctx.base, created_directories).await?;
    #[cfg(all(not(unix), not(windows)))]
    cleanup_materialization_directories(ctx.base, created_directories).await?;
    #[cfg(unix)]
    let _ = (created_directories, downloads);
    Ok(())
}
