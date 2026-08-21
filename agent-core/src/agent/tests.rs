use crate::paths::{agent_dir, agent_root, validate_name};

use super::runtime::open_agent_runtime;

#[test]
fn validate_name_accepts_simple_identifier() {
    assert!(validate_name("ci1").is_ok());
    assert!(validate_name("agent-foo").is_ok());
    assert!(validate_name("agent_foo").is_ok());
    assert!(validate_name("agent.foo").is_ok());
}

#[test]
fn validate_name_rejects_empty() {
    let error = validate_name("").expect_err("empty name should fail");
    assert!(error.to_string().contains("empty"));
}

#[test]
fn validate_name_rejects_forward_slash() {
    assert!(validate_name("a/b").is_err());
}

#[test]
fn validate_name_rejects_backslash() {
    assert!(validate_name(r"a\b").is_err());
}

#[test]
fn validate_name_rejects_dot() {
    assert!(validate_name(".").is_err());
}

#[test]
fn validate_name_rejects_dotdot() {
    assert!(validate_name("..").is_err());
}

#[test]
fn validate_name_rejects_control_chars() {
    assert!(validate_name("a\nb").is_err());
    assert!(validate_name("a\tb").is_err());
    assert!(validate_name("a\0b").is_err());
}

#[test]
fn validate_name_rejects_overlong_portable_component() {
    assert!(validate_name(&"a".repeat(feanorfs_common::AGENT_NAME_MAX_BYTES + 1)).is_err());
}

#[test]
fn agent_root_rejects_names_that_escape_the_agent_directory() {
    let base = tempfile::tempdir().unwrap();
    for name in ["../outside", "/tmp/outside", "nested/name", r"nested\name"] {
        assert!(crate::paths::agent_root(base.path(), name).is_err());
    }
}

#[tokio::test]
async fn configured_runner_blocks_agent_clean_and_spawn_replace_before_mutation() {
    let base = tempfile::tempdir().unwrap();
    let name = "runner-owned";
    crate::local::save_config(
        base.path(),
        &crate::local::Config {
            server_url: "http://127.0.0.1:1".into(),
            workspace_id: "runner-clean-test".into(),
            encryption_password: Some("e".repeat(64)),
            server_password: None,
            tls_ca_pem: None,
            format_version: 3,
            hub_local: false,
            relay: None,
        },
    )
    .unwrap();
    let root = agent_root(base.path(), name).unwrap();
    std::fs::create_dir_all(root.join("worktree")).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::write(root.join("state/base-snapshot"), "f".repeat(64)).unwrap();
    let program = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    super::RunnerStore::configure(
        base.path(),
        name,
        &program,
        Vec::new(),
        3600,
        &"a".repeat(64),
    )
    .unwrap();

    let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
    let db = crate::local::ClientDb::new(state).await.unwrap();
    let clean_error = super::clean_agent(base.path(), &db, name)
        .await
        .expect_err("configured runner must block cleanup");
    assert!(clean_error.to_string().contains("configured runner"));
    assert!(root.join("worktree").is_dir());
    assert!(root.join("state/runner/runner-state.json").is_file());

    let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
    let replace_error = super::spawn_agent(
        base.path(),
        &db,
        &api,
        "runner-clean-test",
        name,
        Some(&"e".repeat(64)),
        false,
        true,
    )
    .await
    .expect_err("configured runner must block replacement before network access");
    assert!(replace_error.to_string().contains("configured runner"));
    assert!(root.join("worktree").is_dir());
    assert!(root.join("state/base-snapshot").is_file());
    assert!(root.join("state/runner/runner-state.json").is_file());
}

