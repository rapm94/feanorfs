use super::new_db;
use crate::state::{ConflictRecordStatus, ResolutionMethod};

#[tokio::test]
async fn conflict_registry_crud() {
    let (_dir, db) = new_db().await;
    db.upsert_conflict(
        "src/lib.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        "/tmp/conflict1",
        100,
        ConflictRecordStatus::Pending,
    )
    .await
    .expect("upsert");
    assert_eq!(
        db.list_pending_conflict_paths().await.expect("list"),
        vec!["src/lib.rs"]
    );
    let records = db.list_conflict_records().await.expect("list records");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, feanorfs_common::ConflictKind::EditEdit);
    assert_eq!(
        db.get_conflict_record("src/lib.rs")
            .await
            .expect("get")
            .expect("present")
            .conflict_dir,
        "/tmp/conflict1"
    );
    assert_eq!(
        db.count_pending_in_dir("/tmp/conflict1")
            .await
            .expect("count"),
        1
    );
    db.resolve_conflict_path("src/lib.rs")
        .await
        .expect("resolve");
    assert!(db
        .get_conflict_record("src/lib.rs")
        .await
        .expect("get resolved")
        .is_none());
}

#[tokio::test]
async fn conflict_count_per_directory() {
    let (_dir, db) = new_db().await;
    for index in 0..5 {
        db.upsert_conflict(
            &format!("file_{index}.txt"),
            &feanorfs_common::ConflictKind::EditEdit,
            "/tmp/dir-a",
            100 + index,
            ConflictRecordStatus::Pending,
        )
        .await
        .expect("upsert");
    }
    for index in 5..8 {
        db.upsert_conflict(
            &format!("file_{index}.txt"),
            &feanorfs_common::ConflictKind::EditDelete,
            "/tmp/dir-b",
            100 + index,
            ConflictRecordStatus::LegacyUnfingerprinted,
        )
        .await
        .expect("upsert");
    }
    assert_eq!(db.count_pending_in_dir("/tmp/dir-a").await.expect("a"), 5);
    assert_eq!(db.count_pending_in_dir("/tmp/dir-b").await.expect("b"), 3);
    assert_eq!(
        db.count_pending_in_dir("/tmp/nonexistent")
            .await
            .expect("missing"),
        0
    );
}

#[tokio::test]
async fn conflict_resolution_history() {
    let (_dir, db) = new_db().await;
    db.record_conflict_resolution("a.txt", ResolutionMethod::Local, Some("hash1"), "human")
        .await
        .expect("record");
    db.record_conflict_resolution("b.txt", ResolutionMethod::Cloud, None, "agent")
        .await
        .expect("record");
    let history = db.list_conflict_resolutions().await.expect("list");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].path, "b.txt");
    assert_eq!(history[1].path, "a.txt");
    assert_eq!(history[0].method, "cloud");
    assert_eq!(history[1].resolver, "human");
}

#[tokio::test]
async fn bulk_local_resolution_updates_registry_and_history_atomically() {
    let (_dir, db) = new_db().await;
    let paths = vec!["a.txt".to_string(), "b.txt".to_string()];
    for path in &paths {
        db.upsert_conflict(
            path,
            &feanorfs_common::ConflictKind::EditEdit,
            "/tmp/bulk",
            100,
            ConflictRecordStatus::Pending,
        )
        .await
        .expect("upsert");
    }

    db.resolve_conflict_paths_with_history(&paths, ResolutionMethod::Local, "human")
        .await
        .expect("bulk resolve");

    assert!(db
        .list_pending_conflict_paths()
        .await
        .expect("pending")
        .is_empty());
    let history = db.list_conflict_resolutions().await.expect("history");
    assert_eq!(history.len(), 2);
    assert!(history.iter().all(|entry| entry.method == "local"));
}

