use super::super::{AccessEntryV1, LocalStateV1, ACCESS_LOG_MAX_ENTRIES, CURRENT_SCHEMA_VERSION};
use super::{cache_entry, empty_state};

#[test]
fn serde_roundtrip_is_deterministic() {
    let mut state = empty_state();
    state
        .local_files
        .insert("a.txt".into(), cache_entry("a", 100));
    state
        .local_files
        .insert("b.txt".into(), cache_entry("b", 200));

    let first = state.to_json().expect("serialize");
    let second = state.to_json().expect("serialize again");
    assert_eq!(first, second);
}

#[test]
fn reject_future_schema_version() {
    let error = LocalStateV1::from_json(r#"{"schema_version": 999, "local_files": {}}"#)
        .expect_err("future version should fail");
    assert!(error.to_string().contains("newer than supported"));
}

#[test]
fn reject_zero_schema_version() {
    let error = LocalStateV1::from_json(r#"{"schema_version": 0, "local_files": {}}"#)
        .expect_err("zero version should fail");
    assert!(error.to_string().contains("invalid schema version 0"));
}

#[test]
fn accept_valid_empty_state() {
    let state = LocalStateV1::from_json(r#"{"schema_version": 1, "local_files": {}}"#)
        .expect("valid state should parse");
    assert_eq!(state.schema_version, 1);
    assert!(state.local_files.is_empty());
}

#[test]
fn reject_malformed_json() {
    let error = LocalStateV1::from_json("not json").expect_err("malformed should fail");
    assert!(error.to_string().contains("parse local state JSON"));
}

#[test]
fn reject_corrupt_schema_version() {
    let error = LocalStateV1::from_json(r#"{"schema_version": "v1", "local_files": {}}"#)
        .expect_err("corrupt version should fail");
    let message = error.to_string();
    assert!(
        message.contains("invalid schema version 0") || message.contains("deserialize local state")
    );
}

#[test]
fn default_state_has_current_version() {
    assert_eq!(empty_state().schema_version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn prune_access_log_truncates_deterministically() {
    let total = ACCESS_LOG_MAX_ENTRIES + 3;
    let build_state = || {
        let mut state = empty_state();
        for index in 0..total {
            state.file_access_log.push(AccessEntryV1 {
                path: "x".into(),
                sibling_path: format!("sibling_{index:05}"),
                weight: f64::from(u32::try_from(index % 50).expect("weight fits u32")) + 0.01,
                updated_at: i64::try_from(total - index).expect("timestamp fits i64"),
            });
        }
        state
    };
    let mut first = build_state();
    let mut second = build_state();

    first.prune_access_log();
    second.prune_access_log();

    assert_eq!(first.file_access_log.len(), ACCESS_LOG_MAX_ENTRIES);
    assert_eq!(
        first.to_json().expect("serialize first"),
        second.to_json().expect("serialize second")
    );
}

#[test]
fn prune_access_log_removes_below_min_weight() {
    let mut state = empty_state();
    for (sibling_path, weight, updated_at) in [("b", 0.0005, 1), ("c", -0.0001, 2), ("d", 1.0, 3)] {
        state.file_access_log.push(AccessEntryV1 {
            path: "a".into(),
            sibling_path: sibling_path.into(),
            weight,
            updated_at,
        });
    }

    state.prune_access_log();

    assert_eq!(state.file_access_log.len(), 1);
    assert_eq!(state.file_access_log[0].sibling_path, "d");
}

#[test]
fn from_json_rejects_non_finite_weights_via_direct_check() {
    let mut state = empty_state();
    state.file_access_log.push(AccessEntryV1 {
        path: "x".into(),
        sibling_path: "y".into(),
        weight: f64::INFINITY,
        updated_at: 1,
    });

    let json = state.to_json().expect("serialize");

    assert!(LocalStateV1::from_json(&json).is_err());
}
