//! Staged download preparation and bounded worktree reconstruction.

use crate::ctx::SyncCtx;
#[cfg(not(unix))]
use crate::fs_util::{apply_executable_mode, set_readonly};
use crate::fs_util::{atomic_write_visible, file_mtime_ms};
use crate::local::CacheEntry;
use anyhow::{Context, Result};
use feanorfs_common::{is_safe_rel_path, FileState, SyncResponse};
use futures_util::StreamExt as _;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;

#[cfg(unix)]
use super::materialize::unix::{
    apply_materialized_file_state, backup_original_no_follow, publish_staged_no_follow,
    sync_backup_directories, sync_published_directories,
};
#[cfg(windows)]
use super::materialize::windows::open_verified_published_file_async;
#[cfg(not(unix))]
use super::materialize::{ensure_materialization_ancestors, sync_file};
use super::materialize::{
    must_preserve_materialization_stage, open_materialization_anchors,
    preserve_materialization_stage, remove_empty_destination_tree,
    revalidate_materialization_input_at, revalidate_materialization_inputs, sync_directory,
    sync_directory_chain, validate_worktree_ancestors, JournalDownload, MaterializationBackup,
    MaterializationJournal, PublishedDownload,
};
use super::negotiate::preflight_download_projection;
use super::pause_sync_test;
use super::rollback::{
    recover_materialization_stages, remove_materialization_stage, rollback_materialization,
    write_materialization_journal,
};

struct PreparedDownload {
    file: FileState,
    #[cfg(not(unix))]
    staged_path: PathBuf,
    plaintext_hash: String,
    hydrated: bool,
}

#[doc(hidden)]
pub async fn process_downloads(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    lazy: bool,
) -> Result<(u32, u32)> {
    if recover_materialization_stages(ctx).await? {
        anyhow::bail!("recovered an interrupted materialization; retry the operation");
    }
    if response.download_required.is_empty() && response.delete_local.is_empty() {
        return Ok((0, 0));
    }
    let mut download_paths = HashSet::with_capacity(response.download_required.len());
    for file in &response.download_required {
        if !download_paths.insert(file.path.as_str()) {
            anyhow::bail!("server requested duplicate download path: {}", file.path);
        }
    }
    let mut delete_paths = HashSet::with_capacity(response.delete_local.len());
    for path in &response.delete_local {
        if !delete_paths.insert(path.as_str()) {
            anyhow::bail!("server requested duplicate local deletion path: {path}");
        }
        if download_paths.contains(path.as_str()) {
            anyhow::bail!("server requested both download and deletion for path: {path}");
        }
    }
    preflight_download_projection(ctx.base, local_files, response).await?;
    let stage = create_materialization_stage(ctx).await?;
    let result = process_downloads_in_stage(ctx, response, local_files, lazy, &stage).await;
    let preserve = result
        .as_ref()
        .err()
        .is_some_and(must_preserve_materialization_stage);
    if !preserve {
        if let Err(error) = remove_materialization_stage(&stage, ctx.base).await {
            tracing::warn!(
                "failed to remove materialization staging directory {} durably: {error}",
                stage.display()
            );
        }
    } else {
        tracing::error!(
            "materialization recovery data retained at {}",
            stage.display()
        );
    }
    result
}

async fn process_downloads_in_stage(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    lazy: bool,
    stage: &Path,
) -> Result<(u32, u32)> {
    let cached_entries = ctx.db.get_cache_entries().await?;
    let placeholder_anchors =
        capture_placeholder_anchors(ctx, response, local_files, &cached_entries)?;
    // Download, authenticate, and fsync every replacement before changing the
    // worktree. A corrupt or unavailable later object therefore cannot leave a
    // half-applied file/directory transition. Each staged file is independent,
    // so bounded concurrency batches the hub round trips; activation below
    // still applies the staged files strictly sequentially.
    let mut prepared = Vec::with_capacity(response.download_required.len());
    let mut downloads = futures_util::stream::iter(response.download_required.iter().cloned().map(
        |replica_file| {
            prepare_download(ctx, &cached_entries, local_files, lazy, stage, replica_file)
        },
    ))
    .buffer_unordered(MAX_PARALLEL_DOWNLOADS);
    while let Some(item) = downloads.next().await {
        prepared.push(item?);
    }

    pause_sync_test(ctx, "before-final-validation").await?;
    revalidate_materialization_inputs(
        ctx,
        response,
        local_files,
        &cached_entries,
        &placeholder_anchors,
    )
    .await?;
    activate_prepared_downloads(
        ctx,
        response,
        local_files,
        &cached_entries,
        &placeholder_anchors,
        prepared,
        stage,
    )
    .await
}

