use feanorfs_common::{
    diff_trees, flat_to_tree, flat_to_tree_with_conflicts, hash_bytes, tree_to_flat,
    ConcurrentEdit, FileState, Snapshot, Tree, TreeChangeKind, TreeEntry, TreeEntryKind,
    EXECUTABLE_MODE,
};
use std::collections::HashMap;

fn file(path: &str, hash: &str, size: u64, mode: u32) -> FileState {
    FileState {
        path: path.to_string(),
        hash: hash_bytes(hash.as_bytes()),
        size,
        mtime: 123,
        deleted: false,
        mode,
    }
}

#[test]
fn flat_tree_roundtrip_preserves_snapshot_identity() {
    let files = HashMap::from([
        ("README.md".to_string(), file("README.md", "readme", 12, 0)),
        (
            "bin/run.sh".to_string(),
            file("bin/run.sh", "runner", 34, EXECUTABLE_MODE),
        ),
        (
            "src/main.rs".to_string(),
            file("src/main.rs", "main", 56, 0),
        ),
    ]);

    let bundle = flat_to_tree(&files).expect("build canonical tree");
    let restored = tree_to_flat(&bundle.root, |hash| {
        bundle
            .trees
            .get(hash)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing tree {hash}"))
    })
    .expect("flatten canonical tree");

    assert_eq!(restored.len(), files.len());
    for (path, original) in files {
        let actual = restored.get(&path).expect("round-tripped path");
        assert_eq!(actual.path, original.path);
        assert_eq!(actual.hash, original.hash);
        assert_eq!(actual.size, original.size);
        assert_eq!(actual.mode, original.mode);
        assert!(!actual.deleted);
    }
}

#[test]
fn tree_hash_is_stable_under_flat_insertion_order() {
    let ordered = HashMap::from([
        ("a/one.txt".to_string(), file("a/one.txt", "one", 1, 0)),
        ("b/two.txt".to_string(), file("b/two.txt", "two", 2, 0)),
    ]);
    let reversed = HashMap::from([
        ("b/two.txt".to_string(), file("b/two.txt", "two", 2, 0)),
        ("a/one.txt".to_string(), file("a/one.txt", "one", 1, 0)),
    ]);

    let first = flat_to_tree(&ordered).expect("build first tree");
    let second = flat_to_tree(&reversed).expect("build second tree");

    assert_eq!(first.root, second.root);
    assert_eq!(first.trees, second.trees);
}

#[test]
fn canonical_tree_and_snapshot_bytes_roundtrip() {
    let bundle = flat_to_tree(&HashMap::from([(
        "file.txt".to_string(),
        file("file.txt", "blob", 4, 0),
    )]))
    .expect("build tree");
    let tree = bundle.trees.get(&bundle.root).expect("root tree");
    let tree_bytes = tree.to_canonical_bytes();
    assert_eq!(
        Tree::from_canonical_bytes(&tree_bytes).expect("decode tree"),
        *tree
    );

    let snapshot = Snapshot {
        root: bundle.root,
        parents: vec![hash_bytes(b"parent-a"), hash_bytes(b"parent-b")],
        author: "agent:test".to_string(),
        created_at_ms: 42,
        message: Some("land".to_string()),
    };
    let snapshot_bytes = snapshot.to_canonical_bytes();
    assert_eq!(
        Snapshot::from_canonical_bytes(&snapshot_bytes).expect("decode snapshot"),
        snapshot
    );
}

#[test]
fn conflict_hash_must_identify_visible_leg() {
    let base = hash_bytes(b"base");
    let ours = hash_bytes(b"ours");
    let theirs = hash_bytes(b"theirs");
    let invalid = Tree {
        entries: vec![TreeEntry {
            name: "conflicted.txt".to_string(),
            kind: TreeEntryKind::Conflict {
                base: Some(base),
                ours: Some(ours),
                theirs: Some(theirs),
            },
            hash: hash_bytes(b"unrelated"),
            size: 10,
            mode: 0,
        }],
    };

    assert!(Tree::from_canonical_bytes(&invalid.to_canonical_bytes()).is_err());
}

#[test]
fn flat_tree_overlay_encodes_edit_delete_conflict() {
    let base = file("src/lib.rs", "base", 10, 0);
    let ours = file("src/lib.rs", "ours", 12, 0);
    let conflict = ConcurrentEdit::new(
        "src/lib.rs".to_string(),
        Some(base.clone()),
        Some(ours.clone()),
        None,
    );
    let bundle =
        flat_to_tree_with_conflicts(&HashMap::new(), &[conflict]).expect("build conflict tree");
    let root = bundle.trees.get(&bundle.root).expect("root");
    let src = root.entries.first().expect("src directory");
    let child = bundle.trees.get(&src.hash).expect("src tree");
    let entry = child.entries.first().expect("conflict entry");

    assert_eq!(entry.hash, ours.hash);
    assert_eq!(entry.size, ours.size);
    assert_eq!(entry.mode, 0);
    assert_eq!(
        entry.kind,
        TreeEntryKind::Conflict {
            base: Some(base.hash),
            ours: Some(ours.hash),
            theirs: None,
        }
    );
}

#[test]
fn one_file_change_descends_only_into_changed_subtree() {
    let mut before = HashMap::new();
    for directory in 0..100 {
        for file_index in 0..100 {
            let path = format!("dir-{directory:03}/file-{file_index:03}.txt");
            before.insert(path.clone(), file(&path, &format!("h-{path}"), 1, 0));
        }
    }
    let mut after = before.clone();
    after.insert(
        "dir-042/file-007.txt".to_string(),
        file("dir-042/file-007.txt", "changed", 7, EXECUTABLE_MODE),
    );

    let before_bundle = flat_to_tree(&before).expect("build before tree");
    let after_bundle = flat_to_tree(&after).expect("build after tree");
    let mut fetches = 0_usize;
    let changes = diff_trees(&before_bundle.root, &after_bundle.root, |hash| {
        fetches += 1;
        before_bundle
            .trees
            .get(hash)
            .or_else(|| after_bundle.trees.get(hash))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing tree {hash}"))
    })
    .expect("diff trees");

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "dir-042/file-007.txt");
    assert_eq!(changes[0].kind, TreeChangeKind::Modified);
    assert_eq!(fetches, 4, "root pair plus changed directory pair");
}

#[test]
fn no_change_tree_diff_fetches_nothing() {
    let hash = "same-root";
    let mut fetches = 0;

    let changes = diff_trees(hash, hash, |_| {
        fetches += 1;
        Ok(Tree::default())
    })
    .expect("diff identical roots");

    assert!(changes.is_empty());
    assert_eq!(fetches, 0);
}
