mod atomic;
mod model;
mod persistence;

use super::{CacheEntryV1, LocalStateV1};

fn empty_state() -> LocalStateV1 {
    LocalStateV1::default()
}

fn cache_entry(marker: &str, size: i64) -> CacheEntryV1 {
    CacheEntryV1 {
        plaintext_hash: format!("plaintext-{marker}"),
        encrypted_hash: format!("encrypted-{marker}"),
        size,
        mtime: size,
        server_mtime: size,
        mode: 0,
        hydrated: true,
        deleted_at: None,
    }
}
