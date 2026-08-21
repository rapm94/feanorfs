//! Narrow WebSocket primitives shared by the pair and tunnel relay routes.
//!
//! Both routes rendezvous an offer/host with a join/client over a plain
//! WebSocket and then forward messages bidirectionally. The mechanics live
//! here once — waiting for the peer while answering Ping/Pong heartbeats, and
//! forwarding with a per-route policy (message bound, optional frame or byte
//! budget, optional idle timeout, per-write budget). Each route keeps its own
//! policy values and its own frame-count vs byte/idle semantics; this module
//! only parameterizes the shared machinery.

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt as _, StreamExt as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex};

/// True when `value` is exactly `expected_len` lowercase ASCII hex digits.
///
/// `expected_len` is caller-fixed (pair sessions: 32; tunnel routes: 64), so
/// the empty string is rejected by every real caller.
pub(crate) fn valid_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Policy for [`wait_for_peer`]: how long an offer waits for its peer, and
/// how long a single Pong heartbeat write may take.
#[derive(Clone, Copy)]
pub(crate) struct PeerWaitPolicy {
    /// Total time the offer waits for its peer before giving up.
    pub expiry: Duration,
    /// Per-heartbeat write budget for the Pong reply.
    pub heartbeat_write_timeout: Duration,
}

/// Wait for a peer to join, answering Ping heartbeats with Pongs meanwhile.
///
/// Returns `Some(peer)` when `joined` fires, or `None` when the peer never
/// joins before `policy.expiry`, the socket dies mid-wait, or a heartbeat
/// write cannot complete within `policy.heartbeat_write_timeout`.
pub(crate) async fn wait_for_peer<T>(
    socket: &mut WebSocket,
    mut joined: oneshot::Receiver<T>,
    policy: PeerWaitPolicy,
) -> Option<T> {
    let deadline = tokio::time::sleep(policy.expiry);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            peer = &mut joined => return peer.ok(),
            _ = &mut deadline => return None,
            message = socket.next() => match message {
                Some(Ok(Message::Ping(bytes))) => {
                    tokio::time::timeout(
                        policy.heartbeat_write_timeout,
                        socket.send(Message::Pong(bytes)),
                    )
                    .await
                    .ok()?
                    .ok()?;
                }
                Some(Ok(Message::Pong(_))) => {}
                _ => return None,
            }
        }
    }
}

/// Policy for [`relay`]: per-message bound, optional frame and byte budgets
/// shared across both directions, optional idle timeout, and the per-write
/// send budget.
///
/// The pair relay sets `max_frames` (a small PAKE handshake cap) and no idle
/// timeout; the tunnel relay sets `max_bytes` and an idle timeout. Each route
/// sets exactly one budget; the mechanics below stay policy-agnostic.
#[derive(Clone, Copy)]
pub(crate) struct ForwardPolicy {
    /// Largest single relayed message, in bytes.
    pub max_message_bytes: usize,
    /// Optional total frame cap shared by both directions. When set, every
    /// relayed message — including Ping/Pong heartbeats — costs one frame.
    pub max_frames: Option<usize>,
    /// Optional total byte cap shared by both directions. Only Binary payload
    /// bytes count; Ping/Pong heartbeats are free.
    pub max_bytes: Option<u64>,
    /// Optional wall-clock idle timeout; the relay ends after this long with
    /// no message relayed in either direction.
    pub idle_timeout: Option<Duration>,
    /// Budget for writing a single forwarded message.
    pub write_timeout: Duration,
}

/// Forward messages between two WebSockets until either direction ends, a
/// budget is exhausted, or the idle timeout elapses.
pub(crate) async fn relay(a: &mut WebSocket, b: &mut WebSocket, policy: ForwardPolicy) {
    let frames = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let activity = Arc::new(Mutex::new(Instant::now()));
    let (a_send, a_receive) = a.split();
    let (b_send, b_receive) = b.split();
    let a_activity = Arc::clone(&activity);
    let a_to_b = forward_loop(a_receive, b_send, &frames, &bytes, a_activity, policy);
    let b_activity = Arc::clone(&activity);
    let b_to_a = forward_loop(b_receive, a_send, &frames, &bytes, b_activity, policy);
    tokio::select! {
        () = a_to_b => {}
        () = b_to_a => {}
        () = wait_for_idle(&activity, policy.idle_timeout) => {}
    }
}

