use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt as _, StreamExt as _};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex, OwnedSemaphorePermit, Semaphore};

use super::AppState;

const SESSION_HEX_BYTES: usize = 16;
const SESSION_HEX_LEN: usize = SESSION_HEX_BYTES * 2;
const MAX_PENDING_OFFERS: usize = 1024;
const MAX_RELAY_CONNECTIONS: usize = 4096;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_RELAYED_FRAMES: usize = 8;
const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct PairRelayState {
    pending: Arc<Mutex<HashMap<String, PendingOffer>>>,
    connections: Arc<Semaphore>,
}

impl Default for PairRelayState {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            connections: Arc::new(Semaphore::new(MAX_RELAY_CONNECTIONS)),
        }
    }
}

struct PendingOffer {
    created_at: Instant,
    join: oneshot::Sender<AdmittedSocket>,
}

struct AdmittedSocket {
    socket: WebSocket,
    _permit: OwnedSemaphorePermit,
}

pub(crate) async fn handle_pair_relay(
    Path((session, role)): Path<(String, String)>,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_session_id(&session) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !matches!(role.as_str(), "offer" | "join") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let relay = state.pair_relay.clone();
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
            if role == "offer" {
                relay.offer(session, admitted).await;
            } else {
                relay.join(session, admitted).await;
            }
        })
        .into_response()
}

impl PairRelayState {
    async fn offer(&self, session: String, mut admitted: AdmittedSocket) {
        let socket = &mut admitted.socket;
        let (join, joined) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            if pending.len() >= MAX_PENDING_OFFERS || pending.contains_key(&session) {
                return;
            }
            pending.insert(
                session.clone(),
                PendingOffer {
                    created_at: Instant::now(),
                    join,
                },
            );
        }

        let mut peer = match wait_for_join(socket, joined).await {
            Some(peer) => peer,
            None => {
                self.pending.lock().await.remove(&session);
                return;
            }
        };

        let _ = tokio::time::timeout(
            EXCHANGE_TIMEOUT,
            relay(&mut admitted.socket, &mut peer.socket),
        )
        .await;
    }

    async fn join(&self, session: String, admitted: AdmittedSocket) {
        let offer = {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            pending.remove(&session)
        };
        let Some(offer) = offer else {
            return;
        };
        let _ = offer.join.send(admitted);
    }
}

async fn wait_for_join(
    socket: &mut WebSocket,
    mut joined: oneshot::Receiver<AdmittedSocket>,
) -> Option<AdmittedSocket> {
    let deadline = tokio::time::sleep(SESSION_TTL);
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

async fn relay(offer: &mut WebSocket, join: &mut WebSocket) {
    let relayed = AtomicUsize::new(0);
    let (mut offer_send, mut offer_receive) = offer.split();
    let (mut join_send, mut join_receive) = join.split();
    let offer_to_join = async {
        while let Some(Ok(message)) = offer_receive.next().await {
            if !claim_frame(&relayed) || !allowed_message(&message) {
                return;
            }
            if !matches!(
                tokio::time::timeout(SOCKET_WRITE_TIMEOUT, join_send.send(message)).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    };
    let join_to_offer = async {
        while let Some(Ok(message)) = join_receive.next().await {
            if !claim_frame(&relayed) || !allowed_message(&message) {
                return;
            }
            if !matches!(
                tokio::time::timeout(SOCKET_WRITE_TIMEOUT, offer_send.send(message)).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    };
    tokio::select! {
        () = offer_to_join => {}
        () = join_to_offer => {}
    }
}

fn claim_frame(relayed: &AtomicUsize) -> bool {
    relayed
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < MAX_RELAYED_FRAMES).then_some(count + 1)
        })
        .is_ok()
}

fn allowed_message(message: &Message) -> bool {
    matches!(
        message,
        Message::Binary(bytes) if bytes.len() <= MAX_FRAME_BYTES
    ) || matches!(message, Message::Ping(_) | Message::Pong(_))
}

fn prune_expired(pending: &mut HashMap<String, PendingOffer>) {
    pending.retain(|_, offer| offer.created_at.elapsed() < SESSION_TTL && !offer.join.is_closed());
}

fn valid_session_id(session: &str) -> bool {
    session.len() == SESSION_HEX_LEN
        && session
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_session_ids_are_128_bit_lowercase_hex() {
        assert!(valid_session_id("0123456789abcdef0123456789abcdef"));
        assert!(!valid_session_id("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!valid_session_id("0123456789abcdef"));
        assert!(!valid_session_id("../../../../../../../../etc/passwd"));
    }

    #[test]
    fn relay_connection_admission_is_globally_bounded() {
        let relay = PairRelayState::default();
        let permits = (0..MAX_RELAY_CONNECTIONS)
            .map(|_| relay.connections.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(relay.connections.clone().try_acquire_owned().is_err());
        drop(permits);
        assert_eq!(relay.connections.available_permits(), MAX_RELAY_CONNECTIONS);
    }

    #[tokio::test]
    async fn expired_and_closed_offers_are_pruned() {
        let (open, _receiver) = oneshot::channel();
        let (closed, receiver) = oneshot::channel();
        drop(receiver);
        let mut pending = HashMap::from([
            (
                "open".into(),
                PendingOffer {
                    created_at: Instant::now(),
                    join: open,
                },
            ),
            (
                "closed".into(),
                PendingOffer {
                    created_at: Instant::now(),
                    join: closed,
                },
            ),
        ]);

        prune_expired(&mut pending);
        assert!(pending.contains_key("open"));
        assert!(!pending.contains_key("closed"));
    }
}
