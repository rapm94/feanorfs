use std::fs;

use crate::durable::{atomic_overwrite, set_atomic_faults, AtomicFaults};

#[test]
fn atomic_overwrite_happy_path_succeeds() {
    let dir = tempfile::tempdir().expect("create temp dir");
    set_atomic_faults(AtomicFaults::default());
    let path = dir.path().join("test.json");
    fs::write(&path, b"original").expect("seed file");

    atomic_overwrite(&path, b"replacement").expect("atomic overwrite should succeed");

    assert_eq!(fs::read_to_string(&path).expect("read back"), "replacement");
}

#[test]
fn atomic_overwrite_pre_commit_fault_preserves_old_bytes() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("test.json");
    fs::write(&path, b"original").expect("seed file");
    set_atomic_faults(AtomicFaults {
        fail_before_commit: true,
        fail_after_commit: false,
    });

    let error = atomic_overwrite(&path, b"replacement").expect_err("pre-commit fault must fail");

    assert!(error.to_string().contains("injected pre-commit fault"));
    assert_eq!(fs::read_to_string(&path).expect("read back"), "original");
    set_atomic_faults(AtomicFaults::default());
}

#[test]
fn atomic_overwrite_post_commit_fault_reports_uncertain_with_valid_new_bytes() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("test.json");
    fs::write(&path, b"original").expect("seed file");
    set_atomic_faults(AtomicFaults {
        fail_before_commit: false,
        fail_after_commit: true,
    });

    let error = atomic_overwrite(&path, b"replacement").expect_err("post-commit fault must fail");

    assert!(error
        .to_string()
        .contains("committed-but-durability-uncertain"));
    assert_eq!(fs::read_to_string(&path).expect("read back"), "replacement");
    set_atomic_faults(AtomicFaults::default());
}
