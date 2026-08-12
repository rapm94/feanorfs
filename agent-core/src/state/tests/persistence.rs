use std::fs;
use std::sync::{Arc, Barrier};

use super::super::{
    check_no_legacy_db, DurableState, LocalStateV1, CURRENT_SCHEMA_VERSION, MAX_LOCAL_STATE_BYTES,
};
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
fn durable_state_streaming_bytes_match_canonical_serialization() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state_store = DurableState::new(dir.path()).expect("open durable state");
    state_store
        .with_write(|state| {
            state
                .local_files
                .insert("a.txt".into(), cache_entry("a", 42));
            Ok(())
        })
        .expect("write entry");

    let expected = state_store
        .with_read(LocalStateV1::to_json)
        .expect("serialize canonical state");
    let persisted = fs::read_to_string(state_store.state_path()).expect("read persisted state");

    assert_eq!(persisted, expected);
}

#[test]
fn durable_state_streaming_overflow_preserves_destination_and_temp_cleanup() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state_store = DurableState::new(dir.path()).expect("open durable state");
    let destination = state_store.state_path();
    let original = fs::read(destination).expect("read initial state");
    let temporary_files_before = temporary_state_files(dir.path());

    let error = state_store
        .with_write_limit_for_test(16, |state| {
            state
                .local_files
                .insert("a.txt".into(), cache_entry("a", 42));
            Ok(())
        })
        .expect_err("bounded streaming write should fail");

    assert!(error.to_string().contains("exceeds"));
    assert_eq!(fs::read(destination).expect("read destination"), original);
    assert_eq!(temporary_state_files(dir.path()), temporary_files_before);
}

#[test]
#[ignore = "manual 100k-entry streaming local-state persistence profile"]
fn local_state_persistence_profile_100k() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state_store = DurableState::new(dir.path()).expect("open durable state");

    let started = std::time::Instant::now();
    state_store
        .with_write(|state| {
            for index in 0..100_000 {
                let path = format!("src/file_{index:06}.txt");
                state
                    .local_files
                    .insert(path, cache_entry(&format!("entry-{index:06}"), index));
            }
            Ok(())
        })
        .expect("persist large local state");
    let elapsed = started.elapsed();
    let bytes = fs::metadata(state_store.state_path())
        .expect("inspect persisted state")
        .len();

    assert!(bytes > 10_000_000);
    eprintln!("local_state_persistence_profile_100k: elapsed={elapsed:.2?} bytes={bytes}");
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
fn durable_state_rejects_oversized_json_before_reading_it() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let file = fs::File::create(dir.path().join("local_state.json")).expect("create sparse state");
    file.set_len((MAX_LOCAL_STATE_BYTES + 1) as u64)
        .expect("size sparse state");
    fs::write(dir.path().join("local_state.lock"), b"").expect("create lock");

    let error = DurableState::new(dir.path()).expect_err("oversized state should fail");
    assert!(error.to_string().contains("exceeds"));
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

fn temporary_state_files(directory: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut files = fs::read_dir(directory)
        .expect("read state directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with(".local_state.json."))
        .collect::<Vec<_>>();
    files.sort();
    files
}