/// Bounded download concurrency for independent staged blob fetches.
const MAX_PARALLEL_DOWNLOADS: usize = 8;
pub(crate) const MAX_MATERIALIZATION_STAGES: usize = 64;
pub(crate) const MAX_MATERIALIZATION_JOURNAL_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_MATERIALIZATION_DIRECTORY_PROOFS: usize =
    feanorfs_common::MAX_TREE_OUTPUT_PATHS;

/// Fetches, authenticates, and fsyncs one staged download inside the
/// materialization stage. Worktree changes are never made here; activation
/// applies every staged file after the whole batch is durable.
async fn prepare_download(
    ctx: &SyncCtx<'_>,
    cached_entries: &HashMap<String, CacheEntry>,
    local_files: &HashMap<String, FileState>,
    lazy: bool,
    stage: &Path,
    replica_file: FileState,
) -> Result<PreparedDownload> {
    let path = &replica_file.path;
    if !is_safe_rel_path(path) {
        return Err(crate::agent::continuous::unsafe_path_failure(format!(
            "server requested an unsafe download path: {path}"
        )));
    }
    let full_path = ctx.base.join(path);
    let stale_local = match local_files.get(path) {
        None => false,
        Some(local) if local.deleted || !full_path.exists() => false,
        Some(_) => match cached_entries.get(path) {
            Some(entry) if !entry.hydrated => false,
            Some(entry) => match fs::metadata(&full_path).await {
                Ok(meta) => {
                    let current_mtime = file_mtime_ms(&full_path).await?;
                    current_mtime != entry.mtime || meta.len() != entry.size
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            },
            None => false,
        },
    };
    if stale_local {
        return Err(crate::agent::continuous::retryable_volatility_failure(
            format!("local path {path} changed while downloads were staged"),
        ));
    }

    let new_root = stage.join("new");
    let staged_path = new_root.join(path);
    if let Some(parent) = staged_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let (plaintext_hash, materialized_size) = if lazy {
        tracing::debug!(
            "Preparing placeholder: {} ({} bytes)",
            path,
            replica_file.size
        );
        atomic_write_visible(&new_root, path, b"").await?;
        (String::new(), replica_file.size)
    } else {
        tracing::debug!("Preparing download: {} ({} bytes)", path, replica_file.size);
        let materialized = crate::large_file::materialize_to(
            ctx,
            path,
            &replica_file.hash,
            replica_file.size,
            &staged_path,
        )
        .await?;
        (materialized.plaintext_hash, materialized.size)
    };
    if let Some(parent) = staged_path.parent() {
        sync_directory_chain(parent, stage).await?;
    }
    let mut prepared_file = replica_file.clone();
    if !lazy && prepared_file.size == 0 {
        prepared_file.size = materialized_size;
    }
    Ok(PreparedDownload {
        file: prepared_file,
        #[cfg(not(unix))]
        staged_path,
        plaintext_hash,
        hydrated: !lazy,
    })
}

async fn activate_prepared_downloads(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
    placeholder_anchors: &HashMap<String, std::fs::File>,
    mut prepared: Vec<PreparedDownload>,
    stage: &Path,
) -> Result<(u32, u32)> {
    use std::collections::{BTreeMap, BTreeSet};

    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    prepared.sort_by(|left, right| left.file.path.cmp(&right.file.path));
    let mut original_path_set = response
        .delete_local
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    original_path_set.extend(prepared.iter().map(|item| item.file.path.clone()));
    let mut original_paths = original_path_set.iter().cloned().collect::<Vec<_>>();
    original_paths.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| left.cmp(right))
    });

    ensure_same_device(ctx.base, stage, &original_paths, &prepared).await?;
    let mut original_readonly = BTreeMap::new();
    for path in &original_paths {
        if let Ok(metadata) = fs::symlink_metadata(ctx.base.join(path)).await {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                original_readonly.insert(path.clone(), metadata.permissions().readonly());
            }
        }
    }
    let mut journal = MaterializationJournal {
        phase: "activating".to_string(),
        publication_progress_recorded: true,
        original_paths: original_paths.clone(),
        original_readonly,
        downloads: prepared
            .iter()
            .map(|item| JournalDownload {
                file: item.file.clone(),
                plaintext_hash: item.plaintext_hash.clone(),
                hydrated: item.hydrated,
            })
            .collect(),
        delete_paths: response.delete_local.clone(),
        published_paths: Vec::new(),
        publishing_path: None,
        created_directories: Vec::new(),
    };
    write_materialization_journal(stage, &journal).await?;
    let backup_root = stage.join("backup");
    let anchors = open_materialization_anchors(ctx.base, stage).await?;
    let mut backups = Vec::new();
    let mut published = Vec::new();
    let activation: Result<(Vec<CacheEntry>, u32, u32)> = async {
        for path in &original_paths {
            let original = ctx.base.join(path);
            let scheduled_file_ancestor = path
                .match_indices('/')
                .map(|(index, _)| &path[..index])
                .any(|ancestor| {
                    original_path_set.contains(ancestor)
                        && local_files
                            .get(ancestor)
                            .is_some_and(|state| !state.deleted)
                });
            if !scheduled_file_ancestor {
                validate_worktree_ancestors(ctx.base, path).await?;
            }
            revalidate_materialization_input_at(
                ctx,
                path,
                &original,
                local_files.get(path),
                cached_entries,
                placeholder_anchors.get(path),
                &read_root,
            )
            .await?;
            let metadata = match fs::symlink_metadata(&original).await {
                Ok(metadata) => metadata,
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
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                continue;
            }
            let backup = backup_root.join(path);
            #[cfg(not(unix))]
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent).await?;
            }
            #[cfg(not(unix))]
            let readonly = journal
                .original_readonly
                .get(path)
                .copied()
                .unwrap_or(false);
            #[cfg(windows)]
            if readonly {
                set_readonly(&original, false).await?;
            }
            if !scheduled_file_ancestor {
                validate_worktree_ancestors(ctx.base, path).await?;
            }
            let point = format!("before-backup-{}", backups.len() + 1);
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            #[cfg(unix)]
            let (original_parent, backup_parent) =
                backup_original_no_follow(&anchors, path).await?;
            #[cfg(not(unix))]
            if let Err(error) = fs::rename(&original, &backup).await {
                #[cfg(windows)]
                if readonly {
                    let _ = set_readonly(&original, true).await;
                }
                return Err(error).with_context(|| format!("back up local path {path}"));
            }
            backups.push(MaterializationBackup {
                path: path.clone(),
                #[cfg(not(unix))]
                original,
                #[cfg(not(unix))]
                backup: backup.clone(),
                #[cfg(not(unix))]
                readonly,
                #[cfg(unix)]
                original_parent,
                #[cfg(unix)]
                backup_parent,
            });
            // Record the namespace mutation before any fallible durability
            // step so rollback still owns the moved original if fsync fails.
            let mutation_point = format!("after-backup-mutation-{}", backups.len());
            pause_sync_test(ctx, &mutation_point).await?;
            inject_materialization_failure(ctx, &mutation_point).await?;
            #[cfg(unix)]
            sync_backup_directories(backups.last().context("missing backup state")?).await?;
            #[cfg(not(unix))]
            if let Some(parent) = backups.last().and_then(|item| item.original.parent()) {
                sync_directory(parent).await?;
            }
            #[cfg(not(unix))]
            if let Some(parent) = backup.parent() {
                sync_directory_chain(parent, stage).await?;
            }
            #[cfg(windows)]
            if readonly {
                set_readonly(&backup, true).await?;
            }
            // Verify the file after the atomic move. An edit racing the
            // pre-move fingerprint is now preserved in the backup and aborts
            // the transaction rather than being silently discarded.
            revalidate_materialization_input_at(
                ctx,
                path,
                &backup,
                local_files.get(path),
                cached_entries,
                placeholder_anchors.get(path),
                &read_root,
            )
            .await?;
            let point = format!("after-backup-{}", backups.len());
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
        }

        let mut cache_entries = Vec::with_capacity(prepared.len());
        let mut downloads = 0_u32;
        let mut placeholders = 0_u32;
        for item in prepared {
            let path = &item.file.path;
            let destination = ctx.base.join(path);
            let point = format!("before-publish-{}", published.len() + 1);
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            validate_worktree_ancestors(ctx.base, path).await?;
            remove_empty_destination_tree(&destination).await?;
            validate_worktree_ancestors(ctx.base, path).await?;
            match fs::symlink_metadata(&destination).await {
                Ok(_) => anyhow::bail!(
                    "local path {path} appeared while a staged download was being published"
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            let point = format!("after-final-validation-{}", published.len() + 1);
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            let expected = JournalDownload {
                file: item.file.clone(),
                plaintext_hash: item.plaintext_hash.clone(),
                hydrated: item.hydrated,
            };
            // Publish relative to already-open, no-follow directory handles on
            // Unix. This prevents an ancestor swap from redirecting the link
            // or subsequent mode/fsync operations outside the workspace.
            #[cfg(unix)]
            let (published_file, published_directories, created_directories) =
                publish_staged_no_follow(ctx.base, stage, path).await?;
            #[cfg(not(unix))]
            let publication_created_directories = {
                // A same-device hard link is an atomic no-clobber publication.
                // Unlike rename it cannot replace a path created after the
                // preceding absence check.
                let created_directories =
                    ensure_materialization_ancestors(ctx.base, path, stage, &mut journal).await?;
                #[cfg(windows)]
                {
                    // Persist the in-flight marker before the namespace
                    // mutation. A crash after this write but before the hard
                    // link must be treated as ambiguous, never guessed as a
                    // completed publication.
                    journal.publishing_path = Some(path.clone());
                    write_materialization_journal(stage, &journal).await?;
                }
                fs::hard_link(&item.staged_path, &destination)
                    .await
                    .with_context(|| format!("publish staged download {path}"))?;
                created_directories
            };
            #[cfg(windows)]
            let published_file = match open_verified_published_file_async(
                &item.staged_path,
                &destination,
                &expected,
                ctx.password_str(),
            )
            .await
            {
                Ok(file) => file,
                Err(error) => {
                    // Publication already created a hard link, but no
                    // ownership record can be constructed without a
                    // validated destination handle. Keep the journal/stage
                    // for handle-based crash recovery instead of allowing
                    // the ordinary rollback to discard its only proof.
                    return Err(preserve_materialization_stage(
                        error.context(format!("verify published download {path}")),
                    ));
                }
            };
            published.push(PublishedDownload {
                destination: destination.clone(),
                expected,
                mode_applied: false,
                #[cfg(unix)]
                file: published_file,
                #[cfg(windows)]
                file: published_file,
                #[cfg(unix)]
                directory_chain: published_directories,
                #[cfg(unix)]
                created_directories,
                #[cfg(not(unix))]
                created_directories: publication_created_directories,
            });
            #[cfg(windows)]
            {
                // The handle proof is now retained in `published`; record
                // the completed namespace mutation and clear the in-flight
                // marker. If this write fails, ordinary rollback still owns
                // the exact destination handle and the journal remains
                // conservatively marked as publishing for crash recovery.
                journal.published_paths.push(path.clone());
                journal.publishing_path = None;
                write_materialization_journal(stage, &journal).await?;
            }
            // Rollback ownership begins at the namespace mutation, not after
            // the following directory durability step succeeds.
            let mutation_point = format!("after-publish-mutation-{}", published.len());
            pause_sync_test(ctx, &mutation_point).await?;
            inject_materialization_failure(ctx, &mutation_point).await?;
            #[cfg(unix)]
            sync_published_directories(published.last().context("missing publication state")?)
                .await?;
            #[cfg(not(unix))]
            if let Some(parent) = destination.parent() {
                sync_directory_chain(parent, ctx.base).await?;
            }
            // Keep the staged hard link until the cache commits and the whole
            // stage is durably removed. Crash recovery can therefore identify
            // the exact published inode instead of trusting content equality.
            let point = format!("after-publish-{}", published.len());
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            #[cfg(unix)]
            let actual_mtime = apply_materialized_file_state(
                &published.last().context("missing publication state")?.file,
                item.file.mode,
                !item.hydrated,
            )
            .await?;
            #[cfg(not(unix))]
            {
                apply_executable_mode(&destination, item.file.mode).await?;
            }
            if let Some(published) = published.last_mut() {
                published.mode_applied = true;
            }
            let point = format!("after-mode-{}", published.len());
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            if !item.hydrated {
                placeholders += 1;
            } else {
                downloads += 1;
            }
            #[cfg(not(unix))]
            sync_file(&destination).await?;
            if !item.hydrated {
                #[cfg(not(unix))]
                set_readonly(&destination, true).await?;
            }
            #[cfg(not(unix))]
            let actual_mtime = file_mtime_ms(&destination).await.unwrap_or_else(|error| {
                tracing::warn!(
                    "failed to read mtime after download of {path}: {error}; using server mtime"
                );
                item.file.mtime
            });

            cache_entries.push(CacheEntry {
                path: path.clone(),
                plaintext_hash: item.plaintext_hash,
                encrypted_hash: item.file.hash.clone(),
                size: item.file.size,
                mtime: actual_mtime,
                server_mtime: item.file.mtime,
                mode: item.file.mode,
                hydrated: item.hydrated,
                deleted_at: None,
            });
        }
        Ok((cache_entries, downloads, placeholders))
    }
    .await;

    let (cache_entries, downloads, placeholders) = match activation {
        Ok(value) => value,
        Err(error) => {
            if let Err(rollback) = rollback_materialization(
                ctx,
                &anchors,
                stage,
                &journal.original_paths,
                published,
                &backups,
                &journal.downloads,
                &journal.created_directories,
            )
            .await
            {
                return Err(preserve_materialization_stage(
                    error.context(format!("materialization rollback failed: {rollback:#}")),
                ));
            }
            return Err(error);
        }
    };
    journal.phase = "activated".to_string();
    if let Err(error) = write_materialization_journal(stage, &journal).await {
        if let Err(rollback) = rollback_materialization(
            ctx,
            &anchors,
            stage,
            &journal.original_paths,
            published,
            &backups,
            &journal.downloads,
            &journal.created_directories,
        )
        .await
        {
            return Err(preserve_materialization_stage(error.context(format!(
                "journal update and materialization rollback failed: {rollback:#}"
            ))));
        }
        return Err(error).context("mark materialization activated");
    }
    let before_cache = match pause_sync_test(ctx, "before-cache").await {
        Ok(()) => inject_materialization_failure(ctx, "before-cache").await,
        Err(error) => Err(error),
    };
    if let Err(error) = before_cache {
        if let Err(rollback) = rollback_materialization(
            ctx,
            &anchors,
            stage,
            &journal.original_paths,
            published,
            &backups,
            &journal.downloads,
            &journal.created_directories,
        )
        .await
        {
            return Err(preserve_materialization_stage(
                error.context(format!("materialization rollback failed: {rollback:#}")),
            ));
        }
        return Err(error);
    }
    if let Err(error) = ctx
        .db
        .apply_cache_changes(&cache_entries, &response.delete_local)
        .await
    {
        if crate::durable::commit_durability_is_uncertain(&error) {
            return Err(preserve_materialization_stage(
                error.context("commit materialized cache state"),
            ));
        }
        if let Err(rollback) = rollback_materialization(
            ctx,
            &anchors,
            stage,
            &journal.original_paths,
            published,
            &backups,
            &journal.downloads,
            &journal.created_directories,
        )
        .await
        {
            return Err(preserve_materialization_stage(error.context(format!(
                "cache commit and materialization rollback failed: {rollback:#}"
            ))));
        }
        return Err(error).context("commit materialized cache state");
    }
    for entry in &cache_entries {
        crate::upload_registry::remember(&ctx.state_dir()?, &entry.encrypted_hash);
    }
    Ok((downloads, placeholders))
}

