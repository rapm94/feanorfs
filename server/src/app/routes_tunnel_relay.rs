use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex, Semaphore};

use super::AppState;

const ROUTE_HEX_LEN: usize = 64;
const MAX_PENDING_HOSTS: usize = 4096;
const MAX_PENDING_PER_ROUTE: usize = 8;
const MAX_ACTIVE_TUNNELS: usize = 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_TUNNEL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const HOST_OFFER_TTL: Duration = Duration::from_secs(90);
const TUNNEL_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
pub(crate) struct TunnelRelayState {
    pending: Arc<Mutex<HashMap<String, VecDeque<PendingHost>>>>,
    active: Arc<Semaphore>,
    next_id: Arc<AtomicU64>,
}

impl Default for TunnelRelayState {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

struct PendingHost {
    id: u64,
    created_at: Instant,
    client: oneshot::Sender<WebSocket>,
}

pub(crate) async fn handle_tunnel_relay(
    Path((route, role)): Path<(String, String)>,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_route(&route) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match role.as_str() {
        "host" => {
            let relay = state.tunnel_relay.clone();
            upgrade
                .on_upgrade(move |socket| async move { relay.host(route, socket).await })
                .into_response()
        }
        "client" => {
            let relay = state.tunnel_relay.clone();
            upgrade
                .on_upgrade(move |socket| async move { relay.client(route, socket).await })
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

impl TunnelRelayState {
    async fn host(&self, route: String, mut socket: WebSocket) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (client, joined) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            let total = pending.values().map(VecDeque::len).sum::<usize>();
            let route_len = pending.get(&route).map_or(0, VecDeque::len);
            if total >= MAX_PENDING_HOSTS || route_len >= MAX_PENDING_PER_ROUTE {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            pending
                .entry(route.clone())
                .or_default()
                .push_back(PendingHost {
                    id,
                    created_at: Instant::now(),
                    client,
                });
        }

        let mut peer = match wait_for_client(&mut socket, joined).await {
            Some(peer) => peer,
            None => {
                self.remove_pending(&route, id).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        };

        let Ok(_permit) = self.active.clone().try_acquire_owned() else {
            let _ = socket.send(Message::Close(None)).await;
            let _ = peer.send(Message::Close(None)).await;
            return;
        };
        let _ = tokio::time::timeout(TUNNEL_LIFETIME, relay(&mut socket, &mut peer)).await;
        let _ = socket.send(Message::Close(None)).await;
        let _ = peer.send(Message::Close(None)).await;
    }

    async fn client(&self, route: String, mut socket: WebSocket) {
        let host = {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            let host = pending.get_mut(&route).and_then(VecDeque::pop_front);
            if pending.get(&route).is_some_and(VecDeque::is_empty) {
                pending.remove(&route);
            }
            host
        };
        let Some(host) = host else {
            let _ = socket.send(Message::Close(None)).await;
            return;
        };
        if let Err(mut socket) = host.client.send(socket) {
            let _ = socket.send(Message::Close(None)).await;
        }
    }

    async fn remove_pending(&self, route: &str, id: u64) {
        let mut pending = self.pending.lock().await;
        if let Some(hosts) = pending.get_mut(route) {
            hosts.retain(|host| host.id != id);
            if hosts.is_empty() {
                pending.remove(route);
            }
        }
    }
}

async fn wait_for_client(
    socket: &mut WebSocket,
    mut joined: oneshot::Receiver<WebSocket>,
) -> Option<WebSocket> {
    let deadline = tokio::time::sleep(HOST_OFFER_TTL);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            peer = &mut joined => return peer.ok(),
            _ = &mut deadline => return None,
            message = socket.next() => match message {
                Some(Ok(Message::Ping(bytes))) => {
                    socket.send(Message::Pong(bytes)).await.ok()?;
                }
                Some(Ok(Message::Pong(_))) => {}
                _ => return None,
            }
        }
    }
}

async fn relay(host: &mut WebSocket, client: &mut WebSocket) {
    let mut bytes = 0_u64;
    loop {
        let forwarded = tokio::select! {
            message = host.next() => forward(message, client).await,
            message = client.next() => forward(message, host).await,
        };
        let Some(forwarded) = forwarded else {
            return;
        };
        let Some(next) = bytes.checked_add(forwarded) else {
            return;
        };
        if next > MAX_TUNNEL_BYTES {
            return;
        }
        bytes = next;
    }
}

async fn forward(
    message: Option<Result<Message, axum::Error>>,
    destination: &mut WebSocket,
) -> Option<u64> {
    let message = message?.ok()?;
    let bytes = match &message {
        Message::Binary(bytes) if bytes.len() <= MAX_FRAME_BYTES => bytes.len() as u64,
        Message::Ping(_) | Message::Pong(_) => 0,
        _ => return None,
    };
    destination.send(message).await.ok()?;
    Some(bytes)
}

fn prune_expired(pending: &mut HashMap<String, VecDeque<PendingHost>>) {
    pending.retain(|_, hosts| {
        hosts.retain(|host| host.created_at.elapsed() < HOST_OFFER_TTL && !host.client.is_closed());
        !hosts.is_empty()
    });
}

fn valid_route(route: &str) -> bool {
    route.len() == ROUTE_HEX_LEN
        && route
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_routes_are_256_bit_lowercase_hex() {
        assert!(valid_route(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!valid_route(
            "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF"
        ));
        assert!(!valid_route("0123456789abcdef0123456789abcdef"));
        assert!(!valid_route("../../../../../../../../etc/passwd"));
    }

    #[tokio::test]
    async fn closed_hosts_are_pruned() {
        let (open, _receiver) = oneshot::channel();
        let (closed, receiver) = oneshot::channel();
        drop(receiver);
        let mut pending = HashMap::from([(
            "route".into(),
            VecDeque::from([
                PendingHost {
                    id: 1,
                    created_at: Instant::now(),
                    client: open,
                },
                PendingHost {
                    id: 2,
                    created_at: Instant::now(),
                    client: closed,
                },
            ]),
        )]);

        prune_expired(&mut pending);
        let hosts = pending.get("route").unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts.front().unwrap().id, 1);
    }
}
