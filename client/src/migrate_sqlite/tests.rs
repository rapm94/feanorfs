use super::cache::normalize_dto;
use super::journal::{load_journal, Fault, StorePhase};
use super::migrate_workspace_stores_with_fault;
use feanorfs_agent_core::{
    ClientDb, LocalHub, MigrationAccessEntry, MigrationCacheEntry, MigrationConflictRecord,
    MigrationConflictResolution, MigrationHubState, MigrationHubWorkspace, MigrationLocalState,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn hub_setup() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let hub_dir = root.join(".feanorfs/hub-data");
    std::fs::create_dir_all(hub_dir.join("blobs")).unwrap();
    (dir, root, hub_dir)
}

async fn create_hub_db(db_path: &Path, blob_dir: &Path) {
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE IF NOT EXISTS files (workspace_id TEXT, path TEXT, hash TEXT, size INTEGER, mtime INTEGER, mode INTEGER DEFAULT 0, deleted BOOLEAN DEFAULT 0, PRIMARY KEY(workspace_id,path))").execute(&pool).await.unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS heads (workspace_id TEXT PRIMARY KEY, snapshot_id TEXT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE TABLE IF NOT EXISTS workspace_formats (workspace_id TEXT PRIMARY KEY, format_version INTEGER)").execute(&pool).await.unwrap();
    sqlx::query("CREATE TABLE IF NOT EXISTS snapshot_manifests (workspace_id TEXT, snapshot_id TEXT, manifest BLOB, created_at_ms INTEGER, PRIMARY KEY(workspace_id,snapshot_id))").execute(&pool).await.unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS migration_fences (workspace_id TEXT PRIMARY KEY, token TEXT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let h1 = feanorfs_common::hash_bytes(b"blob1");
    std::fs::write(blob_dir.join(&h1), b"blob1").unwrap();
    let h2 = feanorfs_common::hash_bytes(b"blob2");
    std::fs::write(blob_dir.join(&h2), b"blob2").unwrap();
    sqlx::query("INSERT INTO files VALUES ('ws1','a.txt',?,10,1000,0,0)")
        .bind(&h1)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO files VALUES ('ws1','b.txt',?,20,2000,1,1)")
        .bind(&h2)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO heads VALUES ('ws1','snap1')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_formats VALUES ('ws1',2)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO snapshot_manifests VALUES ('ws1','snap1',?,5000)")
        .bind(format!("{h1}\n{h2}\n").as_bytes())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO migration_fences VALUES ('ws1','fence-token')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn migrates_embedded_hub_exactly() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
    let target = LocalHub::open_for_migration(root.join(".feanorfs/hub-data"))
        .await
        .unwrap();
    let dto = target.migration_db().export_for_migration().unwrap();
    let ws = dto.workspaces.get("ws1").unwrap();
    assert_eq!(ws.format_version, 2);
    assert_eq!(ws.head.as_deref(), Some("snap1"));
    assert_eq!(ws.files.len(), 2);
    assert!(ws.files.get("b.txt").unwrap().deleted);
    assert_eq!(ws.manifests.len(), 1);
    assert!(ws.migration_fence.is_some());
}

#[tokio::test]
async fn hub_blobs_remain_byte_identical() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    let h1 = feanorfs_common::hash_bytes(b"blob1");
    let before = std::fs::read(hub_dir.join("blobs").join(&h1)).unwrap();
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    let after = std::fs::read(hub_dir.join("blobs").join(&h1)).unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn hub_migration_rerun_is_noop() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(hub_dir.join("db.migrated-v1.db").exists());
}

#[tokio::test]
async fn hub_json_sqlite_divergence_blocks() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    let target = LocalHub::open_for_migration(root.join(".feanorfs/hub-data"))
        .await
        .unwrap();
    let mut dto = MigrationHubState {
        workspaces: BTreeMap::new(),
    };
    dto.workspaces.insert(
        "ws1".into(),
        MigrationHubWorkspace {
            format_version: 2,
            head: None,
            manifests: BTreeMap::new(),
            files: BTreeMap::new(),
            migration_fence: None,
        },
    );
    target.migration_db().replace_from_migration(&dto).unwrap();
    let err = migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("divergent"));
    assert!(db_path.exists());
}

