//! Two clones sharing work on different Git commits: the publishing clone's
//! baseline rides inside the encrypted sync snapshot and the other clone's
//! status warns about the mismatch.

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{do_status, do_sync, load_config, save_config};
use feanorfs_common::git_baseline::GitBaseline;
use std::path::Path;
use support::{
    spawn_test_client_with_server, spawn_test_server, write_workspace_file, TEST_PASSWORD,
    WORKSPACE_ID,
};

fn make_v3(workspace: &Path) {
    let mut config = load_config(workspace).unwrap();
    config.format_version = 3;
    save_config(workspace, &config).unwrap();
}

fn fake_clone(workspace: &Path, branch: &str, commit: char) -> String {
    let id: String = std::iter::repeat_n(commit, 40).collect();
    std::fs::create_dir_all(workspace.join(".git/refs/heads")).unwrap();
    std::fs::write(
        workspace.join(".git/HEAD"),
        format!("ref: refs/heads/{branch}\n"),
    )
    .unwrap();
    std::fs::write(workspace.join(format!(".git/refs/heads/{branch}")), &id).unwrap();
    id
}

#[tokio::test]
async fn status_warns_when_clones_share_work_on_different_git_commits() {
    let server = spawn_test_server().await;
    let linux = spawn_test_client_with_server(&server).await;
    let mac = spawn_test_client_with_server(&server).await;
    let (linux_dir, mac_dir) = (linux.workspace.path(), mac.workspace.path());
    make_v3(linux_dir);
    make_v3(mac_dir);
    let published = fake_clone(linux_dir, "main", 'a');
    fake_clone(mac_dir, "feature", 'b');

    write_workspace_file(linux_dir, "src/lib.rs", b"wip").await;
    do_sync(
        &server.api,
        &linux.db,
        linux_dir,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &mac.db,
        mac_dir,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let mac_status = do_status(
        &server.api,
        &mac.db,
        mac_dir,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let mismatch = mac_status
        .git_baseline_mismatch
        .expect("different commits must be reported");
    assert_eq!(
        mismatch.shared,
        GitBaseline::new(&published, Some("main")).unwrap()
    );
    assert_eq!(mismatch.local.branch.as_deref(), Some("feature"));

    let linux_status = do_status(
        &server.api,
        &linux.db,
        linux_dir,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(linux_status.git_baseline_mismatch.is_none());
    assert!(
        !mac_dir.join(".git/objects").exists(),
        ".git is never written"
    );
}
