use std::fs;

use super::{cache_entry, ClientDb};

#[tokio::test]
async fn independent_handles_observe_each_others_writes() {
    let dir = tempfile::tempdir().expect("create dir");
    let first = ClientDb::new(dir.path()).await.expect("open first handle");
    let second = ClientDb::new(dir.path()).await.expect("open second handle");

    first
        .upsert_cache_entry(&cache_entry("from-first.txt", "first", 1))
        .await
        .expect("first write");
    assert!(second
        .get_cache_entries()
        .await
        .expect("second read")
        .contains_key("from-first.txt"));

    second
        .upsert_cache_entry(&cache_entry("from-second.txt", "second", 2))
        .await
        .expect("second write");
    let entries = first
        .get_cache_entries()
        .await
        .expect("first read after second write");
    assert!(entries.contains_key("from-first.txt"));
    assert!(entries.contains_key("from-second.txt"));
}

#[tokio::test]
async fn disjoint_updates_from_two_handles_survive() {
    let dir = tempfile::tempdir().expect("create dir");
    let first = ClientDb::new(dir.path()).await.expect("open first handle");
    let second = ClientDb::new(dir.path()).await.expect("open second handle");

    first
        .upsert_cache_entry(&cache_entry("shared.txt", "base", 0))
        .await
        .expect("seed");
    first
        .upsert_cache_entry(&cache_entry("only-first.txt", "first", 1))
        .await
        .expect("first write");
    second
        .upsert_cache_entry(&cache_entry("only-second.txt", "second", 2))
        .await
        .expect("second write");

    let entries = first.get_cache_entries().await.expect("read all");
    assert_eq!(entries.len(), 3);
    assert!(entries.contains_key("shared.txt"));
    assert!(entries.contains_key("only-first.txt"));
    assert!(entries.contains_key("only-second.txt"));
}

#[tokio::test]
async fn malformed_state_is_rejected_without_mutation() {
    let dir = tempfile::tempdir().expect("create dir");
    fs::write(dir.path().join("local_state.json"), b"not json").expect("write malformed state");
    fs::write(dir.path().join("local_state.lock"), b"").expect("create lock");

    let err = ClientDb::new(dir.path())
        .await
        .expect_err("malformed state should fail");
    assert!(err.to_string().contains("parse local state JSON"));
    assert_eq!(
        fs::read(dir.path().join("local_state.json")).expect("read malformed state"),
        b"not json"
    );
}

#[tokio::test]
async fn malformed_state_is_rejected_on_read() {
    let dir = tempfile::tempdir().expect("create dir");
    let db = ClientDb::new(dir.path()).await.expect("open state");
    fs::write(dir.path().join("local_state.json"), b"not json")
        .expect("replace with malformed state");

    let err = db
        .get_cache_entries()
        .await
        .expect_err("malformed state should fail on read");
    assert!(err.to_string().contains("parse local state JSON"));
}

#[tokio::test]
async fn future_schema_is_rejected_on_open() {
    let dir = tempfile::tempdir().expect("create dir");
    let future_state = r#"{"schema_version": 99, "local_files": {}}"#;
    fs::write(dir.path().join("local_state.json"), future_state).expect("write future state");
    fs::write(dir.path().join("local_state.lock"), b"").expect("create lock");
    let err = ClientDb::new(dir.path())
        .await
        .expect_err("future schema should fail on open");
    assert!(err.to_string().contains("newer than supported"));
}

#[tokio::test]
async fn future_schema_is_rejected_on_read() {
    let dir = tempfile::tempdir().expect("create dir");
    let db = ClientDb::new(dir.path()).await.expect("open state");
    fs::write(
        dir.path().join("local_state.json"),
        r#"{"schema_version": 99, "local_files": {}}"#,
    )
    .expect("replace with future state");
    let err = db
        .get_cache_entries()
        .await
        .expect_err("future schema should fail on read");
    assert!(err.to_string().contains("newer than supported"));
}

#[tokio::test]
async fn legacy_database_requires_migration() {
    let dir = tempfile::tempdir().expect("create dir");
    fs::write(dir.path().join("local_cache.db"), b"legacy sqlite").expect("write legacy database");

    let err = ClientDb::new(dir.path())
        .await
        .expect_err("unmigrated workspace should fail");
    assert!(err.to_string().contains("feanorfs migrate"));
}

#[tokio::test]
async fn state_roundtrip_serialization_is_deterministic() {
    let dir = tempfile::tempdir().expect("create dir");
    let db = ClientDb::new(dir.path()).await.expect("open state");
    db.upsert_cache_entry(&cache_entry("a.txt", "a", 10))
        .await
        .expect("upsert a");
    db.upsert_cache_entry(&cache_entry("b.txt", "b", 20))
        .await
        .expect("upsert b");
    db.set_session_key("k1", "v1")
        .await
        .expect("set session key");
    db.record_access_pair("a.txt", "b.txt", 1.0)
        .await
        .expect("record access");

    let first =
        fs::read_to_string(dir.path().join("local_state.json")).expect("read persisted state");
    let state = crate::state::LocalStateV1::from_json(&first).expect("parse persisted state");
    let second = state.to_json().expect("serialize state");
    assert_eq!(first, second);
}
