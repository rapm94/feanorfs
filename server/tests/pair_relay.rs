use feanorfs_server::{run_http_server, ServeOptions};
use futures_util::{SinkExt as _, StreamExt as _};
use std::net::{Ipv4Addr, TcpListener};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn public_pair_relay_forwards_only_opaque_binary_frames() {
    let data = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let server = tokio::spawn(run_http_server(ServeOptions {
        data_dir: data.path().to_path_buf(),
        port,
        token: Some("hub-api-token".into()),
        allow_http: true,
        relay: true,
        ..ServeOptions::default()
    }));

    let mut ready = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "pair relay did not become ready");

    let protected = reqwest::get(format!("http://127.0.0.1:{port}/api/workspaces"))
        .await
        .unwrap();
    assert_eq!(protected.status(), reqwest::StatusCode::UNAUTHORIZED);

    let session = "0123456789abcdef0123456789abcdef";
    let (mut offer, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/pair-relay/{session}/offer"
    ))
    .await
    .unwrap();
    let (mut join, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/pair-relay/{session}/join"
    ))
    .await
    .unwrap();

    let first = b"opaque PAKE frame".to_vec();
    offer
        .send(Message::Binary(first.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        join.next().await.unwrap().unwrap().into_data(),
        first.as_slice()
    );

    let second = b"opaque encrypted invite".to_vec();
    join.send(Message::Binary(second.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        offer.next().await.unwrap().unwrap().into_data(),
        second.as_slice()
    );

    let invalid = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/pair-relay/not-a-session/offer"
    ))
    .await;
    assert!(invalid.is_err());

    server.abort();
}

#[tokio::test]
async fn pair_relay_is_disabled_unless_explicitly_enabled() {
    let data = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let server = tokio::spawn(run_http_server(ServeOptions {
        data_dir: data.path().to_path_buf(),
        port,
        allow_http: true,
        ..ServeOptions::default()
    }));

    for _ in 0..100 {
        if tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let connection = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/pair-relay/0123456789abcdef0123456789abcdef/offer"
    ))
    .await;
    assert!(connection.is_err());
    let tunnel = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/api/tunnel-relay/{}/host",
        "a".repeat(64)
    ))
    .await;
    assert!(tunnel.is_err());

    server.abort();
}
