use feanorfs_common::{
    diff_trees, flat_to_tree, flat_to_tree_with_conflicts, hash_bytes, tree_to_flat,
    ConcurrentEdit, ConflictModes, FileState, Snapshot, Tree, TreeChangeKind, TreeEntry,
    TreeEntryKind, EXECUTABLE_MODE,
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

const PATH_BUDGET_DAG_CHAIN_DEPTH: usize = 15;
const PATH_BUDGET_DAG_WIDTH: usize = 133;
const PATH_BUDGET_DAG_BRANCH: usize = 100;
const PATH_BUDGET_DAG_MIDDLE: usize = 200;
const PATH_BUDGET_DAG_EMPTY: usize = 300;

fn synthetic_tree_id(value: usize) -> String {
    format!("{value:064x}")
}

fn path_budget_dag_tree(id: &str) -> anyhow::Result<Tree> {
    let value = usize::from_str_radix(id, 16)?;
    if (1..=PATH_BUDGET_DAG_CHAIN_DEPTH).contains(&value) {
        let child = if value == PATH_BUDGET_DAG_CHAIN_DEPTH {
            PATH_BUDGET_DAG_BRANCH
        } else {
            value + 1
        };
        return Ok(Tree {
            entries: vec![TreeEntry {
                name: "p".repeat(255),
                kind: TreeEntryKind::Dir,
                hash: synthetic_tree_id(child),
                size: 0,
                mode: 0,
            }],
        });
    }
    let (prefix, child) = match value {
        PATH_BUDGET_DAG_BRANCH => ("r", PATH_BUDGET_DAG_MIDDLE),
        PATH_BUDGET_DAG_MIDDLE => ("m", PATH_BUDGET_DAG_EMPTY),
        PATH_BUDGET_DAG_EMPTY => {
            return Ok(Tree {
                entries: vec![TreeEntry {
                    name: "leaf".to_string(),
                    kind: TreeEntryKind::File,
                    hash: hash_bytes(b"path-budget-leaf"),
                    size: 1,
                    mode: 0,
                }],
            });
        }
        _ => anyhow::bail!("unknown synthetic tree {id}"),
    };
    Ok(Tree {
        entries: (0..PATH_BUDGET_DAG_WIDTH)
            .map(|index| TreeEntry {
                name: format!("{prefix}{index:03}"),
                kind: TreeEntryKind::Dir,
                hash: synthetic_tree_id(child),
                size: 0,
                mode: 0,
            })
            .collect(),
    })
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
                modes: ConflictModes::default(),
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
            modes: ConflictModes::default(),
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

#[test]
fn canonical_tree_rejects_unsafe_and_portably_colliding_siblings() {
    let hash = hash_bytes(b"blob");
    for name in ["CON", ".git", "bad:name", "trail.", "line\nbreak"] {
        let tree = Tree {
            entries: vec![TreeEntry {
                name: name.to_string(),
                kind: TreeEntryKind::File,
                hash: hash.clone(),
                size: 1,
                mode: 0,
            }],
        };
        assert!(
            Tree::from_canonical_bytes(&tree.to_canonical_bytes()).is_err(),
            "unsafe component {name:?} was accepted"
        );
        assert!(tree.validate().is_err());
    }

    let collision = Tree {
        entries: vec![
            TreeEntry {
                name: "Foo".into(),
                kind: TreeEntryKind::File,
                hash: hash.clone(),
                size: 1,
                mode: 0,
            },
            TreeEntry {
                name: "foo".into(),
                kind: TreeEntryKind::File,
                hash,
                size: 1,
                mode: 0,
            },
        ],
    };
    assert!(Tree::from_canonical_bytes(&collision.to_canonical_bytes()).is_err());
    assert!(collision.validate().is_err());

    let flat = HashMap::from([
        ("Foo".to_string(), file("Foo", "one", 1, 0)),
        ("foo".to_string(), file("foo", "two", 1, 0)),
    ]);
    assert!(flat_to_tree(&flat).is_err());
}

#[test]
fn canonical_snapshot_rejects_duplicate_or_excessive_parents() {
    let parent = hash_bytes(b"parent");
    let duplicate = Snapshot {
        root: hash_bytes(b"root"),
        parents: vec![parent.clone(), parent],
        author: "test".into(),
        created_at_ms: 0,
        message: None,
    };
    assert!(duplicate.validate().is_err());
    assert!(Snapshot::from_canonical_bytes(&duplicate.to_canonical_bytes()).is_err());

    let excessive = Snapshot {
        root: hash_bytes(b"root"),
        parents: vec![hash_bytes(b"a"), hash_bytes(b"b"), hash_bytes(b"c")],
        author: "test".into(),
        created_at_ms: 0,
        message: None,
    };
    assert!(excessive.validate().is_err());
    assert!(Snapshot::from_canonical_bytes(&excessive.to_canonical_bytes()).is_err());
}

#[test]
fn deep_graphs_fail_boundedly_and_reused_subtrees_are_budgeted() {
    let too_deep = vec!["a"; feanorfs_common::MAX_TREE_DEPTH + 1].join("/");
    let files = HashMap::from([(too_deep.clone(), file(&too_deep, "deep", 1, 0))]);
    assert!(flat_to_tree(&files).is_err());

    let root = format!("{:064x}", 0);
    let result = tree_to_flat(&root, |id| {
        let index = usize::from_str_radix(id, 16)?;
        Ok(Tree {
            entries: vec![TreeEntry {
                name: "a".into(),
                kind: if index < 10_000 {
                    TreeEntryKind::Dir
                } else {
                    TreeEntryKind::File
                },
                hash: if index < 10_000 {
                    format!("{:064x}", index + 1)
                } else {
                    hash_bytes(b"leaf")
                },
                size: 1,
                mode: 0,
            }],
        })
    });
    assert!(result.is_err(), "deep remote graph must return an error");

    let child = hash_bytes(b"child-tree");
    let reused = tree_to_flat(&hash_bytes(b"root-tree"), |id| {
        if id == child {
            Ok(Tree {
                entries: vec![TreeEntry {
                    name: "file".into(),
                    kind: TreeEntryKind::File,
                    hash: hash_bytes(b"file"),
                    size: 1,
                    mode: 0,
                }],
            })
        } else {
            Ok(Tree {
                entries: vec![
                    TreeEntry {
                        name: "left".into(),
                        kind: TreeEntryKind::Dir,
                        hash: child.clone(),
                        size: 0,
                        mode: 0,
                    },
                    TreeEntry {
                        name: "right".into(),
                        kind: TreeEntryKind::Dir,
                        hash: child.clone(),
                        size: 0,
                        mode: 0,
                    },
                ],
            })
        }
    });
    let reused = reused.expect("non-ancestor subtree reuse is allowed under traversal budgets");
    assert!(reused.contains_key("left/file"));
    assert!(reused.contains_key("right/file"));
}

#[test]
fn diff_allows_reused_subtree_outside_active_ancestry() {
    let child = Tree {
        entries: vec![TreeEntry {
            name: "file".into(),
            kind: TreeEntryKind::File,
            hash: hash_bytes(b"blob"),
            size: 1,
            mode: 0,
        }],
    };
    let child_id = child.id();
    let before = Tree::default();
    let before_id = before.id();
    let after = Tree {
        entries: vec![
            TreeEntry {
                name: "left".into(),
                kind: TreeEntryKind::Dir,
                hash: child_id.clone(),
                size: 0,
                mode: 0,
            },
            TreeEntry {
                name: "right".into(),
                kind: TreeEntryKind::Dir,
                hash: child_id.clone(),
                size: 0,
                mode: 0,
            },
        ],
    };
    let after_id = after.id();
    let trees = HashMap::from([
        (before_id.clone(), before),
        (after_id.clone(), after),
        (child_id, child),
    ]);

    let changes = diff_trees(&before_id, &after_id, |hash| {
        trees
            .get(hash)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing reused diff tree {hash}"))
    })
    .expect("subtree reuse outside active ancestry is valid");
    assert_eq!(
        changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        ["left/file", "right/file"]
    );
    assert!(changes
        .iter()
        .all(|change| change.kind == TreeChangeKind::Added));
}

#[test]
fn max_depth_tree_operations_do_not_depend_on_recursive_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let path = vec!["a"; feanorfs_common::MAX_TREE_DEPTH].join("/");
            let before_files = HashMap::from([(path.clone(), file(&path, "before", 1, 0))]);
            let after_files =
                HashMap::from([(path.clone(), file(&path, "after", 2, EXECUTABLE_MODE))]);
            let before = flat_to_tree(&before_files).expect("build max-depth before tree");
            let after = flat_to_tree(&after_files).expect("build max-depth after tree");

            let flat = tree_to_flat(&before.root, |hash| {
                before
                    .trees
                    .get(hash)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing max-depth tree {hash}"))
            })
            .expect("flatten max-depth tree");
            assert_eq!(flat.len(), 1);
            assert_eq!(flat[&path].hash, before_files[&path].hash);

            let changes = diff_trees(&before.root, &after.root, |hash| {
                before
                    .trees
                    .get(hash)
                    .or_else(|| after.trees.get(hash))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing max-depth diff tree {hash}"))
            })
            .expect("diff max-depth trees");
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].path, path);
            assert_eq!(changes[0].kind, TreeChangeKind::Modified);
        })
        .expect("spawn small-stack tree test")
        .join()
        .expect("small-stack tree test must not overflow");
}