#[tokio::test]
async fn unknown_status_records_stay_excluded_and_loadable() {
    let (_dir, db) = new_db().await;
    db.upsert_conflict(
        "future.txt",
        &feanorfs_common::ConflictKind::EditEdit,
        "/tmp/future",
        100,
        ConflictRecordStatus::Unknown,
    )
    .await
    .expect("upsert");
    assert!(db
        .list_pending_conflict_paths()
        .await
        .expect("list")
        .is_empty());
    assert!(db
        .list_automatic_candidates()
        .await
        .expect("automatic")
        .is_empty());
    assert!(db
        .get_conflict_record("future.txt")
        .await
        .expect("get")
        .is_none());
}

/// Legacy path-only records (pending status, no identity sidecar) migrate to
/// explicit `legacy_unfingerprinted` state: displayed and manually resolvable,
/// never eligible for automatic prepare/apply.
#[tokio::test]
async fn legacy_records_migrate_to_explicit_unfingerprinted_state() {
    let (_dir, db) = new_db().await;
    let conflict_dir = _dir.path().join("conflicts/legacy");
    std::fs::create_dir_all(&conflict_dir).unwrap();
    db.upsert_conflict(
        "legacy.txt",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        100,
        ConflictRecordStatus::Pending,
    )
    .await
    .expect("upsert");

    // Read-time classification surfaces legacy_unfingerprinted before the
    // durable migration.
    let record = db
        .get_conflict_record("legacy.txt")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(record.status, "legacy_unfingerprinted");
    assert!(!db
        .is_conflict_fingerprinted("legacy.txt")
        .await
        .expect("fingerprinted"));
    assert!(db
        .list_automatic_candidates()
        .await
        .expect("automatic")
        .is_empty());

    // Durable migration rewrites the stored status.
    let migrated = db
        .migrate_legacy_conflict_statuses()
        .await
        .expect("migrate");
    assert_eq!(migrated, 1);
    let record = db
        .get_conflict_record("legacy.txt")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(record.status, "legacy_unfingerprinted");
    // Idempotent: a second migration finds nothing left.
    assert_eq!(
        db.migrate_legacy_conflict_statuses()
            .await
            .expect("migrate again"),
        0
    );

    // Manual resolution still works for the migrated record.
    db.resolve_conflict_path("legacy.txt")
        .await
        .expect("manual resolve");
    assert!(db
        .get_conflict_record("legacy.txt")
        .await
        .expect("get")
        .is_none());
}

/// Fingerprinted records (identity sidecar beside artifacts) are pending and
/// eligible for automatic prepare/apply; the sidecar binds the exact
/// identity/fingerprint.
#[tokio::test]
async fn fingerprinted_records_are_automatic_candidates() {
    let (_dir, db) = new_db().await;
    let conflict_dir = _dir.path().join("conflicts/fingerprinted");
    std::fs::create_dir_all(&conflict_dir).unwrap();
    let identity = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    let fingerprint = feanorfs_common::compute_conflict_identity_fingerprint(&identity);

    db.upsert_conflict_fingerprinted(
        "src/main.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        200,
        &identity,
        &fingerprint,
    )
    .await
    .expect("upsert fingerprinted");

    assert!(db
        .is_conflict_fingerprinted("src/main.rs")
        .await
        .expect("fingerprinted"));
    assert_eq!(
        db.list_automatic_candidates().await.expect("automatic"),
        vec!["src/main.rs"]
    );
    let record = db
        .get_conflict_record("src/main.rs")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(record.status, "pending");

    // The persisted sidecar round-trips the exact identity + fingerprint.
    let (read_identity, read_fingerprint) =
        crate::conflict_artifacts::read_identity_sidecar(&conflict_dir, &fingerprint)
            .expect("sidecar");
    assert_eq!(read_identity, identity);
    assert_eq!(read_fingerprint, fingerprint);

    // Migration leaves fingerprinted records untouched.
    assert_eq!(
        db.migrate_legacy_conflict_statuses()
            .await
            .expect("migrate"),
        0
    );
    assert!(db
        .is_conflict_fingerprinted("src/main.rs")
        .await
        .expect("still fingerprinted"));
}

