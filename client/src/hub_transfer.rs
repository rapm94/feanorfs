//! Fail-closed transfer of one encrypted format-v3 workspace between hubs.

use crate::{load_config, open_api_client, open_client_db, save_config_secure, ApiClient, SyncCtx};
use anyhow::{bail, Context, Result};
use feanorfs_agent_core::{lock::SyncLock, ObjectStore, SwapHeadResult};
use feanorfs_common::{hash_bytes, is_valid_hash, TreeEntryKind};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HubTransferResult {
    pub workspace_id: String,
    pub snapshots: usize,
    pub objects: usize,
    pub local_objects_seeded: usize,
    pub destination_url: String,
    pub format_version: u32,
    pub configuration_updated: bool,
}

struct SnapshotManifest {
    snapshot_id: String,
    hashes: Vec<String>,
}

struct ReachableHistory {
    manifests: Vec<SnapshotManifest>,
    hashes: BTreeSet<String>,
}

/// Copies a workspace's complete reachable encrypted history to another configured hub.
///
/// `source_url` may use plaintext HTTP only on loopback. Destination trust and credentials
/// are loaded from `destination_workspace`; no secret is accepted through CLI arguments.
/// The source workspace configuration is changed only after the destination head and format
/// have been authenticated and verified.
pub async fn transfer_hub(
    source_workspace: &Path,
    source_url: &str,
    destination_workspace: &Path,
) -> Result<HubTransferResult> {
    let source_workspace = source_workspace
        .canonicalize()
        .context("canonicalize source workspace")?;
    let destination_workspace = destination_workspace
        .canonicalize()
        .context("canonicalize destination workspace")?;
    if source_workspace == destination_workspace {
        bail!("source and destination workspace folders must be different");
    }

    let _sync_guard = SyncLock::acquire(&source_workspace)?;
    let source_config = load_config(&source_workspace)?;
    if source_config.format_version != 3 {
        bail!("hub transfer requires a format-v3 workspace; run `feanorfs migrate` first");
    }
    if source_config.encryption_password.is_none() {
        bail!("hub transfer requires the source workspace E2EE key");
    }

    let normalized_source_url = validate_source_url(source_url)?;
    let mut source_transport = source_config.clone();
    source_transport.server_url = normalized_source_url;
    source_transport.hub_local = false;
    source_transport.relay = None;
    if source_transport.server_url.starts_with("http://") {
        source_transport.tls_ca_pem = None;
    }
    let source_api = ApiClient::from_config_direct(&source_workspace, &source_transport)
        .await
        .context("open source hub connection")?;

    let destination_config = load_config(&destination_workspace)?;
    if destination_config.is_local_hub() {
        bail!("hub transfer destination must be a network hub workspace");
    }
    let destination_api = open_api_client(&destination_workspace, &destination_config)
        .await
        .context("open authenticated destination hub connection")?;

    let source_db = open_client_db(&source_workspace).await?;
    let source_ctx =
        SyncCtx::from_config(&source_api, &source_db, &source_workspace, &source_config)?;
    let destination_ctx = SyncCtx::from_config(
        &destination_api,
        &source_db,
        &source_workspace,
        &source_config,
    )?;
    let copied =
        transfer_snapshot_history(&source_api, &destination_api, &source_ctx, &destination_ctx)
            .await?;
    let local_objects_seeded = seed_local_file_objects(&destination_api, &source_ctx).await?;

    // Endpoint selection may have safely refreshed the destination workspace URL. Reload its
    // authenticated configuration before copying only the connection fields.
    let destination_config = load_config(&destination_workspace)?;
    let mut updated = source_config;
    updated.server_url = destination_config.server_url.clone();
    updated.server_password = destination_config.server_password;
    updated.tls_ca_pem = destination_config.tls_ca_pem;
    updated.relay = destination_config.relay;
    updated.hub_local = false;
    save_config_secure(&source_workspace, &updated)
        .context("save verified destination connection securely")?;

    Ok(HubTransferResult {
        workspace_id: updated.workspace_id,
        snapshots: copied.manifests.len(),
        objects: copied.hashes.len(),
        local_objects_seeded,
        destination_url: updated.server_url,
        format_version: updated.format_version,
        configuration_updated: true,
    })
}

