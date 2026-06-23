use feanorfs_common::{FileState, SyncRequest, SyncResponse};
use serde_json::json;

fn sample_file_state(path: &str, hash: &str, size: u64, mtime: i64, deleted: bool) -> FileState {
    FileState {
        path: path.to_string(),
        hash: hash.to_string(),
        size,
        mtime,
        deleted,
    }
}

#[test]
fn sync_request_serializes_with_expected_field_names() {
    let request = SyncRequest {
        workspace_id: "my-workspace".to_string(),
        files: vec![sample_file_state(
            "src/main.rs",
            "abc123",
            1024,
            1719500000000,
            false,
        )],
    };

    let json = serde_json::to_value(&request).unwrap();

    assert_eq!(json["workspace_id"], "my-workspace");
    assert_eq!(json["files"][0]["path"], "src/main.rs");
    assert_eq!(json["files"][0]["hash"], "abc123");
    assert_eq!(json["files"][0]["size"], 1024);
    assert_eq!(json["files"][0]["mtime"], 1719500000000i64);
    assert_eq!(json["files"][0]["deleted"], false);
}

#[test]
fn sync_request_roundtrip_preserves_all_fields() {
    let original = SyncRequest {
        workspace_id: "roundtrip-test".to_string(),
        files: vec![
            sample_file_state("a.txt", "hash-a", 10, 1000, false),
            sample_file_state("b.txt", "hash-b", 20, 2000, true),
            sample_file_state("c.txt", "hash-c", 30, 3000, false),
        ],
    };

    let encoded = serde_json::to_string(&original).unwrap();
    let decoded: SyncRequest = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.workspace_id, original.workspace_id);
    assert_eq!(decoded.files.len(), original.files.len());
    for (a, b) in original.files.iter().zip(decoded.files.iter()) {
        assert_eq!(a.path, b.path);
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.size, b.size);
        assert_eq!(a.mtime, b.mtime);
        assert_eq!(a.deleted, b.deleted);
    }
}

#[test]
fn sync_request_with_empty_files_list_serializes() {
    let request = SyncRequest {
        workspace_id: "empty".to_string(),
        files: vec![],
    };

    let json = serde_json::to_string(&request).unwrap();
    let decoded: SyncRequest = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.workspace_id, "empty");
    assert!(decoded.files.is_empty());
}

#[test]
fn sync_response_roundtrip_preserves_all_buckets() {
    let original = SyncResponse {
        upload_required: vec!["new.txt".to_string(), "modified.txt".to_string()],
        download_required: vec![sample_file_state(
            "remote.txt",
            "remote-hash",
            4096,
            1719600000000,
            false,
        )],
        delete_local: vec!["gone.txt".to_string()],
    };

    let encoded = serde_json::to_string(&original).unwrap();
    let decoded: SyncResponse = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.upload_required, original.upload_required);
    assert_eq!(decoded.upload_required.len(), 2);

    assert_eq!(decoded.download_required.len(), 1);
    assert_eq!(decoded.download_required[0].path, "remote.txt");
    assert_eq!(decoded.download_required[0].hash, "remote-hash");
    assert_eq!(decoded.download_required[0].size, 4096);

    assert_eq!(decoded.delete_local, original.delete_local);
    assert_eq!(decoded.delete_local.len(), 1);
}

#[test]
fn sync_response_with_empty_buckets_serializes() {
    let response = SyncResponse {
        upload_required: vec![],
        download_required: vec![],
        delete_local: vec![],
    };

    let json = serde_json::to_string(&response).unwrap();
    let decoded: SyncResponse = serde_json::from_str(&json).unwrap();

    assert!(decoded.upload_required.is_empty());
    assert!(decoded.download_required.is_empty());
    assert!(decoded.delete_local.is_empty());
}

#[test]
fn sync_request_can_deserialize_from_raw_json() {
    let raw = json!({
        "workspace_id": "raw-test",
        "files": [
            {
                "path": "deeply/nested/file.rs",
                "hash": "feedface",
                "size": 2048,
                "mtime": 1719700000000_i64,
                "deleted": false
            }
        ]
    });

    let decoded: SyncRequest = serde_json::from_value(raw).unwrap();

    assert_eq!(decoded.workspace_id, "raw-test");
    assert_eq!(decoded.files.len(), 1);
    assert_eq!(decoded.files[0].path, "deeply/nested/file.rs");
    assert_eq!(decoded.files[0].hash, "feedface");
    assert_eq!(decoded.files[0].size, 2048);
    assert_eq!(decoded.files[0].mtime, 1719700000000);
    assert!(!decoded.files[0].deleted);
}

#[test]
fn sync_response_can_deserialize_from_raw_json() {
    let raw = json!({
        "upload_required": ["u1.txt", "u2.txt", "u3.txt"],
        "download_required": [
            {
                "path": "d1.txt",
                "hash": "d1hash",
                "size": 100,
                "mtime": 5000,
                "deleted": false
            }
        ],
        "delete_local": ["del.txt"]
    });

    let decoded: SyncResponse = serde_json::from_value(raw).unwrap();

    assert_eq!(decoded.upload_required, vec!["u1.txt", "u2.txt", "u3.txt"]);
    assert_eq!(decoded.download_required.len(), 1);
    assert_eq!(decoded.download_required[0].path, "d1.txt");
    assert_eq!(decoded.delete_local, vec!["del.txt"]);
}

#[test]
fn file_state_with_large_mtime_and_size_roundtrips() {
    let state = sample_file_state("big.bin", "large-hash", u64::MAX, i64::MAX, false);

    let json = serde_json::to_string(&state).unwrap();
    let decoded: FileState = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.size, u64::MAX);
    assert_eq!(decoded.mtime, i64::MAX);
}
