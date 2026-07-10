use super::{cache_entry, new_db};

#[tokio::test]
async fn cache_entry_crud() {
    let (_dir, db) = new_db().await;
    db.upsert_cache_entry(&cache_entry("src/main.rs", "main", 42))
        .await
        .expect("upsert");
    let all = db.get_cache_entries().await.expect("get all");
    assert_eq!(all.len(), 1);
    assert_eq!(all.get("src/main.rs").expect("entry").size, 42);

    db.delete_cache_entry("src/main.rs").await.expect("delete");
    assert!(db
        .get_cache_entries()
        .await
        .expect("get after delete")
        .is_empty());
}

#[tokio::test]
async fn cache_entry_set_deleted_at() {
    let (_dir, db) = new_db().await;
    db.upsert_cache_entry(&cache_entry("f.txt", "f", 1))
        .await
        .expect("upsert");
    db.set_deleted_at("f.txt", 5000)
        .await
        .expect("set deleted_at");
    let all = db.get_cache_entries().await.expect("get");
    assert_eq!(all.get("f.txt").expect("entry").deleted_at, Some(5000));
}

#[tokio::test]
async fn cache_entry_server_mtime_update() {
    let (_dir, db) = new_db().await;
    let mut entry = cache_entry("f.txt", "f", 1);
    entry.mtime = 100;
    entry.server_mtime = 100;
    db.upsert_cache_entry(&entry).await.expect("upsert");
    db.set_cache_server_mtime("f.txt", 9999)
        .await
        .expect("set server mtime");
    let stored = db
        .get_cache_entries()
        .await
        .expect("get")
        .remove("f.txt")
        .expect("entry");
    assert_eq!(stored.server_mtime, 9999);
    assert_eq!(stored.mtime, 100);
}

#[tokio::test]
async fn bulk_upsert_commits_multiple_entries_at_once() {
    let (_dir, db) = new_db().await;
    let entries = (0..100)
        .map(|index| cache_entry(&format!("file_{index:03}.txt"), &index.to_string(), index))
        .collect::<Vec<_>>();
    db.bulk_upsert_cache_entries(&entries)
        .await
        .expect("bulk upsert");
    let all = db.get_cache_entries().await.expect("get all");
    assert_eq!(all.len(), 100);
    assert_eq!(all.get("file_042.txt").expect("entry").size, 42);
}

#[tokio::test]
async fn bulk_upsert_overwrites_existing_entries() {
    let (_dir, db) = new_db().await;
    db.upsert_cache_entry(&cache_entry("a.txt", "old", 1))
        .await
        .expect("upsert old");
    db.bulk_upsert_cache_entries(&[cache_entry("a.txt", "new", 99)])
        .await
        .expect("bulk overwrite");
    let all = db.get_cache_entries().await.expect("get");
    assert_eq!(all.get("a.txt").expect("entry").size, 99);
}