/// A tampered sidecar (mutated identity) fails closed: the record is no
/// longer an automatic candidate.
#[tokio::test]
async fn tampered_identity_sidecar_is_not_an_automatic_candidate() {
    let (_dir, db) = new_db().await;
    let conflict_dir = _dir.path().join("conflicts/tampered");
    std::fs::create_dir_all(&conflict_dir).unwrap();
    let mut identity = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    let fingerprint = feanorfs_common::compute_conflict_identity_fingerprint(&identity);
    db.upsert_conflict_fingerprinted(
        "src/main.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        300,
        &identity,
        &fingerprint,
    )
    .await
    .expect("upsert fingerprinted");

    identity.path = "src/tampered.rs".into();
    let tampered = serde_json::json!({
        "schema_version": 1,
        "fingerprint": fingerprint,
        "identity": identity,
    });
    std::fs::write(
        conflict_dir.join(crate::conflict_artifacts::identity_file_name(&fingerprint)),
        serde_json::to_vec(&tampered).unwrap(),
    )
    .unwrap();

    assert!(!db
        .is_conflict_fingerprinted("src/main.rs")
        .await
        .expect("fingerprinted check"));
    assert!(db
        .list_automatic_candidates()
        .await
        .expect("automatic")
        .is_empty());
    // The pending record is still visible and manually resolvable.
    let record = db
        .get_conflict_record("src/main.rs")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(record.status, "legacy_unfingerprinted");
}

/// Two conflicts in the same directory get DISTINCT fingerprint-keyed
/// sidecars (`identity-<first-32-chars-of-fingerprint>.json`) and neither
/// overwrites the other.
#[tokio::test]
async fn conflicts_in_the_same_directory_get_distinct_fingerprint_keyed_sidecars() {
    let (_dir, db) = new_db().await;
    let conflict_dir = _dir.path().join("conflicts/shared-dir");
    std::fs::create_dir_all(&conflict_dir).unwrap();

    let mut identity_a = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_a.path = "src/a.rs".into();
    let fingerprint_a = feanorfs_common::compute_conflict_identity_fingerprint(&identity_a);
    let mut identity_b = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_b.path = "src/b.rs".into();
    let fingerprint_b = feanorfs_common::compute_conflict_identity_fingerprint(&identity_b);

    db.upsert_conflict_fingerprinted(
        "src/a.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        100,
        &identity_a,
        &fingerprint_a,
    )
    .await
    .expect("upsert a");
    db.upsert_conflict_fingerprinted(
        "src/b.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        101,
        &identity_b,
        &fingerprint_b,
    )
    .await
    .expect("upsert b");

    // Distinct sidecar files, one per fingerprint.
    let file_a = conflict_dir.join(crate::conflict_artifacts::identity_file_name(
        &fingerprint_a,
    ));
    let file_b = conflict_dir.join(crate::conflict_artifacts::identity_file_name(
        &fingerprint_b,
    ));
    assert_ne!(file_a, file_b);
    assert!(file_a.is_file(), "sidecar A must exist");
    assert!(file_b.is_file(), "sidecar B must exist");

    // Each sidecar round-trips its own conflict and nothing was clobbered.
    let (read_a, read_fp_a) =
        crate::conflict_artifacts::read_identity_sidecar(&conflict_dir, &fingerprint_a)
            .expect("sidecar a");
    let (read_b, read_fp_b) =
        crate::conflict_artifacts::read_identity_sidecar(&conflict_dir, &fingerprint_b)
            .expect("sidecar b");
    assert_eq!(read_a, identity_a);
    assert_eq!(read_fp_a, fingerprint_a);
    assert_eq!(read_b, identity_b);
    assert_eq!(read_fp_b, fingerprint_b);

    // Both records are pending and automatic candidates.
    assert!(db
        .is_conflict_fingerprinted("src/a.rs")
        .await
        .expect("a fingerprinted"));
    assert!(db
        .is_conflict_fingerprinted("src/b.rs")
        .await
        .expect("b fingerprinted"));
    assert_eq!(
        db.list_automatic_candidates().await.expect("automatic"),
        vec!["src/a.rs", "src/b.rs"]
    );
}