async fn seed_local_file_objects(destination_api: &ApiClient, ctx: &SyncCtx<'_>) -> Result<usize> {
    let mut seeded_paths = BTreeSet::new();
    for _ in 0..3 {
        let files = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
        let mut paths: Vec<_> = files.keys().cloned().collect();
        paths.sort();
        let mut changed_during_pass = false;
        for path in &paths {
            let state = &files[path];
            let content = match tokio::fs::read(ctx.base.join(path)).await {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    changed_during_pass = true;
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("read current local file {path}"))
                }
            };
            let (hash, ciphertext) =
                feanorfs_agent_core::crypto::seal(&content, ctx.password_str(), path)?;
            changed_during_pass |= hash != state.hash;
            destination_api
                .upload_object(ctx.workspace_id(), &hash, ciphertext)
                .await
                .with_context(|| {
                    format!("seed destination object for current local file {path}")
                })?;
            seeded_paths.insert(path.clone());
        }
        if !changed_during_pass {
            return Ok(seeded_paths.len());
        }
    }
    bail!("local files kept changing while preparing transfer; stop writers and retry")
}

async fn transfer_snapshot_history(
    source_api: &ApiClient,
    destination_api: &ApiClient,
    source_ctx: &SyncCtx<'_>,
    destination_ctx: &SyncCtx<'_>,
) -> Result<ReachableHistory> {
    let workspace_id = source_ctx.workspace_id();
    let source_head = source_api
        .get_head(workspace_id)
        .await?
        .context("source hub has no format-v3 snapshot head")?;
    let destination_head = destination_api.get_head(workspace_id).await?;
    let history = collect_reachable_history(source_ctx, &source_head).await?;
    if let Some(destination_head) = destination_head
        .as_deref()
        .filter(|head| *head != source_head)
    {
        if !snapshot_descends_from(destination_ctx, destination_head, &source_head).await? {
            bail!(
                "destination already contains a different encrypted history for workspace {workspace_id}; no changes were made"
            );
        }
        verify_destination_head(destination_api, workspace_id, destination_head).await?;
        return Ok(history);
    }
    for hash in &history.hashes {
        let ciphertext = source_api
            .download_file(hash)
            .await
            .with_context(|| format!("read source object {hash}"))?;
        if hash_bytes(&ciphertext) != *hash {
            bail!("source object hash mismatch for {hash}");
        }
        destination_api
            .upload_object(workspace_id, hash, ciphertext)
            .await
            .with_context(|| format!("write destination object {hash}"))?;
    }

    if source_api.get_head(workspace_id).await?.as_deref() != Some(&source_head) {
        bail!("source workspace changed during transfer; destination head was not published");
    }
    for manifest in &history.manifests {
        destination_api
            .upload_manifest(workspace_id, &manifest.snapshot_id, &manifest.hashes)
            .await
            .with_context(|| {
                format!(
                    "publish destination manifest for snapshot {}",
                    manifest.snapshot_id
                )
            })?;
    }

    match destination_api.get_head(workspace_id).await? {
        Some(head) if head == source_head => {}
        Some(_) => {
            bail!("destination head changed during transfer; source configuration was not changed")
        }
        None => match destination_api
            .swap_head(workspace_id, None, &source_head)
            .await?
        {
            SwapHeadResult::Swapped => {}
            SwapHeadResult::Conflict(_) => {
                bail!(
                    "destination head changed during transfer; source configuration was not changed"
                )
            }
        },
    }

    verify_destination_head(destination_api, workspace_id, &source_head).await?;

    Ok(history)
}

async fn verify_destination_head(
    destination_api: &ApiClient,
    workspace_id: &str,
    expected_head: &str,
) -> Result<()> {
    destination_api
        .set_workspace_format(workspace_id, 3)
        .await?;
    if destination_api.workspace_format(workspace_id).await? != 3
        || destination_api.get_head(workspace_id).await?.as_deref() != Some(expected_head)
    {
        bail!("destination verification failed; source configuration was not changed");
    }
    let destination_head_bytes = destination_api.download_file(expected_head).await?;
    if hash_bytes(&destination_head_bytes) != expected_head {
        bail!("destination head object failed ciphertext verification");
    }
    Ok(())
}