#[tokio::test]
async fn hub_mid_archive_fault_resumes() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO files VALUES ('ws2','c.txt','cc',1,1,0,0)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let r = migrate_workspace_stores_with_fault(&root, Fault::MidSidecarArchive).await;
    match r {
        Ok(()) => {}
        Err(e) => {
            assert!(e.to_string().contains("mid_sidecar"));
        }
    }
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(hub_dir.join("db.migrated-v1.db").exists());
}

#[tokio::test]
async fn hub_existing_archive_collision_blocks() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    std::fs::write(hub_dir.join("db.migrated-v1.db"), b"different").unwrap();
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    let err = migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("exists with different content"));
}

#[tokio::test]
async fn migrates_hub_wal_committed_rows() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO files VALUES ('ws_wal','wal.txt','hh',99,9999,0,0)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    let target = LocalHub::open_for_migration(root.join(".feanorfs/hub-data"))
        .await
        .unwrap();
    let dto = target.migration_db().export_for_migration().unwrap();
    assert!(dto
        .workspaces
        .get("ws_wal")
        .unwrap()
        .files
        .contains_key("wal.txt"));
}

#[tokio::test]
async fn migrates_hub_with_missing_historical_tables() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE files (workspace_id TEXT, path TEXT, hash TEXT, size INTEGER, mtime INTEGER, deleted BOOLEAN DEFAULT 0, PRIMARY KEY(workspace_id,path))").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO files VALUES ('ws_min','f.txt','hh',1,1,0)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    let target = LocalHub::open_for_migration(root.join(".feanorfs/hub-data"))
        .await
        .unwrap();
    let dto = target.migration_db().export_for_migration().unwrap();
    let ws = dto.workspaces.get("ws_min").unwrap();
    assert_eq!(ws.files.len(), 1);
    assert_eq!(ws.format_version, 2);
    assert!(ws.head.is_none());
}

#[tokio::test]
async fn localhub_legacy_guard_blocks_and_no_target_created() {
    let (_dir, _root, hub_dir) = hub_setup();
    std::fs::write(hub_dir.join("db.sqlite"), b"fake sqlite").unwrap();
    let err = LocalHub::open(hub_dir.clone(), None).await.unwrap_err();
    assert!(err.to_string().contains("feanorfs migrate"));
    assert!(!hub_dir.join("hub_state.json").exists());
}

#[tokio::test]
async fn hub_fault_after_imported_journal_resumes() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    let _ = migrate_workspace_stores_with_fault(&root, Fault::AfterTargetWrite).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    let journal = load_journal(&root).unwrap();
    assert_eq!(
        journal.stores.get("hub").unwrap().phase,
        StorePhase::Archived
    );
}

#[tokio::test]
async fn hub_fault_after_verified_journal_resumes() {
    let (_dir, root, hub_dir) = hub_setup();
    let db_path = hub_dir.join("db.sqlite");
    create_hub_db(&db_path, &root.join(".feanorfs/hub-data/blobs")).await;
    migrate_workspace_stores_with_fault(&root, Fault::BeforeArchive)
        .await
        .unwrap_err();
    assert!(db_path.exists());
    let journal = load_journal(&root).unwrap();
    assert_eq!(
        journal.stores.get("hub").unwrap().phase,
        StorePhase::Verified
    );
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
}

