use feanorfs_server::{run_http_server, ServeOptions};
use futures_util::{SinkExt as _, StreamExt as _};
use std::net::{Ipv4Addr, TcpListener};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn public_tunnel_relay_forwards_only_bounded_opaque_binary_frames() {
    let data = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let server = tokio::spawn(run_http_server(ServeOptions {
        data_dir: data.path().to_path_buf(),
        port,
        token: Some("protected-hub-token".into()),
        allow_http: true,
        relay: true,
        ..ServeOptions::default()
    }));
    wait_for_port(port).await;

    let protected = reqwest::get(format!("http://127.0.0.1:{port}/api/workspaces"))
        .await
        .unwrap();
    assert_eq!(protected.status(), reqwest::StatusCode::UNAUTHORIZED);

    let route = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let (mut host, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/tunnel-relay/{route}/host"
    ))
    .await
    .unwrap();
    let (mut client, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/tunnel-relay/{route}/client"
    ))
    .await
    .unwrap();

    let client_hello = vec![0x16; 32 * 1024];
    client
        .send(Message::Binary(client_hello.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        host.next().await.unwrap().unwrap().into_data(),
        client_hello
    );
    let encrypted_response = vec![0x17; 48 * 1024];
    host.send(Message::Binary(encrypted_response.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        client.next().await.unwrap().unwrap().into_data(),
        encrypted_response
    );

    let invalid = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/tunnel-relay/not-a-route/host"
    ))
    .await;
    assert!(invalid.is_err());
    server.abort();
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
    panic!("relay did not become ready");
}
