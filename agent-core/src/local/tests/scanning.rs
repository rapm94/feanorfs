use std::collections::HashSet;
use std::fs;
use std::path::Path;

use super::super::walker::CACHEDIR_TAG_SIGNATURE;
use super::super::{
    build_workspace_walker, collect_symlink_warnings, scan_local_directory,
    scan_local_directory_with_policy, ClientDb,
};

fn walked_files(root: &Path) -> HashSet<String> {
    build_workspace_walker(root, true)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .map(feanorfs_common::normalize_path)
        })
        .collect()
}

async fn scanned_files(root: &Path) -> HashSet<String> {
    let state = tempfile::tempdir().expect("create scanner state");
    let db = ClientDb::new(state.path())
        .await
        .expect("create scanner DB");
    scan_local_directory(root, &db, Some("test-key"))
        .await
        .expect("scan workspace")
        .into_keys()
        .collect()
}

fn write_tagged_tree(root: &Path) {
    fs::create_dir_all(root.join("cache")).expect("create tagged cache");
    fs::write(
        root.join("cache/CACHEDIR.TAG"),
        [CACHEDIR_TAG_SIGNATURE, b"This directory is disposable.\n"].concat(),
    )
    .expect("write valid tag");
    fs::write(root.join("cache/generated.bin"), b"generated").expect("write cache file");
    fs::create_dir_all(root.join("not-cache")).expect("create untagged directory");
    fs::write(
        root.join("not-cache/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55",
    )
    .expect("write invalid tag without LF");
    fs::write(root.join("not-cache/keep.txt"), b"keep").expect("write retained file");
}

#[tokio::test]
async fn valid_cachedir_tag_prunes_main_and_agent_scans() {
    let main = tempfile::tempdir().expect("create main workspace");
    let agent = tempfile::tempdir().expect("create agent workspace");
    write_tagged_tree(main.path());
    write_tagged_tree(agent.path());

    for root in [main.path(), agent.path()] {
        let files = scanned_files(root).await;
        assert!(!files.contains("cache/CACHEDIR.TAG"));
        assert!(!files.contains("cache/generated.bin"));
        assert!(files.contains("not-cache/CACHEDIR.TAG"));
        assert!(files.contains("not-cache/keep.txt"));
    }
}

#[tokio::test]
async fn valid_cachedir_tag_at_workspace_root_does_not_prune_workspace() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("CACHEDIR.TAG"),
        CACHEDIR_TAG_SIGNATURE,
    )
    .expect("write valid root tag");
    fs::write(workspace.path().join("keep.txt"), b"workspace content")
        .expect("write workspace file");

    let files = scanned_files(workspace.path()).await;
    assert!(files.contains("CACHEDIR.TAG"));
    assert!(files.contains("keep.txt"));
}

#[tokio::test]
async fn custom_directory_ignore_prunes_the_directory_before_descending() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir_all(workspace.path().join("server-data/blobs"))
        .expect("create ignored directory");
    fs::write(
        workspace.path().join("server-data/blobs/changing"),
        b"runtime data",
    )
    .expect("write ignored runtime file");
    fs::write(workspace.path().join("keep.txt"), b"workspace content")
        .expect("write retained file");

    let state = tempfile::tempdir().expect("create scanner state");
    let db = ClientDb::new(state.path())
        .await
        .expect("create scanner DB");
    let files = scan_local_directory_with_policy(
        workspace.path(),
        &db,
        Some("test-key"),
        false,
        Some("server-data/\n"),
    )
    .await
    .expect("scan with global ignore rules")
    .into_keys()
    .collect::<HashSet<_>>();
    assert!(files.contains("keep.txt"));
    assert!(!files.contains("server-data/blobs/changing"));
    assert!(!workspace.path().join(".feanorfsignore").exists());
}

#[test]
fn vcs_and_legacy_metadata_are_hard_excluded() {
    let workspace = tempfile::tempdir().expect("create workspace");
    for path in [".git/index", ".jj/repo/store", ".feanorfs/config.json"] {
        let path = workspace.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"metadata").unwrap();
    }
    fs::write(workspace.path().join(".feanorfsignore"), b"legacy").unwrap();
    fs::write(workspace.path().join("keep.txt"), b"content").unwrap();

    let files = walked_files(workspace.path());

    assert_eq!(files, HashSet::from(["keep.txt".to_string()]));
}

#[cfg(unix)]
#[test]
fn symlink_warnings_are_sorted_and_links_are_never_followed() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir(workspace.path().join("real-dir")).expect("create real directory");
    fs::write(workspace.path().join("real-dir/secret.txt"), b"secret").expect("write target file");
    fs::write(workspace.path().join("real-file"), b"file").expect("write target");
    symlink("real-file", workspace.path().join("z-file-link")).expect("create file link");
    symlink("real-dir", workspace.path().join("a-dir-link")).expect("create directory link");

    assert_eq!(
        collect_symlink_warnings(workspace.path()),
        vec!["a-dir-link", "z-file-link"]
    );
    assert!(!walked_files(workspace.path()).contains("a-dir-link/secret.txt"));
}