async fn snapshot_descends_from(ctx: &SyncCtx<'_>, head: &str, ancestor: &str) -> Result<bool> {
    let objects = ObjectStore::new(ctx);
    let mut pending = vec![head.to_string()];
    let mut visited = BTreeSet::new();
    while let Some(snapshot_id) = pending.pop() {
        if snapshot_id == ancestor {
            return Ok(true);
        }
        if !visited.insert(snapshot_id.clone()) {
            continue;
        }
        let snapshot = objects.get_snapshot(&snapshot_id).await?;
        pending.extend(snapshot.parents);
    }
    Ok(false)
}

async fn collect_reachable_history(ctx: &SyncCtx<'_>, head: &str) -> Result<ReachableHistory> {
    if !is_valid_hash(head) {
        bail!("source hub returned an invalid snapshot head");
    }
    let objects = ObjectStore::new(ctx);
    let mut pending_snapshots = vec![head.to_string()];
    let mut visited_snapshots = BTreeSet::new();
    let mut manifests = Vec::new();
    let mut all_hashes = BTreeSet::new();

    while let Some(snapshot_id) = pending_snapshots.pop() {
        if !visited_snapshots.insert(snapshot_id.clone()) {
            continue;
        }
        let snapshot = objects.get_snapshot(&snapshot_id).await?;
        for parent in &snapshot.parents {
            if !is_valid_hash(parent) {
                bail!("snapshot {snapshot_id} contains an invalid parent id");
            }
            pending_snapshots.push(parent.clone());
        }

        let mut hashes = BTreeSet::from([snapshot_id.clone()]);
        let mut pending_trees = vec![snapshot.root];
        while let Some(tree_id) = pending_trees.pop() {
            if !is_valid_hash(&tree_id) {
                bail!("snapshot {snapshot_id} contains an invalid tree id");
            }
            if !hashes.insert(tree_id.clone()) {
                continue;
            }
            for entry in objects.get_tree(&tree_id).await?.entries {
                match entry.kind {
                    TreeEntryKind::Dir => pending_trees.push(entry.hash),
                    TreeEntryKind::File => {
                        hashes.insert(entry.hash);
                    }
                    TreeEntryKind::Conflict { base, ours, theirs } => {
                        hashes.insert(entry.hash);
                        hashes.extend(base);
                        hashes.extend(ours);
                        hashes.extend(theirs);
                    }
                }
            }
        }
        if hashes.iter().any(|hash| !is_valid_hash(hash)) {
            bail!("snapshot {snapshot_id} contains an invalid object id");
        }
        all_hashes.extend(hashes.iter().cloned());
        manifests.push(SnapshotManifest {
            snapshot_id,
            hashes: hashes.into_iter().collect(),
        });
    }
    manifests.sort_by(|left, right| left.snapshot_id.cmp(&right.snapshot_id));

    Ok(ReachableHistory {
        manifests,
        hashes: all_hashes,
    })
}