#[test]
fn long_prefix_dag_is_stopped_by_constructed_path_budget() {
    let root = synthetic_tree_id(1);
    let mut flatten_fetches = 0_usize;
    let flatten_error = tree_to_flat(&root, |id| {
        flatten_fetches += 1;
        path_budget_dag_tree(id)
    })
    .expect_err("long-prefix DAG must exhaust constructed-path budget");
    assert!(
        flatten_error
            .to_string()
            .contains("aggregate path-byte limit"),
        "unexpected flatten error: {flatten_error:#}"
    );
    assert!(flatten_fetches < 18_000);

    let after = synthetic_tree_id(400);
    let mut diff_fetches = 0_usize;
    let diff_error = diff_trees(&root, &after, |id| {
        diff_fetches += 1;
        if id == after {
            Ok(Tree::default())
        } else {
            path_budget_dag_tree(id)
        }
    })
    .expect_err("long-prefix DAG diff must exhaust constructed-path budget");
    assert!(
        diff_error.to_string().contains("aggregate path-byte limit"),
        "unexpected diff error: {diff_error:#}"
    );
    assert!(diff_fetches < 18_000);
}

#[test]
fn directory_file_diff_expands_both_structural_sides() {
    let before = flat_to_tree(&HashMap::from([(
        "node/child".to_string(),
        file("node/child", "before", 1, 0),
    )]))
    .unwrap();
    let after = flat_to_tree(&HashMap::from([(
        "node".to_string(),
        file("node", "after", 1, 0),
    )]))
    .unwrap();
    let changes = diff_trees(&before.root, &after.root, |hash| {
        before
            .trees
            .get(hash)
            .or_else(|| after.trees.get(hash))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing tree"))
    })
    .unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].path, "node");
    assert_eq!(changes[0].kind, TreeChangeKind::Added);
    assert_eq!(changes[1].path, "node/child");
    assert_eq!(changes[1].kind, TreeChangeKind::Deleted);
}

