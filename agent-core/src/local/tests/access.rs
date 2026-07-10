use super::new_db;

#[tokio::test]
async fn access_log_records_and_queries_siblings() {
    let (_dir, db) = new_db().await;
    db.record_access_pair("a.rs", "b.rs", 1.0)
        .await
        .expect("record");
    db.record_access_pair("a.rs", "c.rs", 2.0)
        .await
        .expect("record");

    let siblings = db.get_predictive_siblings("a.rs", 10).await.expect("query");
    assert_eq!(siblings.len(), 2);
    assert_eq!(siblings[0].0, "c.rs");
    assert!((siblings[0].1 - 2.0).abs() < 0.001);
    assert_eq!(siblings[1].0, "b.rs");
}

#[tokio::test]
async fn access_log_accumulates_weight_on_repeated_record() {
    let (_dir, db) = new_db().await;
    db.record_access_pair("x", "y", 1.0).await.expect("record");
    db.record_access_pair("x", "y", 0.5).await.expect("record");

    let siblings = db.get_predictive_siblings("x", 10).await.expect("query");
    assert_eq!(siblings.len(), 1);
    assert!((siblings[0].1 - 1.5).abs() < 0.001);
}

#[tokio::test]
async fn access_log_decay_reduces_all_weights() {
    let (_dir, db) = new_db().await;
    db.record_access_pair("a", "b", 1.0).await.expect("record");
    db.record_access_pair("c", "d", 2.0).await.expect("record");

    db.decay_access_log(0.5).await.expect("decay");

    let siblings = db.get_predictive_siblings("c", 10).await.expect("query");
    assert!((siblings[0].1 - 1.0).abs() < 0.001);
}

#[tokio::test]
async fn access_log_rejects_non_finite_weight_delta() {
    let (_dir, db) = new_db().await;
    let err = db
        .record_access_pair("x", "y", f64::NAN)
        .await
        .expect_err("NaN should be rejected");
    assert!(err.to_string().contains("non-finite weight delta"));

    let err = db
        .record_access_pair("x", "y", f64::INFINITY)
        .await
        .expect_err("Inf should be rejected");
    assert!(err.to_string().contains("non-finite weight delta"));
}

#[tokio::test]
async fn access_log_threshold_pruning_removes_tiny_weights() {
    let (_dir, db) = new_db().await;
    db.record_access_pair("a", "b", 1.0).await.expect("valid");
    db.record_access_pair("a", "c", 0.0001).await.expect("tiny");
    db.record_access_pair("a", "d", -0.0005)
        .await
        .expect("negative tiny");

    let siblings = db.get_predictive_siblings("a", 10).await.expect("query");
    assert_eq!(siblings.len(), 1);
    assert_eq!(siblings[0].0, "b");
}

#[tokio::test]
async fn access_log_eviction_stays_within_cap() {
    let (_dir, db) = new_db().await;
    for index in 0_u32..50 {
        db.record_access_pair(
            "x",
            &format!("sibling_{index:05}"),
            f64::from(index % 20) + 0.01,
        )
        .await
        .expect("record");
    }

    let siblings = db.get_predictive_siblings("x", 100).await.expect("query");
    assert!(siblings.len() <= crate::state::ACCESS_LOG_MAX_ENTRIES);
}

#[tokio::test]
async fn access_log_decay_prunes_below_min_weight() {
    let (_dir, db) = new_db().await;
    db.record_access_pair("a", "b", 0.01).await.expect("record");

    db.decay_access_log(0.01).await.expect("decay");

    let siblings = db.get_predictive_siblings("a", 10).await.expect("query");
    assert!(siblings.is_empty());
}

#[tokio::test]
async fn decay_access_log_rejects_non_finite_factor() {
    let (_dir, db) = new_db().await;
    let err = db
        .decay_access_log(f64::NAN)
        .await
        .expect_err("NaN factor should fail");
    assert!(err.to_string().contains("non-finite decay factor"));

    let err = db
        .decay_access_log(f64::INFINITY)
        .await
        .expect_err("Inf factor should fail");
    assert!(err.to_string().contains("non-finite decay factor"));
}

#[tokio::test]
async fn record_access_pair_rejects_weight_overflow() {
    let (_dir, db) = new_db().await;
    db.record_access_pair("a", "b", f64::MAX / 2.0)
        .await
        .expect("first record");
    let err = db
        .record_access_pair("a", "b", f64::MAX / 2.0)
        .await
        .expect_err("overflow should fail");
    assert!(err.to_string().contains("overflow"));
}
