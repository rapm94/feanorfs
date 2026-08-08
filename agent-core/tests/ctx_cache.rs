feanorfs_test_support::isolate_test_process!();

use feanorfs_agent_core::{
    ensure_workspace_state, global_state_root, ApiClient, ClientDb, LocalHub, SyncCtx,
};
use feanorfs_common::LegacyPolicy;
use std::fs;
use std::sync::Arc;

#[tokio::test]
async fn state_dir_cache_retries_pins_and_allows_fresh_context_relocation() {
    let hub_dir = tempfile::tempdir().unwrap();
    let hub = LocalHub::open(hub_dir.path().to_path_buf(), None)
        .await
        .unwrap();
    let api = ApiClient::local(Arc::clone(&hub), None);
    let control = tempfile::tempdir().unwrap();
    let control_state = ensure_workspace_state(control.path()).unwrap();
    let db = ClientDb::new(control_state).await.unwrap();

    // A failed first resolution is not cached: repair the transient global
    // registry obstruction and the same context succeeds on its next call.
    let retry_workspace = tempfile::tempdir().unwrap();
    let retry_ctx = SyncCtx::new(
        &api,
        &db,
        retry_workspace.path(),
        "retry",
        Some("key"),
        LegacyPolicy::Reject,
    );
    let root = global_state_root().unwrap();
    let workspaces = root.join("workspaces");
    let saved = root.join("workspaces-retry-saved");
    fs::rename(&workspaces, &saved).unwrap();
    fs::write(&workspaces, b"temporary obstruction").unwrap();
    assert!(retry_ctx.state_dir().is_err());
    fs::remove_file(&workspaces).unwrap();
    fs::rename(&saved, &workspaces).unwrap();
    let retry_state = retry_ctx.state_dir().unwrap();
    assert!(retry_state.is_dir());

    // Once resolved, concurrent callers all receive the pinned path without
    // touching workspace-state resolution again, even while that path is
    // temporarily moved out of place.
    let moved_state = retry_state.with_extension("cache-test-moved");
    fs::rename(&retry_state, &moved_state).unwrap();
    std::thread::scope(|scope| {
        let handles = (0..16)
            .map(|_| scope.spawn(|| retry_ctx.state_dir().unwrap()))
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), retry_state);
        }
    });
    fs::rename(&moved_state, &retry_state).unwrap();

    // A context pins its successful resolution across an on-disk workspace
    // rename. A fresh context re-resolves the inode identity and updates the
    // durable location while retaining the same private state directory.
    let parent = tempfile::tempdir().unwrap();
    let original = parent.path().join("original");
    let relocated = parent.path().join("relocated");
    fs::create_dir(&original).unwrap();
    let first = SyncCtx::new(
        &api,
        &db,
        &original,
        "relocate",
        Some("key"),
        LegacyPolicy::Reject,
    );
    let state = first.state_dir().unwrap();
    fs::rename(&original, &relocated).unwrap();
    assert_eq!(first.state_dir().unwrap(), state);

    let fresh = SyncCtx::new(
        &api,
        &db,
        &relocated,
        "relocate",
        Some("key"),
        LegacyPolicy::Reject,
    );
    assert_eq!(fresh.state_dir().unwrap(), state);
    assert_eq!(
        fs::read_to_string(state.join("location")).unwrap(),
        fs::canonicalize(relocated).unwrap().to_str().unwrap()
    );
}
