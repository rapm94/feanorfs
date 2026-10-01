use super::*;
use crate::local::ClientDb;
use crate::{ApiClient, LocalHub};
use feanorfs_common::{hash_bytes, pack_bytes, FileState, LegacyPolicy};
use std::os::unix::fs::MetadataExt as _;

async fn recover_case(hydrated: bool, replaced: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let hub_dir = tempfile::tempdir().unwrap();
    let hub = LocalHub::open(hub_dir.path().to_path_buf(), None)
        .await
        .unwrap();
    let api = ApiClient::local(hub, None);
    let db = ClientDb::new(state.path()).await.unwrap();
    let password = "recovery-test-key";
    let ctx = SyncCtx::new(
        &api,
        &db,
        workspace.path(),
        "recovery-test",
        Some(password),
        LegacyPolicy::Reject,
    );
    let stage = workspace
        .path()
        .join(".feanorfs-tmp-materialize-regression");
    fs::create_dir_all(stage.join("new")).await.unwrap();
    let bytes: &[u8] = if hydrated {
        b"same bytes, different owner"
    } else {
        b""
    };
    let staged = stage.join("new/file.txt");
    let destination = workspace.path().join("file.txt");
    fs::write(&staged, bytes).await.unwrap();
    if replaced {
        fs::write(&destination, bytes).await.unwrap();
        assert_ne!(
            fs::metadata(&staged).await.unwrap().ino(),
            fs::metadata(&destination).await.unwrap().ino()
        );
    } else {
        fs::hard_link(&staged, &destination).await.unwrap();
    }
    let journal = MaterializationJournal {
        phase: "activating".into(),
        publication_progress_recorded: false,
        original_paths: Vec::new(),
        original_readonly: Default::default(),
        downloads: vec![JournalDownload {
            file: FileState {
                path: "file.txt".into(),
                hash: hash_bytes(&pack_bytes(bytes, password, "file.txt").unwrap()),
                size: bytes.len() as u64,
                mtime: 0,
                deleted: false,
                mode: 0,
            },
            plaintext_hash: hash_bytes(bytes),
            hydrated,
        }],
        delete_paths: Vec::new(),
        published_paths: Vec::new(),
        publishing_path: None,
        created_directories: Vec::new(),
    };
    write_materialization_journal(&stage, &journal)
        .await
        .unwrap();
    for _ in 0..2 {
        let result = recover_materialization_stages(&ctx).await;
        if replaced {
            assert!(
                result.is_err(),
                "ambiguous ownership must retain recovery state"
            );
            assert_eq!(fs::read(&destination).await.unwrap(), bytes);
            assert_eq!(fs::read(&staged).await.unwrap(), bytes);
            assert!(stage.join("journal.json").exists());
        } else {
            result.unwrap();
            assert!(!destination.exists());
            assert!(!stage.exists());
        }
    }
}

#[tokio::test]
async fn backup_recovery_retries_after_previous_backup_was_restored() {
    let workspace = tempfile::tempdir().unwrap();
    let stage = tempfile::tempdir_in(workspace.path()).unwrap();
    fs::create_dir(stage.path().join("backup")).await.unwrap();
    fs::write(workspace.path().join("first.txt"), b"already restored")
        .await
        .unwrap();
    fs::write(stage.path().join("backup/second.txt"), b"still backed up")
        .await
        .unwrap();
    let anchors = open_materialization_anchors(workspace.path(), stage.path())
        .await
        .unwrap();
    assert!(matches!(
        inspect_backup_recovery(&anchors, "first.txt")
            .await
            .unwrap(),
        BackupRecoveryState::Missing
    ));
    assert!(matches!(
        inspect_backup_recovery(&anchors, "second.txt")
            .await
            .unwrap(),
        BackupRecoveryState::DestinationMissing
    ));
    restore_backup_no_follow(&anchors, "second.txt")
        .await
        .unwrap();
    for path in ["first.txt", "second.txt"] {
        assert!(matches!(
            inspect_backup_recovery(&anchors, path).await.unwrap(),
            BackupRecoveryState::Missing
        ));
    }
    assert_eq!(
        fs::read(workspace.path().join("first.txt")).await.unwrap(),
        b"already restored"
    );
    assert_eq!(
        fs::read(workspace.path().join("second.txt")).await.unwrap(),
        b"still backed up"
    );
}

#[tokio::test]
async fn recovery_preserves_same_bytes_different_inode() {
    recover_case(true, true).await;
}

#[tokio::test]
async fn recovery_preserves_replaced_lazy_placeholder() {
    recover_case(false, true).await;
}

#[tokio::test]
async fn recovery_removes_only_retained_publication_inode() {
    recover_case(true, false).await;
}