/// Cost of relaying one message, or `None` if it must not be relayed
/// (oversized Binary, Text, Close, ...).
#[derive(Clone, Copy)]
struct MessageCost {
    frames: u64,
    bytes: u64,
}

fn relay_cost(message: &Message, policy: &ForwardPolicy) -> Option<MessageCost> {
    match message {
        Message::Binary(bytes) if bytes.len() <= policy.max_message_bytes => Some(MessageCost {
            frames: u64::from(policy.max_frames.is_some()),
            bytes: bytes.len() as u64,
        }),
        Message::Ping(_) | Message::Pong(_) => Some(MessageCost {
            frames: u64::from(policy.max_frames.is_some()),
            bytes: 0,
        }),
        _ => None,
    }
}

/// Reserve a message's cost against the configured budgets.
fn claim(frames: &AtomicU64, bytes: &AtomicU64, cost: MessageCost, policy: &ForwardPolicy) -> bool {
    if let Some(limit) = policy.max_frames {
        let ok = frames
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(cost.frames)
                    .filter(|next| *next <= limit as u64)
            })
            .is_ok();
        if !ok {
            return false;
        }
    }
    if let Some(limit) = policy.max_bytes {
        let ok = bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(cost.bytes)
                    .filter(|next| *next <= limit)
            })
            .is_ok();
        if !ok {
            return false;
        }
    }
    true
}

/// Pump messages from `receive` to `send`, charging each against the shared
/// budgets and refreshing the shared activity clock, until the source ends, a
/// message is disallowed, a budget is exhausted, or a write fails.
async fn forward_loop(
    mut receive: futures_util::stream::SplitStream<&mut WebSocket>,
    mut send: futures_util::stream::SplitSink<&mut WebSocket, Message>,
    frames: &AtomicU64,
    bytes: &AtomicU64,
    activity: Arc<Mutex<Instant>>,
    policy: ForwardPolicy,
) {
    while let Some(Ok(message)) = receive.next().await {
        let Some(cost) = relay_cost(&message, &policy) else {
            return;
        };
        if !claim(frames, bytes, cost, &policy) {
            return;
        }
        *activity.lock().await = Instant::now();
        if !matches!(
            tokio::time::timeout(policy.write_timeout, send.send(message)).await,
            Ok(Ok(()))
        ) {
            return;
        }
    }
}