#[tokio::test]
#[ignore = "manual 10k scanner profile"]
async fn scan_profile_10k() {
    let corpus = tempfile::tempdir().expect("create corpus dir");
    const FILE_COUNT: usize = 10_000;
    for index in 0..FILE_COUNT {
        fs::write(
            corpus.path().join(format!("file_{index:05}.txt")),
            format!("content-{index:05}\n"),
        )
        .expect("write corpus file");
    }
    let db_dir = tempfile::tempdir().expect("create db dir");
    let db = ClientDb::new(db_dir.path()).await.expect("create ClientDb");
    let password = Some("scan-bench-key-64chars___________________________");

    let cold_start = std::time::Instant::now();
    let cold = scan_local_directory(corpus.path(), &db, password)
        .await
        .expect("cold scan");
    let cold_elapsed = cold_start.elapsed();
    assert_eq!(cold.len(), FILE_COUNT);

    let warm_start = std::time::Instant::now();
    let warm = scan_local_directory(corpus.path(), &db, password)
        .await
        .expect("warm scan");
    let warm_elapsed = warm_start.elapsed();
    assert_eq!(warm.len(), FILE_COUNT);

    fs::write(corpus.path().join("file_00420.txt"), b"modified-content\n").expect("modify file");
    let changed_start = std::time::Instant::now();
    let changed = scan_local_directory(corpus.path(), &db, password)
        .await
        .expect("one-change scan");
    let changed_elapsed = changed_start.elapsed();
    assert_eq!(changed.len(), FILE_COUNT);
    eprintln!(
        "scan_profile_10k: cold={cold_elapsed:.2?} warm={warm_elapsed:.2?} one-change={changed_elapsed:.2?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_backslash_filename_is_not_aliased_to_a_portable_path() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir_all(workspace.path().join("literal")).expect("create nested directory");
    fs::write(workspace.path().join("literal/name.txt"), b"nested").expect("write nested file");
    fs::write(workspace.path().join(r"literal\name.txt"), b"backslash")
        .expect("write backslash file");

    let files = scanned_files(workspace.path()).await;
    assert_eq!(files, HashSet::from(["literal/name.txt".to_string()]));
}

#[tokio::test]
async fn resurrected_file_with_preserved_mtime_clears_stale_tombstone() {
    // Deleting a file tombstone it; recreating the same bytes with the same
    // mtime (rsync -a / cp -p / backup restore) used to match the cached
    // entry and keep the stale `deleted_at`, which later made `hydrate`/`cat`
    // and predictive hydration treat the live file as deleted.
    let root = tempfile::tempdir().expect("create workspace");
    let state = tempfile::tempdir().expect("create scanner state");
    let db = ClientDb::new(state.path())
        .await
        .expect("create scanner DB");

    let path = root.path().join("note.txt");
    let original_mtime = 1_700_000_000_000_i64;
    fs::write(&path, b"same bytes").expect("write file");
    set_mtime_ms(&path, original_mtime);

    let scanned = scan_local_directory(root.path(), &db, Some("test-key"))
        .await
        .expect("scan live file");
    assert!(scanned.contains_key("note.txt"));
    assert!(db.get_cache_entries().await.unwrap()["note.txt"]
        .deleted_at
        .is_none());

    // Delete the file and scan: tombstone recorded.
    fs::remove_file(&path).expect("delete file");
    let scanned = scan_local_directory(root.path(), &db, Some("test-key"))
        .await
        .expect("scan deletion");
    assert!(scanned["note.txt"].deleted);
    assert!(db.get_cache_entries().await.unwrap()["note.txt"]
        .deleted_at
        .is_some());

    // Resurrect identical bytes with the preserved mtime.
    fs::write(&path, b"same bytes").expect("restore file");
    set_mtime_ms(&path, original_mtime);
    let scanned = scan_local_directory(root.path(), &db, Some("test-key"))
        .await
        .expect("scan resurrection");
    assert!(!scanned["note.txt"].deleted);
    let entry = db.get_cache_entries().await.unwrap()["note.txt"].clone();
    assert!(
        entry.deleted_at.is_none(),
        "stale tombstone survived same-mtime resurrection"
    );
    assert_eq!(entry.encrypted_hash, scanned["note.txt"].hash);
}

fn set_mtime_ms(path: &std::path::Path, millis: i64) {
    let seconds = millis.div_euclid(1000);
    let nanos = millis.rem_euclid(1000) as u32 * 1_000_000;
    let times = std::fs::FileTimes::new()
        .set_accessed(std::time::UNIX_EPOCH)
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(seconds as u64, nanos));
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open file to set mtime")
        .set_times(times)
        .expect("set mtime");
}

#[tokio::test]
async fn ignored_paths_with_cached_state_are_frozen_not_tombstoned() {
    // Adding an ignore rule for a previously-synced path used to tombstone it
    // in the scan, which deleted the remote copy on the next sync. The scan
    // must freeze the cached state instead: live FileState, no tombstone.
    let root = tempfile::tempdir().expect("create workspace");
    let state = tempfile::tempdir().expect("create scanner state");
    let db = ClientDb::new(state.path())
        .await
        .expect("create scanner DB");

    fs::create_dir_all(root.path().join("out")).expect("create dir");
    fs::write(root.path().join("out/a.txt"), b"important").expect("write file");
    fs::write(root.path().join("keep.txt"), b"keep").expect("write file");

    let scanned = scan_local_directory(root.path(), &db, Some("test-key"))
        .await
        .expect("scan without policy");
    assert!(scanned.contains_key("out/a.txt"));
    let original_hash = scanned["out/a.txt"].hash.clone();

    // Add an ignore rule and rescan with the same policy the sync applies.
    let scanned =
        scan_local_directory_with_policy(root.path(), &db, Some("test-key"), false, Some("out/"))
            .await
            .expect("scan with policy");
    let file = &scanned["out/a.txt"];
    assert!(!file.deleted, "ignored tracked file must not be tombstoned");
    assert_eq!(
        file.hash, original_hash,
        "frozen file keeps its cached identity"
    );
    assert!(scanned.contains_key("keep.txt"));
    let entry = db.get_cache_entries().await.unwrap()["out/a.txt"].clone();
    assert!(
        entry.deleted_at.is_none(),
        "ignored tracked file must not receive a tombstone"
    );
}