#[test]
fn diff_keeps_before_and_after_cycle_ancestry_independent() {
    let leaf = Tree {
        entries: vec![TreeEntry {
            name: "file".into(),
            kind: TreeEntryKind::File,
            hash: hash_bytes(b"blob"),
            size: 1,
            mode: 0,
        }],
    };
    let leaf_id = leaf.id();
    let before = Tree {
        entries: vec![TreeEntry {
            name: "x".into(),
            kind: TreeEntryKind::Dir,
            hash: leaf_id.clone(),
            size: 0,
            mode: 0,
        }],
    };
    let before_id = before.id();
    let after = Tree {
        entries: vec![TreeEntry {
            name: "x".into(),
            kind: TreeEntryKind::Dir,
            hash: before_id.clone(),
            size: 0,
            mode: 0,
        }],
    };
    let after_id = after.id();
    let trees = HashMap::from([
        (leaf_id, leaf),
        (before_id.clone(), before),
        (after_id.clone(), after),
    ]);
    let changes = diff_trees(&before_id, &after_id, |hash| {
        trees
            .get(hash)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing tree"))
    })
    .expect("cross-version ancestry is not a cycle");
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].path, "x/file");
    assert_eq!(changes[0].kind, TreeChangeKind::Deleted);
    assert_eq!(changes[1].path, "x/x/file");
    assert_eq!(changes[1].kind, TreeChangeKind::Added);
}