async fn inject_materialization_failure(ctx: &SyncCtx<'_>, point: &str) -> Result<()> {
    #[cfg(debug_assertions)]
    {
        let path = ctx.state_dir()?.join("test-materialize-failpoint");
        if fs::read_to_string(&path).await.ok().as_deref() == Some(point) {
            fs::remove_file(path).await?;
            anyhow::bail!("injected materialization failure at {point}");
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = (ctx, point);
    Ok(())
}

async fn ensure_same_device(
    base: &Path,
    stage: &Path,
    original_paths: &[String],
    prepared: &[PreparedDownload],
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let stage_device = fs::metadata(stage).await?.dev();
        for path in original_paths {
            match fs::symlink_metadata(base.join(path)).await {
                Ok(metadata) if metadata.dev() != stage_device => {
                    anyhow::bail!("materialization path {path} is on a different filesystem")
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        for item in prepared {
            let mut ancestor = base.join(&item.file.path);
            while !ancestor.exists() {
                if !ancestor.pop() {
                    anyhow::bail!("materialization destination escaped workspace");
                }
            }
            if fs::metadata(&ancestor).await?.dev() != stage_device {
                anyhow::bail!(
                    "materialization destination {} is on a different filesystem",
                    item.file.path
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (base, stage, original_paths, prepared);
    }
    Ok(())
}

fn capture_placeholder_anchors(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
) -> Result<HashMap<String, std::fs::File>> {
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    let mut anchors = HashMap::new();
    for path in response
        .download_required
        .iter()
        .map(|file| file.path.as_str())
        .chain(response.delete_local.iter().map(String::as_str))
    {
        let Some(expected) = local_files.get(path).filter(|state| !state.deleted) else {
            continue;
        };
        if !cached_entries
            .get(path)
            .is_some_and(|entry| !entry.hydrated && entry.encrypted_hash == expected.hash)
        {
            continue;
        }
        let file = read_root
            .open_regular(path)
            .with_context(|| format!("anchor local placeholder {path} before staging"))?;
        anchors.insert(path.to_string(), file);
    }
    Ok(anchors)
}

pub(crate) async fn prefetch_downloads(ctx: &SyncCtx<'_>, response: &SyncResponse) -> Result<()> {
    if response.download_required.is_empty() {
        return Ok(());
    }
    if recover_materialization_stages(ctx).await? {
        anyhow::bail!("recovered an interrupted materialization; retry the operation");
    }
    let stage = create_materialization_stage(ctx).await?;
    // Prefetch is cache-warming only: each file writes into the disposable
    // stage, so independent files can be fetched with bounded concurrency.
    let result = async {
        let mut prefetches =
            futures_util::stream::iter(response.download_required.iter().cloned().map(|file| {
                let stage = stage.clone();
                async move {
                    if !is_safe_rel_path(&file.path) {
                        anyhow::bail!("server requested an unsafe download path: {}", file.path);
                    }
                    let destination = stage.join("new").join(&file.path);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    crate::large_file::materialize_to(
                        ctx,
                        &file.path,
                        &file.hash,
                        file.size,
                        &destination,
                    )
                    .await
                }
            }))
            .buffer_unordered(MAX_PARALLEL_DOWNLOADS);
        while let Some(result) = prefetches.next().await {
            result?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = remove_materialization_stage(&stage, ctx.base).await {
        tracing::warn!(
            "failed to remove prefetch staging directory {} durably: {error}",
            stage.display()
        );
    }
    result
}

async fn create_materialization_stage(ctx: &SyncCtx<'_>) -> Result<PathBuf> {
    let base = ctx.base;
    for _ in 0..16 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).context("generate materialization staging id")?;
        let id = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = base.join(format!(".feanorfs-tmp-materialize-{id}"));
        match fs::create_dir(&path).await {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await?;
                }
                pause_sync_test(ctx, "stage-before-journal").await?;
                let journal = MaterializationJournal {
                    phase: "preparing".to_string(),
                    publication_progress_recorded: false,
                    original_paths: Vec::new(),
                    original_readonly: std::collections::BTreeMap::new(),
                    downloads: Vec::new(),
                    delete_paths: Vec::new(),
                    published_paths: Vec::new(),
                    publishing_path: None,
                    created_directories: Vec::new(),
                };
                if let Err(error) = write_materialization_journal(&path, &journal).await {
                    let _ = remove_materialization_stage(&path, base).await;
                    return Err(error);
                }
                sync_directory(base).await?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create materialization staging directory"),
        }
    }
    anyhow::bail!("could not allocate a materialization staging directory")
}