fn expected_dto() -> MigrationLocalState {
    let mut dto = MigrationLocalState {
        local_files: BTreeMap::new(),
        file_access_log: vec![],
        last_session: BTreeMap::new(),
        conflict_registry: BTreeMap::new(),
        conflict_resolutions: vec![],
    };
    dto.local_files.insert(
        "a.txt".into(),
        MigrationCacheEntry {
            plaintext_hash: "ph-a".into(),
            encrypted_hash: "eh-a".into(),
            size: 10,
            mtime: 1000,
            server_mtime: 1000,
            mode: 0,
            hydrated: true,
            deleted_at: None,
        },
    );
    dto.local_files.insert(
        "b.txt".into(),
        MigrationCacheEntry {
            plaintext_hash: "ph-b".into(),
            encrypted_hash: "eh-b".into(),
            size: 20,
            mtime: 2000,
            server_mtime: 2000,
            mode: 1,
            hydrated: true,
            deleted_at: Some(3000),
        },
    );
    dto.file_access_log.push(MigrationAccessEntry {
        path: "a.txt".into(),
        sibling_path: "b.txt".into(),
        weight: 5.0,
        updated_at: 4000,
    });
    dto.last_session.insert("k1".into(), "v1".into());
    dto.conflict_registry.insert(
        "src/lib.rs".into(),
        MigrationConflictRecord {
            path: "src/lib.rs".into(),
            kind: feanorfs_common::ConflictKind::EditEdit,
            conflict_dir: "/tmp/c1".into(),
            opened_at: 5000,
            status: "pending".into(),
        },
    );
    dto.conflict_resolutions.push(MigrationConflictResolution {
        path: "x.txt".into(),
        method: "local".into(),
        source_file_hash: Some("hash1".into()),
        resolved_at: 6000,
        resolver: "human".into(),
    });
    dto
}

async fn create_populated_cache(db_path: &Path) {
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    for stmt in [
        "CREATE TABLE local_files (path TEXT PRIMARY KEY, plaintext_hash TEXT, encrypted_hash TEXT, size INTEGER, mtime INTEGER, server_mtime INTEGER DEFAULT 0, mode INTEGER DEFAULT 0, hydrated INTEGER DEFAULT 1, deleted_at INTEGER)",
        "CREATE TABLE file_access_log (path TEXT, sibling_path TEXT, weight REAL, updated_at INTEGER, PRIMARY KEY(path,sibling_path))",
        "CREATE TABLE last_session (key TEXT PRIMARY KEY, value TEXT)",
        "CREATE TABLE conflict_registry (path TEXT PRIMARY KEY, kind TEXT, conflict_dir TEXT, opened_at INTEGER, status TEXT DEFAULT 'pending')",
        "CREATE TABLE conflict_resolutions (path TEXT, method TEXT, source_file_hash TEXT, resolved_at INTEGER, resolver TEXT)",
        "INSERT INTO local_files VALUES ('a.txt','ph-a','eh-a',10,1000,1000,0,1,NULL)",
        "INSERT INTO local_files VALUES ('b.txt','ph-b','eh-b',20,2000,2000,1,1,3000)",
        "INSERT INTO file_access_log VALUES ('a.txt','b.txt',5.0,4000)",
        "INSERT INTO last_session VALUES ('k1','v1')",
        "INSERT INTO conflict_registry VALUES ('src/lib.rs','edit_edit','/tmp/c1',5000,'pending')",
        "INSERT INTO conflict_resolutions VALUES ('x.txt','local','hash1',6000,'human')",
    ] {
        sqlx::query(stmt).execute(&pool).await.unwrap();
    }
    sqlx::query("PRAGMA journal_mode=DELETE")
        .execute(&pool)
        .await
        .ok();
    pool.close().await;
    std::thread::sleep(std::time::Duration::from_millis(50));
}

fn setup() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(".feanorfs")).unwrap();
    (dir, root)
}

#[tokio::test]
async fn migrates_current_cache_exactly() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
    let target = ClientDb::open_for_migration(&root.join(".feanorfs"))
        .await
        .unwrap();
    let mut e = target.export_for_migration().await.unwrap();
    normalize_dto(&mut e);
    let mut exp = expected_dto();
    normalize_dto(&mut exp);
    assert_eq!(e, exp);
}

#[tokio::test]
async fn migrates_historical_cache_with_missing_tables_and_columns() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE local_files (path TEXT PRIMARY KEY, plaintext_hash TEXT, encrypted_hash TEXT, size INTEGER, mtime INTEGER, server_mtime INTEGER DEFAULT 0, hydrated INTEGER DEFAULT 1)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO local_files VALUES ('only.txt','ph','eh',1,1,1,1)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    let e = ClientDb::open_for_migration(&root.join(".feanorfs"))
        .await
        .unwrap()
        .export_for_migration()
        .await
        .unwrap();
    assert_eq!(e.local_files.len(), 1);
    assert!(e.file_access_log.is_empty());
}