fn validate_source_url(source_url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(source_url).context("parse source hub URL")?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let loopback = parsed.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            });
            if !loopback {
                bail!("plaintext source hubs are allowed only on loopback");
            }
        }
        _ => bail!("source hub URL must use https:// or loopback http://"),
    }
    Ok(source_url.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_agent_core::{ClientDb, LocalHub};
    use feanorfs_common::{Snapshot, Tree, TreeEntry};

    #[test]
    fn plaintext_source_must_be_loopback() {
        assert!(validate_source_url("http://127.0.0.1:3031").is_ok());
        assert!(validate_source_url("http://localhost:3031/").is_ok());
        assert!(validate_source_url("http://192.168.1.13:3031").is_err());
        assert!(validate_source_url("https://hub.example").is_ok());
    }

    #[tokio::test]
    async fn transfers_complete_parent_history_and_stamps_format() {
        let source_root = tempfile::tempdir().unwrap();
        let source_hub_dir = tempfile::tempdir().unwrap();
        let destination_hub_dir = tempfile::tempdir().unwrap();
        let source_hub = LocalHub::open(source_hub_dir.path().to_path_buf(), None)
            .await
            .unwrap();
        let destination_hub = LocalHub::open(destination_hub_dir.path().to_path_buf(), None)
            .await
            .unwrap();
        let source_api = ApiClient::local(source_hub, None);
        let destination_api = ApiClient::local(destination_hub, None);
        let db = ClientDb::new(source_root.path().join(".feanorfs"))
            .await
            .unwrap();
        let config = crate::Config {
            server_url: "http://127.0.0.1:1".into(),
            workspace_id: "workspace-transfer-test".into(),
            encryption_password: Some("11".repeat(32)),
            server_password: None,
            tls_ca_pem: None,
            format_version: 3,
            hub_local: false,
            relay: None,
        };
        let ctx = SyncCtx::from_config(&source_api, &db, source_root.path(), &config).unwrap();
        let objects = ObjectStore::new(&ctx);

        let first_bytes = b"opaque-file-one".to_vec();
        let first_hash = hash_bytes(&first_bytes);
        source_api
            .upload_object(&config.workspace_id, &first_hash, first_bytes)
            .await
            .unwrap();
        let first_tree = objects
            .put_tree(&Tree {
                entries: vec![TreeEntry {
                    name: "one".into(),
                    kind: TreeEntryKind::File,
                    hash: first_hash.clone(),
                    size: 15,
                    mode: 0,
                }],
            })
            .await
            .unwrap();
        let first_snapshot = objects
            .put_snapshot(&Snapshot {
                root: first_tree.clone(),
                parents: vec![],
                author: "test".into(),
                created_at_ms: 1,
                message: None,
            })
            .await
            .unwrap();
        source_api
            .upload_manifest(
                &config.workspace_id,
                &first_snapshot,
                &[first_hash, first_tree, first_snapshot.clone()],
            )
            .await
            .unwrap();

        let second_bytes = b"opaque-file-two".to_vec();
        let second_hash = hash_bytes(&second_bytes);
        source_api
            .upload_object(&config.workspace_id, &second_hash, second_bytes)
            .await
            .unwrap();
        let second_tree = objects
            .put_tree(&Tree {
                entries: vec![TreeEntry {
                    name: "two".into(),
                    kind: TreeEntryKind::File,
                    hash: second_hash.clone(),
                    size: 15,
                    mode: 0,
                }],
            })
            .await
            .unwrap();
        let second_snapshot = objects
            .put_snapshot(&Snapshot {
                root: second_tree.clone(),
                parents: vec![first_snapshot.clone()],
                author: "test".into(),
                created_at_ms: 2,
                message: None,
            })
            .await
            .unwrap();
        source_api
            .upload_manifest(
                &config.workspace_id,
                &second_snapshot,
                &[
                    second_hash.clone(),
                    second_tree.clone(),
                    second_snapshot.clone(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            source_api
                .swap_head(&config.workspace_id, None, &second_snapshot)
                .await
                .unwrap(),
            SwapHeadResult::Swapped
        );
        source_api
            .set_workspace_format(&config.workspace_id, 3)
            .await
            .unwrap();

        let destination_ctx =
            SyncCtx::from_config(&destination_api, &db, source_root.path(), &config).unwrap();
        let copied =
            transfer_snapshot_history(&source_api, &destination_api, &ctx, &destination_ctx)
                .await
                .unwrap();
        assert_eq!(copied.manifests.len(), 2);
        assert_eq!(
            destination_api
                .workspace_format(&config.workspace_id)
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            destination_api
                .get_head(&config.workspace_id)
                .await
                .unwrap(),
            Some(second_snapshot.clone())
        );
        for hash in copied.hashes {
            let bytes = destination_api.download_file(&hash).await.unwrap();
            assert_eq!(hash_bytes(&bytes), hash);
        }

        let destination_objects = ObjectStore::new(&destination_ctx);
        let descendant = destination_objects
            .put_snapshot(&Snapshot {
                root: second_tree.clone(),
                parents: vec![second_snapshot.clone()],
                author: "destination".into(),
                created_at_ms: 3,
                message: Some("advanced after transfer".into()),
            })
            .await
            .unwrap();
        destination_api
            .upload_manifest(
                &config.workspace_id,
                &descendant,
                &[second_hash, second_tree, descendant.clone()],
            )
            .await
            .unwrap();
        assert_eq!(
            destination_api
                .swap_head(&config.workspace_id, Some(&second_snapshot), &descendant)
                .await
                .unwrap(),
            SwapHeadResult::Swapped
        );
        transfer_snapshot_history(&source_api, &destination_api, &ctx, &destination_ctx)
            .await
            .unwrap();
        assert_eq!(
            destination_api
                .get_head(&config.workspace_id)
                .await
                .unwrap(),
            Some(descendant)
        );
    }
}
