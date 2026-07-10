use super::new_db;

#[tokio::test]
async fn conflict_registry_crud() {
    let (_dir, db) = new_db().await;
    db.upsert_conflict(
        "src/lib.rs",
        &feanorfs_common::ConflictKind::EditEdit,
        "/tmp/conflict1",
        100,
        "pending",
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
            "pending",
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
            "pending",
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
    db.record_conflict_resolution("a.txt", "local", Some("hash1"), "human")
        .await
        .expect("record");
    db.record_conflict_resolution("b.txt", "cloud", None, "agent")
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
