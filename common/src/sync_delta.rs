use crate::{is_safe_rel_path, FileState, SyncResponse};
use std::collections::HashMap;

/// Read-only LWW sync delta: compares client metadata against server metadata.
pub fn compute_sync_delta(client_files: &[FileState], server_files: &[FileState]) -> SyncResponse {
    let server_map: HashMap<String, FileState> = server_files
        .iter()
        .filter(|file| is_safe_rel_path(&file.path))
        .map(|file| (file.path.clone(), file.clone()))
        .collect();

    let mut upload_required = Vec::new();
    let mut download_required = Vec::new();
    let mut delete_local = Vec::new();

    let client_map: HashMap<String, FileState> = client_files
        .iter()
        .filter_map(|f| {
            if is_safe_rel_path(&f.path) {
                Some((f.path.clone(), f.clone()))
            } else {
                None
            }
        })
        .collect();

    for (path, client_file) in &client_map {
        if let Some(server_file) = server_map.get(path) {
            if client_file.mtime > server_file.mtime {
                upload_required.push(path.clone());
            } else if server_file.mtime > client_file.mtime {
                if server_file.deleted {
                    delete_local.push(path.clone());
                } else {
                    download_required.push(server_file.clone());
                }
            } else if client_file.deleted != server_file.deleted
                || (!client_file.deleted
                    && (client_file.hash != server_file.hash
                        || client_file.mode != server_file.mode))
            {
                // Equal mtimes are ambiguous across filesystems. Preserve the
                // established client-wins tie break for every semantic state,
                // including deletes and executable intent, so the pair cannot
                // remain permanently divergent.
                upload_required.push(path.clone());
            }
        } else {
            upload_required.push(path.clone());
        }
    }

    for (path, server_file) in &server_map {
        if !client_map.contains_key(path) && !server_file.deleted {
            download_required.push(server_file.clone());
        }
    }

    SyncResponse {
        upload_required,
        download_required,
        delete_local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, mtime: i64, hash: &str) -> FileState {
        FileState {
            path: path.into(),
            hash: hash.into(),
            size: 10,
            mtime,
            deleted: false,
            mode: 0,
        }
    }

    fn deleted(path: &str, mtime: i64) -> FileState {
        FileState {
            path: path.into(),
            hash: "aa".repeat(32),
            size: 0,
            mtime,
            deleted: true,
            mode: 0,
        }
    }

    #[test]
    fn client_newer_uploads() {
        let resp = compute_sync_delta(
            &[file("a.txt", 200, "hash_a")],
            &[file("a.txt", 100, "hash_b")],
        );
        assert_eq!(resp.upload_required, vec!["a.txt"]);
        assert!(resp.download_required.is_empty());
    }

    #[test]
    fn server_newer_downloads() {
        let resp = compute_sync_delta(
            &[file("a.txt", 100, "hash_a")],
            &[file("a.txt", 200, "hash_b")],
        );
        assert_eq!(resp.download_required.len(), 1);
        assert_eq!(resp.download_required[0].path, "a.txt");
    }

    #[test]
    fn server_deleted_marks_delete_local() {
        let resp = compute_sync_delta(
            &[file("gone.txt", 100, "hash_a")],
            &[deleted("gone.txt", 200)],
        );
        assert_eq!(resp.delete_local, vec!["gone.txt"]);
    }

    #[test]
    fn equal_mtime_hash_mismatch_uploads() {
        let resp = compute_sync_delta(
            &[file("a.txt", 100, "hash_a")],
            &[file("a.txt", 100, "hash_b")],
        );
        assert_eq!(resp.upload_required, vec!["a.txt"]);
    }

    #[test]
    fn equal_mtime_client_delete_uploads() {
        let resp = compute_sync_delta(
            &[deleted("gone.txt", 100)],
            &[file("gone.txt", 100, "hash_a")],
        );
        assert_eq!(resp.upload_required, vec!["gone.txt"]);
    }

    #[test]
    fn equal_mtime_live_client_over_same_hash_server_tombstone_uploads() {
        let client = file("restored.txt", 100, "same_hash");
        let mut server = deleted("restored.txt", 100);
        server.hash = "same_hash".into();
        let resp = compute_sync_delta(&[client], &[server]);
        assert_eq!(resp.upload_required, vec!["restored.txt"]);
    }

    #[test]
    fn equal_mtime_both_deleted_is_already_converged() {
        let mut client = deleted("gone.txt", 100);
        client.hash = "client_tombstone".into();
        client.mode = crate::EXECUTABLE_MODE;
        let mut server = deleted("gone.txt", 100);
        server.hash = "server_tombstone".into();
        let resp = compute_sync_delta(&[client], &[server]);
        assert!(resp.upload_required.is_empty());
        assert!(resp.download_required.is_empty());
        assert!(resp.delete_local.is_empty());
    }

    #[test]
    fn equal_mtime_executable_intent_uploads() {
        let mut client = file("script.sh", 100, "same_hash");
        client.mode = crate::EXECUTABLE_MODE;
        let resp = compute_sync_delta(&[client], &[file("script.sh", 100, "same_hash")]);
        assert_eq!(resp.upload_required, vec!["script.sh"]);
    }

    #[test]
    fn unsafe_server_paths_are_ignored() {
        let resp = compute_sync_delta(&[], &[file("../escape", 100, "hash")]);
        assert!(resp.download_required.is_empty());
    }

    #[test]
    fn server_only_path_downloads() {
        let resp = compute_sync_delta(&[], &[file("remote.txt", 50, "hash_r")]);
        assert_eq!(resp.download_required.len(), 1);
        assert_eq!(resp.download_required[0].path, "remote.txt");
    }

    #[test]
    fn client_only_path_uploads() {
        let resp = compute_sync_delta(&[file("local.txt", 50, "hash_l")], &[]);
        assert_eq!(resp.upload_required, vec!["local.txt"]);
    }

    #[test]
    fn unsafe_client_paths_ignored() {
        let mut bad = file("../etc/passwd", 1, "x");
        bad.path = "../etc/passwd".into();
        let resp = compute_sync_delta(&[bad], &[]);
        assert!(resp.upload_required.is_empty());
    }
}