/// End the relay after `idle_timeout` with no relayed message. Without an
/// idle timeout this future never completes, so the relay only ends when one
/// of the directional pumps does.
async fn wait_for_idle(activity: &Arc<Mutex<Instant>>, idle_timeout: Option<Duration>) {
    let Some(idle_timeout) = idle_timeout else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        let remaining = {
            let last = *activity.lock().await;
            idle_timeout.saturating_sub(last.elapsed())
        };
        if remaining.is_zero() {
            return;
        }
        tokio::time::sleep(remaining).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::WebSocketUpgrade;
    use axum::extract::State;
    use axum::routing::get;
    use axum::Router;
    use std::net::SocketAddr;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::Message as WireMessage;
    use tokio_tungstenite::{client_async, WebSocketStream};

    type WireSocket = WebSocketStream<TcpStream>;

    #[test]
    fn valid_lower_hex_accepts_exact_length_lowercase_hex() {
        assert!(valid_lower_hex("0123456789abcdef", 16));
        assert!(valid_lower_hex("deadbeef", 8));
    }

    #[test]
    fn valid_lower_hex_rejects_wrong_length() {
        assert!(!valid_lower_hex("0123456789abcdef", 8));
        assert!(!valid_lower_hex("", 16));
        assert!(!valid_lower_hex("abc", 2));
    }

    #[test]
    fn valid_lower_hex_rejects_uppercase_and_non_hex() {
        assert!(!valid_lower_hex("0123456789ABCDEF", 16));
        assert!(!valid_lower_hex("0123456789abcdefg", 17));
        assert!(!valid_lower_hex("../../etc/passwd", 16));
        assert!(!valid_lower_hex("ab cd", 5));
    }

    #[derive(Clone)]
    struct WsTestState {
        a: Arc<Mutex<Option<oneshot::Sender<WebSocket>>>>,
        b: Arc<Mutex<Option<oneshot::Sender<WebSocket>>>>,
    }

    /// Open two real WebSocket connections to a local relay-free router and
    /// return the server-side sockets plus the wire-level client streams.
    async fn ws_pair() -> (WebSocket, WebSocket, WireSocket, WireSocket) {
        let (tx_a, rx_a) = oneshot::channel();
        let (tx_b, rx_b) = oneshot::channel();
        let state = WsTestState {
            a: Arc::new(Mutex::new(Some(tx_a))),
            b: Arc::new(Mutex::new(Some(tx_b))),
        };
        let router = Router::new()
            .route(
                "/a",
                get(
                    |State(state): State<WsTestState>, ws: WebSocketUpgrade| async move {
                        ws.on_upgrade(move |socket| async move {
                            let sender = state.a.lock().await.take();
                            let _ = sender.unwrap().send(socket);
                        })
                    },
                ),
            )
            .route(
                "/b",
                get(
                    |State(state): State<WsTestState>, ws: WebSocketUpgrade| async move {
                        ws.on_upgrade(move |socket| async move {
                            let sender = state.b.lock().await.take();
                            let _ = sender.unwrap().send(socket);
                        })
                    },
                ),
            )
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let (server_a, client_a) = connect_ws(addr, "/a", rx_a).await;
        let (server_b, client_b) = connect_ws(addr, "/b", rx_b).await;
        (server_a, server_b, client_a, client_b)
    }

    async fn connect_ws(
        addr: SocketAddr,
        path: &str,
        rx: oneshot::Receiver<WebSocket>,
    ) -> (WebSocket, WireSocket) {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let url = format!("ws://{addr}{path}");
        let (client, _response) = client_async(url.as_str(), tcp).await.unwrap();
        let server = rx.await.unwrap();
        (server, client)
    }

    #[tokio::test]
    async fn wait_for_peer_expires_when_no_peer_joins() {
        let (mut socket, _peer, _client, _client_peer) = ws_pair().await;
        let (_never, joined) = oneshot::channel::<()>();
        let policy = PeerWaitPolicy {
            expiry: Duration::from_millis(40),
            heartbeat_write_timeout: Duration::from_secs(5),
        };
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_peer(&mut socket, joined, policy),
        )
        .await
        .expect("wait_for_peer must return after its expiry");
        assert!(result.is_none());
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn wait_for_peer_answers_ping_heartbeats_until_expiry() {
        let (mut socket, _peer, mut client, _client_peer) = ws_pair().await;
        let (_never, joined) = oneshot::channel::<()>();
        let policy = PeerWaitPolicy {
            expiry: Duration::from_millis(300),
            heartbeat_write_timeout: Duration::from_secs(5),
        };
        let waiting = tokio::spawn(async move { wait_for_peer(&mut socket, joined, policy).await });
        client
            .send(WireMessage::Ping(b"hb".to_vec().into()))
            .await
            .unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .expect("pong reply must arrive")
            .expect("stream must stay open")
            .expect("no protocol error");
        match pong {
            WireMessage::Pong(bytes) => assert_eq!(&bytes[..], b"hb"),
            other => panic!("expected Pong echoing the ping, got {other:?}"),
        }
        let result = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("wait_for_peer must finish at expiry")
            .expect("waiting task must not panic");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn relay_ends_after_idle_timeout() {
        let (mut a, mut b, client_a, client_b) = ws_pair().await;
        let policy = ForwardPolicy {
            max_frames: None,
            max_bytes: None,
            max_message_bytes: 1024,
            idle_timeout: Some(Duration::from_millis(40)),
            write_timeout: Duration::from_secs(5),
        };
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), relay(&mut a, &mut b, policy))
            .await
            .expect("relay must end at the idle timeout");
        assert!(started.elapsed() >= Duration::from_millis(40));
        drop((client_a, client_b));
    }

    #[tokio::test]
    async fn relay_enforces_shared_frame_budget() {
        let (mut a, mut b, mut client_a, mut client_b) = ws_pair().await;
        let policy = ForwardPolicy {
            max_frames: Some(2),
            max_bytes: None,
            max_message_bytes: 1024,
            idle_timeout: None,
            write_timeout: Duration::from_secs(5),
        };
        let relayed = tokio::spawn(async move { relay(&mut a, &mut b, policy).await });
        for payload in [b"one".as_slice(), b"two", b"three"] {
            client_a
                .send(WireMessage::binary(payload.to_vec()))
                .await
                .unwrap();
        }
        let mut forwarded = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(2), client_b.next())
                .await
                .expect("relay must forward the budgeted frames then stop");
            match next {
                Some(Ok(WireMessage::Binary(bytes))) => forwarded.push(bytes.to_vec()),
                Some(Ok(_)) | None | Some(Err(_)) => break,
            }
        }
        assert_eq!(forwarded, vec![b"one".to_vec(), b"two".to_vec()]);
        tokio::time::timeout(Duration::from_secs(5), relayed)
            .await
            .expect("relay must finish when the frame budget is exhausted")
            .expect("relay must not panic");
    }

    #[tokio::test]
    async fn relay_enforces_shared_byte_budget() {
        let (mut a, mut b, mut client_a, mut client_b) = ws_pair().await;
        let policy = ForwardPolicy {
            max_frames: None,
            max_bytes: Some(3),
            max_message_bytes: 1024,
            idle_timeout: None,
            write_timeout: Duration::from_secs(5),
        };
        let relayed = tokio::spawn(async move { relay(&mut a, &mut b, policy).await });
        client_a
            .send(WireMessage::binary(b"ab".to_vec()))
            .await
            .unwrap();
        client_a
            .send(WireMessage::binary(b"cd".to_vec()))
            .await
            .unwrap();
        let mut forwarded = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(2), client_b.next())
                .await
                .expect("relay must forward the budgeted bytes then stop");
            match next {
                Some(Ok(WireMessage::Binary(bytes))) => forwarded.push(bytes.to_vec()),
                Some(Ok(_)) | None | Some(Err(_)) => break,
            }
        }
        assert_eq!(forwarded, vec![b"ab".to_vec()]);
        tokio::time::timeout(Duration::from_secs(5), relayed)
            .await
            .expect("relay must finish when the byte budget is exhausted")
            .expect("relay must not panic");
    }

    #[tokio::test]
    async fn relay_stops_on_oversized_message() {
        let (mut a, mut b, mut client_a, mut client_b) = ws_pair().await;
        let policy = ForwardPolicy {
            max_frames: None,
            max_bytes: None,
            max_message_bytes: 4,
            idle_timeout: None,
            write_timeout: Duration::from_secs(5),
        };
        let relayed = tokio::spawn(async move { relay(&mut a, &mut b, policy).await });
        client_a
            .send(WireMessage::binary(b"toolong".to_vec()))
            .await
            .unwrap();
        let next = tokio::time::timeout(Duration::from_secs(2), client_b.next())
            .await
            .expect("relay must finish after rejecting the oversized message");
        assert!(
            !matches!(next, Some(Ok(WireMessage::Binary(_)))),
            "oversized message must not be forwarded"
        );
        tokio::time::timeout(Duration::from_secs(5), relayed)
            .await
            .expect("relay must finish")
            .expect("relay must not panic");
    }
}