#[tokio::test]
async fn cache_migration_rerun_is_noop() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(root.join(".feanorfs/local_cache.migrated-v1.db").exists());
}

#[tokio::test]
async fn divergent_json_sqlite_coexistence_blocks() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    let target = ClientDb::open_for_migration(&root.join(".feanorfs"))
        .await
        .unwrap();
    target
        .upsert_cache_entry(&crate::local::CacheEntry {
            path: "divergent.txt".into(),
            plaintext_hash: "xx".into(),
            encrypted_hash: "xx".into(),
            size: 99,
            mtime: 99,
            server_mtime: 99,
            mode: 0,
            hydrated: true,
            deleted_at: None,
        })
        .await
        .unwrap();
    let err = migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("divergent"));
    assert!(db_path.exists());
}

#[tokio::test]
async fn fault_before_target_write_preserves_source() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    let err = migrate_workspace_stores_with_fault(&root, Fault::BeforeTargetWrite)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("before_target_write"));
    assert!(db_path.exists());
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
}

#[tokio::test]
async fn fault_after_target_write_preserves_target_and_source() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    let err = migrate_workspace_stores_with_fault(&root, Fault::AfterTargetWrite)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("after_target_write"));
    assert!(db_path.exists());
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
}

#[tokio::test]
async fn fault_before_archive_preserves_source_and_resumes() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    let err = migrate_workspace_stores_with_fault(&root, Fault::BeforeArchive)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("before_archive"));
    assert!(db_path.exists());
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
}

#[tokio::test]
async fn equal_json_sqlite_coexistence_archives() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
    create_populated_cache(&db_path).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!db_path.exists());
    assert!(root.join(".feanorfs/local_cache.migrated-v1.db").exists());
}

#[tokio::test]
async fn existing_different_archive_blocks() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&db_path).await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    std::fs::write(
        root.join(".feanorfs/local_cache.migrated-v1.db"),
        b"different",
    )
    .unwrap();
    create_populated_cache(&db_path).await;
    let err = migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("exists with different content"));
    assert!(db_path.exists());
}

#[tokio::test]
async fn migrates_wal_committed_rows() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE local_files (path TEXT PRIMARY KEY, plaintext_hash TEXT, encrypted_hash TEXT, size INTEGER, mtime INTEGER, server_mtime INTEGER DEFAULT 0, mode INTEGER DEFAULT 0, hydrated INTEGER DEFAULT 1, deleted_at INTEGER)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO local_files VALUES ('wal.txt','ph-w','eh-w',42,4200,4200,0,1,NULL)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    let e = ClientDb::open_for_migration(&root.join(".feanorfs"))
        .await
        .unwrap()
        .export_for_migration()
        .await
        .unwrap();
    assert_eq!(e.local_files.get("wal.txt").unwrap().size, 42);
}

