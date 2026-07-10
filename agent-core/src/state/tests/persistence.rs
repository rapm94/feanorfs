use std::fs;
use std::sync::{Arc, Barrier};

use super::super::{check_no_legacy_db, DurableState, LocalStateV1, CURRENT_SCHEMA_VERSION};
use super::cache_entry;

#[test]
fn legacy_db_guard_blocks_when_db_exists_without_json() {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::write(dir.path().join("local_cache.db"), b"legacy").expect("write legacy db");

    let error = check_no_legacy_db(dir.path()).expect_err("legacy database should block");

    assert!(error.to_string().contains("feanorfs migrate"));
}

#[test]
fn legacy_db_guard_rejects_when_db_exists_with_json() {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::write(dir.path().join("local_cache.db"), b"legacy").expect("write legacy db");
    fs::write(dir.path().join("local_state.json"), b"{}").expect("write state");

    let error = check_no_legacy_db(dir.path()).expect_err("legacy database should block");

    assert!(error.to_string().contains("feanorfs migrate"));
}

#[test]
fn legacy_db_guard_allows_when_neither_exist() {
    let dir = tempfile::tempdir().expect("create temp dir");
    assert!(check_no_legacy_db(dir.path()).is_ok());
}

#[test]
fn durable_state_open_initializes_empty() {
    let dir = tempfile::tempdir().expect("create temp dir");

    let state_store = DurableState::new(dir.path()).expect("open durable state");

    assert!(state_store.state_path().exists());
    assert!(state_store.lock_path().exists());
    let content = fs::read_to_string(state_store.state_path()).expect("read state");
    let state = LocalStateV1::from_json(&content).expect("parse state");
    assert_eq!(state.schema_version, CURRENT_SCHEMA_VERSION);
    assert!(state.local_files.is_empty());
}

#[test]
fn durable_state_reopen_preserves_data() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let first = DurableState::new(dir.path()).expect("open durable state");
    first
        .with_write(|state| {
            state
                .local_files
                .insert("a.txt".into(), cache_entry("a", 42));
            Ok(())
        })
        .expect("write entry");

    let second = DurableState::new(dir.path()).expect("reopen durable state");
    let entries = second
        .with_read(|state| Ok(state.local_files.clone()))
        .expect("read back");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries.get("a.txt").expect("entry").size, 42);
}

#[test]
fn durable_state_read_sees_latest_commit() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state_store = DurableState::new(dir.path()).expect("open durable state");
    for (path, marker, size) in [("a", "a", 1), ("b", "b", 2)] {
        state_store
            .with_write(|state| {
                state
                    .local_files
                    .insert(path.into(), cache_entry(marker, size));
                Ok(())
            })
            .expect("write entry");
    }

    let entries = state_store
        .with_read(|state| Ok(state.local_files.clone()))
        .expect("read entries");

    assert_eq!(entries.len(), 2);
}

#[test]
fn durable_state_new_validates_malformed_state() {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::write(dir.path().join("local_state.json"), b"not json").expect("write malformed");
    fs::write(dir.path().join("local_state.lock"), b"").expect("create lock");

    let error = DurableState::new(dir.path()).expect_err("malformed state should fail");

    assert!(error.to_string().contains("parse local state JSON"));
}

#[test]
fn durable_state_new_rejects_future_schema() {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::write(
        dir.path().join("local_state.json"),
        r#"{"schema_version": 99, "local_files": {}}"#,
    )
    .expect("write future state");
    fs::write(dir.path().join("local_state.lock"), b"").expect("create lock");

    let error = DurableState::new(dir.path()).expect_err("future schema should fail");

    assert!(error.to_string().contains("newer than supported"));
}

#[test]
fn durable_state_concurrent_first_opens_preserve_data() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let directory = dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let written = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let first_barrier = Arc::clone(&barrier);
    let first_written = Arc::clone(&written);
    let first_directory = directory.clone();
    let first = std::thread::spawn(move || {
        let state_store = DurableState::new(&first_directory).expect("open first");
        state_store
            .with_write(|state| {
                state
                    .local_files
                    .insert("thread1.txt".into(), cache_entry("thread1", 1));
                Ok(())
            })
            .expect("write first entry");
        first_written.store(true, std::sync::atomic::Ordering::SeqCst);
        first_barrier.wait();
        first_barrier.wait();
        let entries = state_store
            .with_read(|state| Ok(state.local_files.clone()))
            .expect("read after second open");
        assert!(entries.contains_key("thread1.txt"));
    });

    let second_barrier = Arc::clone(&barrier);
    let second = std::thread::spawn(move || {
        while !written.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::yield_now();
        }
        second_barrier.wait();
        let state_store = DurableState::new(&directory).expect("open second");
        second_barrier.wait();
        let entries = state_store
            .with_read(|state| Ok(state.local_files.clone()))
            .expect("read from second");
        assert!(entries.contains_key("thread1.txt"));
    });

    first.join().expect("first thread panicked");
    second.join().expect("second thread panicked");
}

#[test]
fn durable_state_read_fails_on_missing_state_file() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state_store = DurableState::new(dir.path()).expect("open durable state");
    fs::remove_file(state_store.state_path()).expect("delete state file");

    let error = state_store
        .with_read(|_| Ok(()))
        .expect_err("missing state should fail");

    assert!(error.to_string().contains("local_state.json is missing"));
}
