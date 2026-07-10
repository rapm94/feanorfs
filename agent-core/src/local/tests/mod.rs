mod access;
mod cache;
mod conflicts;
mod durability;
mod scanning;

use super::{CacheEntry, ClientDb};

async fn new_db() -> (tempfile::TempDir, ClientDb) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = ClientDb::new(dir.path()).await.expect("create ClientDb");
    (dir, db)
}

fn cache_entry(path: &str, marker: &str, size: u64) -> CacheEntry {
    CacheEntry {
        path: path.into(),
        plaintext_hash: format!("ph-{marker}"),
        encrypted_hash: format!("eh-{marker}"),
        size,
        mtime: i64::try_from(size).unwrap_or(i64::MAX),
        server_mtime: i64::try_from(size).unwrap_or(i64::MAX),
        mode: 0,
        hydrated: true,
        deleted_at: None,
    }
}
