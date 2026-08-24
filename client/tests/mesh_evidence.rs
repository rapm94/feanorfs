//! End-to-end mesh evidence: a signed capability's LAN candidate must win
//! through the real production endpoint selection and record attempt stats.

feanorfs_test_support::isolate_test_process!();

use feanorfs_client::Config;
use feanorfs_common::{MeshCandidate, MeshCandidateKind, MeshConfig, MeshTransport};
use std::net::SocketAddr;

fn lan_ipv4() -> Option<std::net::IpAddr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(address)
                if !address.is_loopback() && !address.is_unspecified() =>
            {
                Some(std::net::IpAddr::V4(address))
            }
            _ => None,
        })
}

#[tokio::test]
async fn capability_lan_candidate_reaches_the_hub_and_records_stats() {
    let Some(lan) = lan_ipv4() else {
        panic!("this evidence test requires one non-loopback IPv4 interface");
    };

    let data = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut options = feanorfs_server::ServeOptions {
        data_dir: data.path().to_path_buf(),
        port,
        token: Some("mesh-evidence-token".into()),
        ..feanorfs_server::ServeOptions::default()
    };
    let identity = feanorfs_server::prepare_tls(&mut options).unwrap().unwrap();
    let ca = identity.public_ca_pem.unwrap();
    let server = tokio::spawn(feanorfs_server::run_http_server(options));

    let url = format!("https://{lan}:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if server.is_finished() {
            panic!("evidence hub stopped before readiness");
        }
        let api = feanorfs_agent_core::ApiClient::new_with_tls(
            &url,
            Some("mesh-evidence-token"),
            Some(&ca),
        )
        .unwrap();
        if api.get_workspaces().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "TLS hub did not become ready on {url}");

    // The hub advertises itself exactly like the automatic private hub does:
    // signed node id plus its bounded LAN/Direct interface candidates.
    let host_identity = feanorfs_agent_core::mesh::MachineIdentity::load_or_create().unwrap();
    let advertised = SocketAddr::new(lan, port);
    let mesh = MeshConfig::new(
        host_identity.node_id(),
        vec![MeshCandidate::new(MeshTransport::Tcp, MeshCandidateKind::Lan, advertised).unwrap()],
    )
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let config = Config {
        // The leaf SAN carries every interface IP, so this URL is valid; the
        // capability candidate must still WIN selection before any resolver.
        server_url: url.clone(),
        workspace_id: "fsw1-meshevidence0000000000000000".into(),
        encryption_password: Some("a".repeat(64)),
        server_password: Some("mesh-evidence-token".into()),
        tls_ca_pem: Some(ca),
        format_version: 3,
        hub_local: false,
        relay: None,
        mesh: Some(mesh),
    };
    feanorfs_client::save_config(workspace.path(), &config).unwrap();

    // The configured URL cannot resolve, so ONLY the capability's direct
    // candidate can connect. Public selection must return an authenticated client.
    let api = feanorfs_client::open_api_client(workspace.path(), &config)
        .await
        .unwrap();
    assert!(api.get_workspaces().await.is_ok());

    let store = feanorfs_agent_core::mesh::MeshStateStore::open(
        &feanorfs_agent_core::global_state_root().unwrap(),
    )
    .unwrap();
    let state = store.snapshot().unwrap();
    assert!(
        state.tcp().successes() >= 1,
        "the winning direct dial must be recorded in mesh attempt stats"
    );
    let last = state.last_path().expect("winning path cached");
    assert_eq!(last.candidate().address(), advertised);
}