/// Upserting one conflict never clobbers another conflict's sidecar in the
/// same directory, and re-registering the same fingerprint is idempotent.
#[tokio::test]
async fn upsert_for_one_fingerprint_never_clobbers_another_sidecar() {
    let (_dir, db) = new_db().await;
    let conflict_dir = _dir.path().join("conflicts/clobber");
    std::fs::create_dir_all(&conflict_dir).unwrap();

    let mut identity_a = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_a.path = "src/a.rs".into();
    let fingerprint_a = feanorfs_common::compute_conflict_identity_fingerprint(&identity_a);
    let mut identity_b = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_b.path = "src/b.rs".into();
    let fingerprint_b = feanorfs_common::compute_conflict_identity_fingerprint(&identity_b);

    db.upsert_conflict_fingerprinted(
        "src/a.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        100,
        &identity_a,
        &fingerprint_a,
    )
    .await
    .expect("upsert a");
    let file_a = conflict_dir.join(crate::conflict_artifacts::identity_file_name(
        &fingerprint_a,
    ));
    let bytes_after_a = std::fs::read(&file_a).expect("read sidecar a");

    // Upserting B in the same directory must not touch A's sidecar.
    db.upsert_conflict_fingerprinted(
        "src/b.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        101,
        &identity_b,
        &fingerprint_b,
    )
    .await
    .expect("upsert b");
    assert_eq!(
        std::fs::read(&file_a).expect("read sidecar a again"),
        bytes_after_a,
        "B's upsert must not clobber A's sidecar"
    );

    // Re-registering A with the same fingerprint is idempotent and keeps the
    // exact sidecar bytes.
    db.upsert_conflict_fingerprinted(
        "src/a.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        102,
        &identity_a,
        &fingerprint_a,
    )
    .await
    .expect("re-upsert a");
    assert_eq!(
        std::fs::read(&file_a).expect("read sidecar a after re-upsert"),
        bytes_after_a,
        "idempotent re-registration must not rewrite the sidecar"
    );
}