#[tokio::test]
async fn migrates_root_and_two_agent_caches() {
    let (_dir, root) = setup();
    let root_db = root.join(".feanorfs/local_cache.db");
    create_populated_cache(&root_db).await;
    for name in &["agent-a", "agent-b"] {
        let ad = root.join(".feanorfs/agents").join(name).join(".feanorfs");
        std::fs::create_dir_all(&ad).unwrap();
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(ad.join("local_cache.db"))
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE local_files (path TEXT PRIMARY KEY, plaintext_hash TEXT, encrypted_hash TEXT, size INTEGER, mtime INTEGER, server_mtime INTEGER DEFAULT 0, mode INTEGER DEFAULT 0, hydrated INTEGER DEFAULT 1, deleted_at INTEGER)").execute(&pool).await.unwrap();
        sqlx::query(&format!(
            "INSERT INTO local_files VALUES ('{name}.txt','ph','eh',1,1,1,0,1,NULL)"
        ))
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert!(!root_db.exists());
    for name in &["agent-a", "agent-b"] {
        assert!(!root
            .join(".feanorfs/agents")
            .join(name)
            .join(".feanorfs/local_cache.db")
            .exists());
    }
}

#[tokio::test]
async fn fault_mid_sidecar_archive_resumes() {
    let (_dir, root) = setup();
    let db_path = root.join(".feanorfs/local_cache.db");
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE local_files (path TEXT PRIMARY KEY, plaintext_hash TEXT, encrypted_hash TEXT, size INTEGER, mtime INTEGER, server_mtime INTEGER DEFAULT 0, mode INTEGER DEFAULT 0, hydrated INTEGER DEFAULT 1, deleted_at INTEGER)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO local_files VALUES ('w.txt','ph','eh',1,1,1,0,1,NULL)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let r = migrate_workspace_stores_with_fault(&root, Fault::MidSidecarArchive).await;
    match r {
        Ok(()) => {}
        Err(e) => {
            assert!(e.to_string().contains("mid_sidecar"));
        }
    }
    migrate_workspace_stores_with_fault(&root, Fault::None)
        .await
        .unwrap();
    assert_eq!(
        load_journal(&root)
            .unwrap()
            .stores
            .get("main")
            .unwrap()
            .phase,
        StorePhase::Archived
    );
}

#[tokio::test]
async fn e2e_wiring_migrates_root_agent_hub_and_opens_normally() {
    use crate::local::ClientDb;
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let feanorfs = root.join(".feanorfs");
    std::fs::create_dir_all(&feanorfs).unwrap();
    let root_db = feanorfs.join("local_cache.db");
    create_populated_cache(&root_db).await;
    let agent_dir = feanorfs.join("agents/ci1/.feanorfs");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let ag_opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(agent_dir.join("local_cache.db"))
        .create_if_missing(true);
    let ag_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(ag_opts)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE local_files (path TEXT PRIMARY KEY, plaintext_hash TEXT, encrypted_hash TEXT, size INTEGER, mtime INTEGER, server_mtime INTEGER DEFAULT 0, mode INTEGER DEFAULT 0, hydrated INTEGER DEFAULT 1, deleted_at INTEGER)").execute(&ag_pool).await.unwrap();
    sqlx::query("INSERT INTO local_files VALUES ('agent.txt','ph-ag','eh-ag',1,1,1,0,1,NULL)")
        .execute(&ag_pool)
        .await
        .unwrap();
    ag_pool.close().await;
    let hub_dir = feanorfs.join("hub-data");
    std::fs::create_dir_all(hub_dir.join("blobs")).unwrap();
    let hub_db = hub_dir.join("db.sqlite");
    create_hub_db(&hub_db, &hub_dir.join("blobs")).await;
    let config = serde_json::json!({"server_url":"feanorfs+local://hub","workspace_id":"ws1","encryption_password":"abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234","hub_local":true,"format_version":3});
    std::fs::write(feanorfs.join("config.json"), config.to_string()).unwrap();
    crate::migrate_sqlite::migrate_workspace_stores(&root)
        .await
        .unwrap();
    let db = ClientDb::new(&feanorfs).await.unwrap();
    let entries = db.get_cache_entries().await.unwrap();
    assert!(entries.contains_key("a.txt"));
    assert!(entries.contains_key("b.txt"));
    let agent_entries = ClientDb::new(&agent_dir)
        .await
        .unwrap()
        .get_cache_entries()
        .await
        .unwrap();
    assert!(agent_entries.contains_key("agent.txt"));
    assert!(!hub_db.exists());
    assert!(!root_db.exists());
    crate::migrate_sqlite::migrate_workspace_stores(&root)
        .await
        .unwrap();
    assert!(feanorfs.join("local_cache.migrated-v1.db").exists());
}

#[test]
fn no_raw_client_db_new_in_production_cli_modules() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(src_dir.join("cli")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap();
        let has_raw =
            content.contains("ClientDb::new(") || content.contains("ApiClient::from_config(");
        assert!(
            !has_raw,
            "raw constructor in production CLI file: {}",
            path.display()
        );
    }
    for f in &["tray.rs", "migrate.rs"] {
        let content = std::fs::read_to_string(src_dir.join(f)).unwrap();
        let has_raw =
            content.contains("ClientDb::new(") || content.contains("ApiClient::from_config(");
        assert!(!has_raw, "raw constructor in production file: {f}");
    }
}
