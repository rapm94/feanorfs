use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt as _, StreamExt as _};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex, OwnedSemaphorePermit, Semaphore};

use super::AppState;

const ROUTE_HEX_LEN: usize = 64;
const MAX_PENDING_HOSTS: usize = 4096;
const MAX_PENDING_PER_ROUTE: usize = 8;
const MAX_ACTIVE_TUNNELS: usize = 1024;
const MAX_RELAY_CONNECTIONS: usize = MAX_PENDING_HOSTS + MAX_ACTIVE_TUNNELS * 2;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_TUNNEL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const HOST_OFFER_TTL: Duration = Duration::from_secs(90);
const TUNNEL_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct TunnelRelayState {
    pending: Arc<Mutex<HashMap<String, VecDeque<PendingHost>>>>,
    active: Arc<Semaphore>,
    connections: Arc<Semaphore>,
    next_id: Arc<AtomicU64>,
}

impl Default for TunnelRelayState {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
            connections: Arc::new(Semaphore::new(MAX_RELAY_CONNECTIONS)),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

struct PendingHost {
    id: u64,
    created_at: Instant,
    client: oneshot::Sender<AdmittedSocket>,
}

struct AdmittedSocket {
    socket: WebSocket,
    _permit: OwnedSemaphorePermit,
}

pub(crate) async fn handle_tunnel_relay(
    Path((route, role)): Path<(String, String)>,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_route(&route) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !matches!(role.as_str(), "host" | "client") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let relay = state.tunnel_relay.clone();
    let Ok(permit) = relay.connections.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let upgrade = upgrade
        .max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .write_buffer_size(MAX_FRAME_BYTES)
        .max_write_buffer_size(MAX_FRAME_BYTES * 4);
    upgrade
        .on_upgrade(move |socket| async move {
            let admitted = AdmittedSocket {
                socket,
                _permit: permit,
            };
            if role == "host" {
                relay.host(route, admitted).await;
            } else {
                relay.client(route, admitted).await;
            }
        })
        .into_response()
}

impl TunnelRelayState {
    async fn host(&self, route: String, mut admitted: AdmittedSocket) {
        let socket = &mut admitted.socket;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (client, joined) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            let total = pending.values().map(VecDeque::len).sum::<usize>();
            let route_len = pending.get(&route).map_or(0, VecDeque::len);
            if total >= MAX_PENDING_HOSTS || route_len >= MAX_PENDING_PER_ROUTE {
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

        let mut peer = match wait_for_client(socket, joined).await {
            Some(peer) => peer,
            None => {
                self.remove_pending(&route, id).await;
                return;
            }
        };

        let Ok(_permit) = self.active.clone().try_acquire_owned() else {
            return;
        };
        let _ = tokio::time::timeout(
            TUNNEL_LIFETIME,
            relay(&mut admitted.socket, &mut peer.socket),
        )
        .await;
    }

    async fn client(&self, route: String, admitted: AdmittedSocket) {
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
            return;
        };
        let _ = host.client.send(admitted);
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
    mut joined: oneshot::Receiver<AdmittedSocket>,
) -> Option<AdmittedSocket> {
    let deadline = tokio::time::sleep(HOST_OFFER_TTL);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            peer = &mut joined => return peer.ok(),
            _ = &mut deadline => return None,
            message = socket.next() => match message {
                Some(Ok(Message::Ping(bytes))) => {
                    tokio::time::timeout(
                        SOCKET_WRITE_TIMEOUT,
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

async fn relay(host: &mut WebSocket, client: &mut WebSocket) {
    let bytes = AtomicU64::new(0);
    let activity = Arc::new(Mutex::new(Instant::now()));
    let (mut host_send, mut host_receive) = host.split();
    let (mut client_send, mut client_receive) = client.split();
    let host_activity = Arc::clone(&activity);
    let host_to_client = async {
        while let Some(Ok(message)) = host_receive.next().await {
            let Some(message_bytes) = allowed_message_bytes(&message) else {
                return;
            };
            if !claim_bytes(&bytes, message_bytes) {
                return;
            }
            *host_activity.lock().await = Instant::now();
            if !matches!(
                tokio::time::timeout(SOCKET_WRITE_TIMEOUT, client_send.send(message)).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    };
    let client_activity = Arc::clone(&activity);
    let client_to_host = async {
        while let Some(Ok(message)) = client_receive.next().await {
            let Some(message_bytes) = allowed_message_bytes(&message) else {
                return;
            };
            if !claim_bytes(&bytes, message_bytes) {
                return;
            }
            *client_activity.lock().await = Instant::now();
            if !matches!(
                tokio::time::timeout(SOCKET_WRITE_TIMEOUT, host_send.send(message)).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    };
    tokio::select! {
        () = host_to_client => {}
        () = client_to_host => {}
        () = wait_for_idle(activity) => {}
    }
}

fn allowed_message_bytes(message: &Message) -> Option<u64> {
    match message {
        Message::Binary(bytes) if bytes.len() <= MAX_FRAME_BYTES => Some(bytes.len() as u64),
        Message::Ping(_) | Message::Pong(_) => Some(0),
        _ => None,
    }
}

fn claim_bytes(bytes: &AtomicU64, additional: u64) -> bool {
    bytes
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current
                .checked_add(additional)
                .filter(|next| *next <= MAX_TUNNEL_BYTES)
        })
        .is_ok()
}

async fn wait_for_idle(activity: Arc<Mutex<Instant>>) {
    loop {
        let remaining = {
            let last = *activity.lock().await;
            TUNNEL_IDLE_TIMEOUT.saturating_sub(last.elapsed())
        };
        if remaining.is_zero() {
            return;
        }
        tokio::time::sleep(remaining).await;
    }
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

    #[test]
    fn tunnel_connection_admission_is_globally_bounded() {
        let relay = TunnelRelayState::default();
        let permits = (0..MAX_RELAY_CONNECTIONS)
            .map(|_| relay.connections.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(relay.connections.clone().try_acquire_owned().is_err());
        drop(permits);
        assert_eq!(relay.connections.available_permits(), MAX_RELAY_CONNECTIONS);
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