#[test]
fn executable_conflict_modes_use_ftr2_and_roundtrip_every_leg() {
    let same_hash = hash_bytes(b"same bytes");
    let base = FileState {
        path: "run.sh".into(),
        hash: same_hash.clone(),
        size: 10,
        mtime: 0,
        deleted: false,
        mode: 0,
    };
    let ours = FileState {
        mode: EXECUTABLE_MODE,
        ..base.clone()
    };
    let theirs = FileState {
        hash: hash_bytes(b"visible nonexec"),
        ..base.clone()
    };
    let conflict = ConcurrentEdit::new(
        "run.sh".into(),
        Some(base.clone()),
        Some(ours.clone()),
        Some(theirs.clone()),
    );
    let bundle = flat_to_tree_with_conflicts(&HashMap::new(), &[conflict]).unwrap();
    let tree = bundle.trees.get(&bundle.root).unwrap();
    let bytes = tree.to_canonical_bytes();
    assert_eq!(&bytes[..4], b"FTR2");
    let decoded = Tree::from_canonical_bytes(&bytes).unwrap();
    assert_eq!(decoded, *tree);
    let entry = &decoded.entries[0];
    assert_eq!(entry.mode, 0, "visible theirs leg is non-executable");
    assert_eq!(
        entry.kind,
        TreeEntryKind::Conflict {
            base: Some(base.hash),
            ours: Some(ours.hash),
            theirs: Some(theirs.hash),
            modes: ConflictModes {
                base: 0,
                ours: EXECUTABLE_MODE,
                theirs: 0,
            },
        }
    );
    let flat = tree_to_flat(&bundle.root, |hash| {
        bundle
            .trees
            .get(hash)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing tree"))
    })
    .unwrap();
    assert_eq!(flat["run.sh"].mode, 0);
}

#[test]
fn legacy_zero_mode_conflicts_remain_byte_exact_ftr1() {
    let conflict = Tree {
        entries: vec![TreeEntry {
            name: "file".into(),
            kind: TreeEntryKind::Conflict {
                base: Some(hash_bytes(b"base")),
                ours: Some(hash_bytes(b"ours")),
                theirs: None,
                modes: ConflictModes::default(),
            },
            hash: hash_bytes(b"ours"),
            size: 4,
            mode: 0,
        }],
    };
    let bytes = conflict.to_canonical_bytes();
    assert_eq!(&bytes[..4], b"FTR1");
    let decoded = Tree::from_canonical_bytes(&bytes).unwrap();
    assert_eq!(decoded.to_canonical_bytes(), bytes);
    assert_eq!(decoded.id(), conflict.id());

    let executable_file = Tree {
        entries: vec![TreeEntry {
            name: "run".into(),
            kind: TreeEntryKind::File,
            hash: hash_bytes(b"run"),
            size: 3,
            mode: EXECUTABLE_MODE,
        }],
    };
    assert_eq!(&executable_file.to_canonical_bytes()[..4], b"FTR1");
}

