use std::sync::Arc;

use http::Method;

use super::LocalHub;

#[tokio::test]
async fn migration_waits_for_inflight_publication() {
    let directory = tempfile::tempdir().expect("create hub directory");
    let hub = LocalHub::open(directory.path().join("hub"), None)
        .await
        .expect("open hub");
    let publication_guard = hub.publication_lock.read().await;
    let migration_hub = Arc::clone(&hub);
    let token = "a".repeat(64);
    let migration = tokio::spawn(async move {
        migration_hub
            .request(
                Method::POST,
                "/api/workspace/migration",
                "workspace_id=workspace",
                Vec::new(),
                (None, Some(&token)),
                None,
            )
            .await
            .expect("migration request")
    });

    tokio::task::yield_now().await;
    assert!(
        !migration.is_finished(),
        "migration established its fence while publication was in flight"
    );

    drop(publication_guard);
    assert_eq!(
        migration.await.expect("migration task").status(),
        http::StatusCode::OK
    );
}
