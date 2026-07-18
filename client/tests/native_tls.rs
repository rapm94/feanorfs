use feanorfs_client::{ApiClient, Config};
use feanorfs_server::{prepare_tls, run_http_server, ServeOptions};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::time::Duration;

#[tokio::test]
async fn native_tls_requires_and_accepts_hub_ca() {
    let data = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut options = ServeOptions {
        data_dir: data.path().to_path_buf(),
        port,
        token: Some("tls-test-token".into()),
        ..ServeOptions::default()
    };
    let identity = prepare_tls(&mut options).unwrap().unwrap();
    let ca = identity.public_ca_pem.unwrap();
    let server = tokio::spawn(run_http_server(options));
    let url = format!("https://127.0.0.1:{port}");
    let trusted = ApiClient::new_with_tls(&url, Some("tls-test-token"), Some(&ca)).unwrap();

    let mut connected = false;
    for _ in 0..20 {
        if tokio::time::timeout(Duration::from_secs(1), trusted.get_workspaces())
            .await
            .is_ok_and(|result| result.is_ok())
        {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        connected,
        "trusted client did not connect to native TLS hub"
    );

    let untrusted = ApiClient::new(&url, Some("tls-test-token"));
    assert!(untrusted.get_workspaces().await.is_err());

    server.abort();
}

#[tokio::test]
async fn opaque_relay_preserves_inner_tls_ca_and_bearer_auth() {
    let hub_data = tempfile::tempdir().unwrap();
    let hub_port = free_port();
    let mut hub_options = ServeOptions {
        data_dir: hub_data.path().to_path_buf(),
        port: hub_port,
        token: Some("inner-tls-token".into()),
        ..ServeOptions::default()
    };
    let identity = prepare_tls(&mut hub_options).unwrap().unwrap();
    let ca = identity.public_ca_pem.unwrap();
    let hostname = identity.mdns_hostname.unwrap();
    let hub = tokio::spawn(run_http_server(hub_options));

    let relay_data = tempfile::tempdir().unwrap();
    let relay_port = free_port();
    let relay = tokio::spawn(run_http_server(ServeOptions {
        data_dir: relay_data.path().to_path_buf(),
        port: relay_port,
        allow_http: true,
        relay: true,
        ..ServeOptions::default()
    }));
    wait_for_port(hub_port).await;
    wait_for_port(relay_port).await;

    let relay_config =
        feanorfs_agent_core::tunnel::generate_config(&format!("http://127.0.0.1:{relay_port}"))
            .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = Config {
        server_url: format!("https://{hostname}:{hub_port}"),
        workspace_id: "opaque-relay".into(),
        encryption_password: Some("a".repeat(64)),
        server_password: Some("inner-tls-token".into()),
        tls_ca_pem: Some(ca.clone()),
        format_version: 3,
        hub_local: false,
        relay: Some(relay_config.clone()),
    };
    let trusted = ApiClient::from_config(workspace.path(), &config)
        .await
        .unwrap();
    let host_relay = relay_config.clone();
    let host_tunnel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        feanorfs_agent_core::tunnel::run_host(
            host_relay,
            SocketAddr::from((Ipv4Addr::LOCALHOST, hub_port)),
        )
        .await
    });
    let mut connected = false;
    for _ in 0..20 {
        if tokio::time::timeout(Duration::from_secs(1), trusted.get_workspaces())
            .await
            .is_ok_and(|result| result.is_ok())
        {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(connected, "inner-TLS client did not connect through relay");

    let mut wrong_token = config.clone();
    wrong_token.server_password = Some("wrong-token".into());
    let unauthorized = ApiClient::from_config(workspace.path(), &wrong_token)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), unauthorized.get_workspaces())
            .await
            .is_ok_and(|result| result.is_err())
    );

    let mut wrong_ca = config;
    wrong_ca.tls_ca_pem = Some("not a certificate".into());
    let untrusted = ApiClient::from_config(workspace.path(), &wrong_ca)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), untrusted.get_workspaces())
            .await
            .is_ok_and(|result| result.is_err())
    );

    host_tunnel.abort();
    relay.abort();
    hub.abort();
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_for_port(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("port {port} did not become ready");
}