#[test]
fn conflict_mode_validation_fails_closed() {
    let hash = hash_bytes(b"leg");
    let invalid_cases = [
        TreeEntry {
            name: "run".into(),
            kind: TreeEntryKind::Conflict {
                base: Some(hash.clone()),
                ours: None,
                theirs: None,
                modes: ConflictModes {
                    base: 2,
                    ..ConflictModes::default()
                },
            },
            hash: hash.clone(),
            size: 1,
            mode: 2,
        },
        TreeEntry {
            name: "run".into(),
            kind: TreeEntryKind::Conflict {
                base: Some(hash.clone()),
                ours: None,
                theirs: None,
                modes: ConflictModes {
                    ours: EXECUTABLE_MODE,
                    ..ConflictModes::default()
                },
            },
            hash: hash.clone(),
            size: 1,
            mode: 0,
        },
        TreeEntry {
            name: "run".into(),
            kind: TreeEntryKind::Conflict {
                base: Some(hash.clone()),
                ours: None,
                theirs: None,
                modes: ConflictModes {
                    base: EXECUTABLE_MODE,
                    ..ConflictModes::default()
                },
            },
            hash,
            size: 1,
            mode: 0,
        },
    ];
    for entry in invalid_cases {
        let tree = Tree {
            entries: vec![entry],
        };
        assert!(tree.validate().is_err());
        assert!(Tree::from_canonical_bytes(&tree.to_canonical_bytes()).is_err());
    }
}

#[test]
fn conflict_modes_serde_defaults_and_omits_zero_legacy_shape() {
    let legacy = serde_json::json!({
        "conflict": {
            "base": hash_bytes(b"base"),
            "ours": hash_bytes(b"ours"),
            "theirs": null
        }
    });
    let decoded: TreeEntryKind = serde_json::from_value(legacy.clone()).unwrap();
    assert!(matches!(
        decoded,
        TreeEntryKind::Conflict { modes, .. } if modes.is_zero()
    ));
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);
}

#[test]
fn flat_conversion_rejects_mismatched_embedded_paths() {
    let files = HashMap::from([(
        "map-key.txt".to_string(),
        file("different.txt", "content", 7, 0),
    )]);
    assert!(flat_to_tree(&files)
        .unwrap_err()
        .to_string()
        .contains("does not match embedded path"));
}

#[test]
fn conflict_conversion_rejects_mismatched_leg_paths() {
    let conflict = ConcurrentEdit {
        path: "conflict.txt".to_string(),
        base: Some(file("other.txt", "base", 4, 0)),
        ours: None,
        theirs: None,
        original_file: None,
        local_file: None,
        cloud_file: None,
        kind: None,
        local_available: false,
        cloud_available: false,
        is_binary: false,
        hint: None,
        proposed_file: None,
        proposal_clean: None,
    };
    assert!(flat_to_tree_with_conflicts(&HashMap::new(), &[conflict])
        .unwrap_err()
        .to_string()
        .contains("does not match leg path"));
}

#[test]
fn canonical_directories_require_zero_size() {
    let tree = Tree {
        entries: vec![TreeEntry {
            name: "dir".to_string(),
            kind: TreeEntryKind::Dir,
            hash: hash_bytes(b"child"),
            size: 1,
            mode: 0,
        }],
    };
    assert!(tree
        .validate()
        .unwrap_err()
        .to_string()
        .contains("zero size"));
    assert!(Tree::from_canonical_bytes(&tree.to_canonical_bytes()).is_err());
}

#[test]
fn non_root_empty_directories_are_rejected_as_semantically_invisible() {
    let empty = Tree::default();
    let empty_id = empty.id();
    let root = Tree {
        entries: vec![TreeEntry {
            name: "empty".to_string(),
            kind: TreeEntryKind::Dir,
            hash: empty_id.clone(),
            size: 0,
            mode: 0,
        }],
    };
    let root_id = root.id();
    let objects = HashMap::from([(root_id.clone(), root.clone()), (empty_id, empty)]);
    let error = tree_to_flat(&root_id, |hash| {
        objects
            .get(hash)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing tree {hash}"))
    })
    .unwrap_err();
    assert!(error.to_string().contains("non-root empty directory"));

    let before = Tree::default();
    let before_id = before.id();
    let mut diff_objects = objects;
    diff_objects.insert(before_id.clone(), before);
    let error = diff_trees(&before_id, &root_id, |hash| {
        diff_objects
            .get(hash)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing tree {hash}"))
    })
    .unwrap_err();
    assert!(error.to_string().contains("non-root empty directory"));
}
