use feanorfs_agent_core::{ApiClient, LocalHub, SwapHeadResult};
use std::sync::Arc;

#[tokio::test]
async fn local_backend_exposes_compare_and_swap_heads() {
    let data = tempfile::tempdir().expect("create hub data");
    let hub = LocalHub::open(data.path().to_path_buf(), None)
        .await
        .expect("open local hub");
    let first = ApiClient::local(Arc::clone(&hub), None);
    let second = ApiClient::local(hub, None);
    assert_eq!(first.get_head("workspace").await.unwrap(), None);

    let first_bytes = b"first snapshot object".to_vec();
    let second_bytes = b"second snapshot object".to_vec();
    let first_id = feanorfs_common::hash_bytes(&first_bytes);
    let second_id = feanorfs_common::hash_bytes(&second_bytes);
    first
        .upload_object("workspace", &first_id, first_bytes)
        .await
        .unwrap();
    first
        .upload_manifest("workspace", &first_id, std::slice::from_ref(&first_id))
        .await
        .unwrap();
    second
        .upload_object("workspace", &second_id, second_bytes)
        .await
        .unwrap();
    second
        .upload_manifest("workspace", &second_id, std::slice::from_ref(&second_id))
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        first.swap_head("workspace", None, &first_id),
        second.swap_head("workspace", None, &second_id),
    );
    let results = [left.unwrap(), right.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == SwapHeadResult::Swapped)
            .count(),
        1
    );
    let current = first.get_head("workspace").await.unwrap().unwrap();
    assert!(results.contains(&SwapHeadResult::Conflict(Some(current.clone()))));
    assert!(current == first_id || current == second_id);
    let loser_id = if current == first_id {
        second_id
    } else {
        first_id
    };
    assert_eq!(
        second
            .swap_head("workspace", Some(&current), &loser_id)
            .await
            .unwrap(),
        SwapHeadResult::Swapped
    );
    assert_eq!(first.get_head("workspace").await.unwrap(), Some(loser_id));
}
