//! Durable per-workspace registry of encrypted object hashes the hub has
//! accepted. Lets `put_bytes` and chunked uploads skip blobs the hub already
//! stores instead of re-uploading byte-identical ciphertext on every sync.
//!
//! Safety: entries are only ever added from reachability manifests the hub
//! accepted (the hub validates every referenced blob exists at accept time),
//! and objects reachable from the current head are always retained by hub GC
//! because head manifests are never expired. A rejected manifest clears the
//! registry so the next pass re-uploads instead of skipping forever.

use anyhow::{Context, Result};
use feanorfs_common::{is_valid_hash, MANIFEST_MAX_ENTRIES};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokio::fs;

/// Workspace-state file holding one accepted object hash per line.
const REGISTRY_FILE: &str = "uploaded-objects";

static REGISTRIES: OnceLock<Mutex<HashMap<PathBuf, HashSet<String>>>> = OnceLock::new();

fn registries() -> &'static Mutex<HashMap<PathBuf, HashSet<String>>> {
    REGISTRIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns true when the hub is known to already store `hash` for this
/// workspace. Read or parse failures fail open (treat as unknown) so the safe
/// default is to upload.
pub(crate) async fn known(state_dir: &Path, hash: &str) -> bool {
    let key = state_dir.to_path_buf();
    {
        let map = registries().lock().expect("upload registry poisoned");
        if let Some(set) = map.get(&key) {
            return set.contains(hash);
        }
    }
    match read_registry_file(&state_dir.join(REGISTRY_FILE)).await {
        Ok(set) => {
            let contains = set.contains(hash);
            registries()
                .lock()
                .expect("upload registry poisoned")
                .entry(key)
                .or_insert(set);
            contains
        }
        Err(error) => {
            tracing::warn!("upload registry unreadable; uploading anyway: {error:#}");
            false
        }
    }
}

/// Records one accepted upload in memory. Persistence happens through
/// [`record_many`] once the covering reachability manifest is accepted, so a
/// crash before manifest acceptance only costs a redundant upload later.
pub(crate) fn remember(state_dir: &Path, hash: &str) {
    if !is_valid_hash(hash) {
        return;
    }
    let key = state_dir.to_path_buf();
    let mut registries = registries().lock().expect("upload registry poisoned");
    let set = registries.entry(key).or_default();
    if set.len() < MANIFEST_MAX_ENTRIES || set.contains(hash) {
        set.insert(hash.to_string());
    }
}

/// Persists the latest accepted bounded reachability closure for this
/// workspace. Failures are non-fatal: the registry is a cache, and a missing
/// entry only causes a redundant upload later.
pub(crate) async fn record_many(state_dir: &Path, hashes: &[String]) -> Result<()> {
    anyhow::ensure!(
        hashes.len() <= MANIFEST_MAX_ENTRIES,
        "upload registry input exceeds manifest object limit"
    );
    let replacement = hashes
        .iter()
        .filter(|hash| is_valid_hash(hash))
        .cloned()
        .collect::<HashSet<_>>();
    let key = state_dir.to_path_buf();
    let changed = {
        let mut map = registries().lock().expect("upload registry poisoned");
        map.get(&key) != Some(&replacement) && {
            map.insert(key, replacement);
            true
        }
    };
    if changed {
        if let Err(error) = persist(state_dir).await {
            tracing::warn!("failed to persist upload registry: {error:#}");
        }
    }
    Ok(())
}

/// Drops all knowledge for this workspace and deletes its registry file.
/// Used after a hub rejects a manifest so the next pass re-uploads.
pub(crate) async fn clear(state_dir: &Path) -> Result<()> {
    let key = state_dir.to_path_buf();
    registries()
        .lock()
        .expect("upload registry poisoned")
        .remove(&key);
    let path = state_dir.join(REGISTRY_FILE);
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("clear upload registry {}", path.display()))
        }
    }
}

async fn persist(state_dir: &Path) -> Result<()> {
    let key = state_dir.to_path_buf();
    let mut hashes: Vec<String> = {
        let map = registries().lock().expect("upload registry poisoned");
        map.get(&key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect()
    };
    hashes.sort_unstable();
    let mut content = hashes.join("\n").into_bytes();
    content.push(b'\n');
    crate::fs_util::atomic_write_visible(state_dir, REGISTRY_FILE, &content).await
}

async fn read_registry_file(path: &Path) -> Result<HashSet<String>> {
    use tokio::io::AsyncReadExt as _;

    const MAX_REGISTRY_BYTES: usize = MANIFEST_MAX_ENTRIES * 65;
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("read upload registry {}", path.display()))
        }
    };
    anyhow::ensure!(
        metadata.len() <= MAX_REGISTRY_BYTES as u64,
        "upload registry exceeds bounded size"
    );
    let file = fs::File::open(path).await?;
    let mut content = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_REGISTRY_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut content)
        .await?;
    anyhow::ensure!(
        content.len() <= MAX_REGISTRY_BYTES,
        "upload registry exceeds bounded size"
    );
    let content = std::str::from_utf8(&content).context("upload registry is not UTF-8")?;
    let mut hashes = HashSet::new();
    for hash in content.lines() {
        if is_valid_hash(hash) {
            anyhow::ensure!(
                hashes.len() < MANIFEST_MAX_ENTRIES || hashes.contains(hash),
                "upload registry exceeds manifest object limit"
            );
            hashes.insert(hash.to_string());
        }
    }
    Ok(hashes)
}