#[tokio::test]
async fn agent_runtime_stays_under_its_owner_and_clean_removes_it() {
    let base = tempfile::tempdir().unwrap();
    let name = "owned-runtime";
    let worktree = agent_dir(base.path(), name).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    let legacy_slot = crate::workspace_layout::workspace_state_path(&worktree).unwrap();
    assert!(!legacy_slot.exists());

    let runtime = open_agent_runtime(base.path(), name).await.unwrap();
    assert_eq!(
        runtime.state_dir,
        agent_root(base.path(), name).unwrap().join("state/runtime")
    );
    assert!(runtime.state_dir.join("local_state.json").is_file());

    let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
    let config = crate::local::Config {
        server_url: "http://127.0.0.1:1".into(),
        workspace_id: "agent-runtime-test".into(),
        encryption_password: Some("a".repeat(64)),
        server_password: None,
        tls_ca_pem: None,
        format_version: 3,
        hub_local: false,
        relay: None,
    };
    let ctx = crate::ctx::SyncCtx::from_config_with_state_dir(
        &api,
        &runtime.db,
        &worktree,
        &config,
        runtime.state_dir.clone(),
    )
    .unwrap();
    assert_eq!(ctx.state_dir().unwrap(), runtime.state_dir);
    assert!(
        !legacy_slot.exists(),
        "agent runtime must not register its worktree as a top-level workspace"
    );

    let parent_state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
    let parent_db = crate::local::ClientDb::new(parent_state).await.unwrap();
    super::clean_agent(base.path(), &parent_db, name)
        .await
        .unwrap();
    assert!(!agent_root(base.path(), name).unwrap().exists());
    assert!(!legacy_slot.exists());
}

#[tokio::test]
async fn verified_legacy_agent_cache_is_copied_but_never_removed() {
    let base = tempfile::tempdir().unwrap();
    let name = "legacy-runtime";
    let worktree = agent_dir(base.path(), name).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    let legacy = crate::workspace_layout::ensure_workspace_state(&worktree).unwrap();
    let legacy_db = crate::local::ClientDb::new(&legacy).await.unwrap();
    legacy_db
        .set_session_key("agent-runtime-migration", "preserved")
        .await
        .unwrap();
    let legacy_before = std::fs::read(legacy.join("local_state.json")).unwrap();
    let stored_identity = std::fs::read_to_string(legacy.join("identity")).ok();
    let identity_matches = stored_identity.is_some_and(|stored| {
        crate::workspace_layout::workspace_identity_matches(&worktree, stored.trim()).unwrap()
    });

    let runtime = open_agent_runtime(base.path(), name).await.unwrap();
    let migrated = runtime
        .db
        .get_session_key("agent-runtime-migration")
        .await
        .unwrap();
    if identity_matches {
        assert_eq!(migrated.as_deref(), Some("preserved"));
    } else {
        assert_eq!(migrated, None, "unverified cache must be rebuilt");
    }
    assert_eq!(
        std::fs::read(legacy.join("local_state.json")).unwrap(),
        legacy_before,
        "compatibility migration must not mutate legacy profile state"
    );

    let parent_state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
    let parent_db = crate::local::ClientDb::new(parent_state).await.unwrap();
    super::clean_agent(base.path(), &parent_db, name)
        .await
        .unwrap();
    assert!(!runtime.state_dir.exists());
    assert!(legacy.join("local_state.json").is_file());
}

#[tokio::test]
async fn unverified_legacy_agent_cache_is_preserved_but_not_adopted() {
    let base = tempfile::tempdir().unwrap();
    let name = "unverified-runtime";
    let worktree = agent_dir(base.path(), name).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    let legacy = crate::workspace_layout::ensure_workspace_state(&worktree).unwrap();
    let legacy_db = crate::local::ClientDb::new(&legacy).await.unwrap();
    legacy_db
        .set_session_key("stale-agent-runtime", "do-not-import")
        .await
        .unwrap();
    std::fs::write(legacy.join("identity"), b"different-directory-identity").unwrap();
    let legacy_before = std::fs::read(legacy.join("local_state.json")).unwrap();

    let runtime = open_agent_runtime(base.path(), name).await.unwrap();

    assert_eq!(
        runtime
            .db
            .get_session_key("stale-agent-runtime")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        std::fs::read(legacy.join("local_state.json")).unwrap(),
        legacy_before
    );
    assert!(legacy.is_dir());
}