/// A fingerprint-keyed sidecar already claimed by a DIFFERENT conflict (same
/// fingerprint key, different identity path) is never overwritten: the
/// upsert fails hard and the original sidecar survives.
#[tokio::test]
async fn sidecar_claimed_by_another_conflict_is_never_overwritten() {
    let (_dir, db) = new_db().await;
    let conflict_dir = _dir.path().join("conflicts/claimed");
    std::fs::create_dir_all(&conflict_dir).unwrap();

    let mut identity_a = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_a.path = "src/a.rs".into();
    let fingerprint_a = feanorfs_common::compute_conflict_identity_fingerprint(&identity_a);
    db.upsert_conflict_fingerprinted(
        "src/a.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        100,
        &identity_a,
        &fingerprint_a,
    )
    .await
    .expect("upsert a");
    let file_a = conflict_dir.join(crate::conflict_artifacts::identity_file_name(
        &fingerprint_a,
    ));
    let bytes_after_a = std::fs::read(&file_a).expect("read sidecar a");

    // A different identity lying about fingerprint A: the fingerprint key is
    // already claimed, so the write must fail closed.
    let mut identity_b = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_b.path = "src/b.rs".into();
    let error = db
        .upsert_conflict_fingerprinted(
            "src/b.rs",
            &feanorfs_common::ConflictKind::EditEdit,
            &conflict_dir.to_string_lossy(),
            101,
            &identity_b,
            &fingerprint_a,
        )
        .await
        .expect_err("a claimed fingerprint key must fail closed");
    assert!(
        error.to_string().contains("refusing to overwrite"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        std::fs::read(&file_a).expect("read sidecar a after refused upsert"),
        bytes_after_a,
        "the refused upsert must leave A's sidecar untouched"
    );
    // A's record stays the sole fingerprinted record.
    assert!(db
        .is_conflict_fingerprinted("src/a.rs")
        .await
        .expect("a still fingerprinted"));
}

/// A record whose fingerprint-keyed sidecar binds a DIFFERENT path fails
/// closed: the sidecar is verified against the record path + fingerprint,
/// and any mismatch downgrades the record to legacy manual-only.
#[tokio::test]
async fn record_sidecar_path_mismatch_fails_closed() {
    let (_dir, db) = new_db().await;
    let conflict_dir = _dir.path().join("conflicts/mismatch");
    std::fs::create_dir_all(&conflict_dir).unwrap();

    let mut identity_a = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_a.path = "src/a.rs".into();
    let fingerprint_a = feanorfs_common::compute_conflict_identity_fingerprint(&identity_a);
    db.upsert_conflict_fingerprinted(
        "src/a.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        &conflict_dir.to_string_lossy(),
        100,
        &identity_a,
        &fingerprint_a,
    )
    .await
    .expect("upsert a");

    // Replace A's sidecar with a VALID sidecar for a different path: the
    // file name key (fingerprint A) no longer matches its stored fingerprint
    // (fingerprint B), so reads for A fail closed.
    let mut identity_b = feanorfs_common::resolution_contract::resolution_fixtures::edit_edit();
    identity_b.path = "src/b.rs".into();
    let fingerprint_b = feanorfs_common::compute_conflict_identity_fingerprint(&identity_b);
    let sidecar_b = serde_json::json!({
        "schema_version": 1,
        "fingerprint": fingerprint_b,
        "identity": identity_b,
    });
    // B's sidecar also exists at its OWN keyed file name (a second conflict
    // in the same directory).
    std::fs::write(
        conflict_dir.join(crate::conflict_artifacts::identity_file_name(
            &fingerprint_b,
        )),
        serde_json::to_vec(&sidecar_b).unwrap(),
    )
    .unwrap();
    std::fs::write(
        conflict_dir.join(crate::conflict_artifacts::identity_file_name(
            &fingerprint_a,
        )),
        serde_json::to_vec(&sidecar_b).unwrap(),
    )
    .unwrap();

    assert!(!db
        .is_conflict_fingerprinted("src/a.rs")
        .await
        .expect("fingerprinted check"));
    assert!(db
        .list_automatic_candidates()
        .await
        .expect("automatic")
        .is_empty());
    let record = db
        .get_conflict_record("src/a.rs")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(record.status, "legacy_unfingerprinted");

    // B's own keyed sidecar is a different file and reads back fine.
    let (_read_b, read_fp_b) =
        crate::conflict_artifacts::read_identity_sidecar(&conflict_dir, &fingerprint_b)
            .expect("sidecar b");
    assert_eq!(read_fp_b, fingerprint_b);
}

/// Unknown persisted status strings (newer clients) keep loading and never
/// become pending or automatic.
#[tokio::test]
async fn persisted_unknown_status_string_parses_as_unknown() {
    use crate::state::LocalStateV1;
    let dir = tempfile::tempdir().unwrap();
    let db = crate::local::ClientDb::new(dir.path()).await.unwrap();
    db.upsert_conflict(
        "future.txt",
        &feanorfs_common::ConflictKind::EditEdit,
        "/tmp/future",
        1,
        ConflictRecordStatus::Unknown,
    )
    .await
    .unwrap();
    let db2 = crate::local::ClientDb::new(dir.path()).await.unwrap();
    assert!(db2.list_pending_conflict_paths().await.unwrap().is_empty());
    // Raw JSON keeps the unknown string (serde(other) round-trips as "unknown").
    let state = LocalStateV1::from_json(
        &std::fs::read_to_string(dir.path().join("local_state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state.conflict_registry["future.txt"].status,
        ConflictRecordStatus::Unknown
    );
}

#[tokio::test]
async fn session_keys_persist_and_overwrite() {
    let (_dir, db) = new_db().await;
    db.set_session_key("last_scan", r#"{"files":[]}"#)
        .await
        .expect("set");
    assert_eq!(
        db.get_session_key("last_scan").await.expect("get"),
        Some(r#"{"files":[]}"#.to_string())
    );
    assert_eq!(db.get_session_key("nonexistent").await.expect("get"), None);
    db.set_session_key("last_scan", "updated")
        .await
        .expect("set");
    assert_eq!(
        db.get_session_key("last_scan").await.expect("get"),
        Some("updated".to_string())
    );
}
