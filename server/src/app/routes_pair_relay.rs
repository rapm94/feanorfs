use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex};

use super::AppState;

const SESSION_HEX_BYTES: usize = 16;
const SESSION_HEX_LEN: usize = SESSION_HEX_BYTES * 2;
const MAX_PENDING_OFFERS: usize = 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_RELAYED_FRAMES: usize = 8;
const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub(crate) struct PairRelayState {
    pending: Arc<Mutex<HashMap<String, PendingOffer>>>,
}

struct PendingOffer {
    created_at: Instant,
    join: oneshot::Sender<WebSocket>,
}

pub(crate) async fn handle_pair_relay(
    Path((session, role)): Path<(String, String)>,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_session_id(&session) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match role.as_str() {
        "offer" => {
            let relay = state.pair_relay.clone();
            upgrade
                .on_upgrade(move |socket| async move { relay.offer(session, socket).await })
                .into_response()
        }
        "join" => {
            let relay = state.pair_relay.clone();
            upgrade
                .on_upgrade(move |socket| async move { relay.join(session, socket).await })
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

impl PairRelayState {
    async fn offer(&self, session: String, mut socket: WebSocket) {
        let (join, joined) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            if pending.len() >= MAX_PENDING_OFFERS || pending.contains_key(&session) {
                let _ = socket.send(Message::Close(None)).await;
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

        let mut peer = match wait_for_join(&mut socket, joined).await {
            Some(peer) => peer,
            None => {
                self.pending.lock().await.remove(&session);
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        };

        let _ = tokio::time::timeout(EXCHANGE_TIMEOUT, relay(&mut socket, &mut peer)).await;
        let _ = socket.send(Message::Close(None)).await;
        let _ = peer.send(Message::Close(None)).await;
    }

    async fn join(&self, session: String, mut socket: WebSocket) {
        let offer = {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            pending.remove(&session)
        };
        let Some(offer) = offer else {
            let _ = socket.send(Message::Close(None)).await;
            return;
        };
        if let Err(mut socket) = offer.join.send(socket) {
            let _ = socket.send(Message::Close(None)).await;
        }
    }
}

async fn wait_for_join(
    socket: &mut WebSocket,
    mut joined: oneshot::Receiver<WebSocket>,
) -> Option<WebSocket> {
    let deadline = tokio::time::sleep(SESSION_TTL);
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

async fn relay(offer: &mut WebSocket, join: &mut WebSocket) {
    let mut relayed = 0_usize;
    while relayed < MAX_RELAYED_FRAMES {
        let forwarded = tokio::select! {
            message = offer.next() => forward(message, join).await,
            message = join.next() => forward(message, offer).await,
        };
        match forwarded {
            Some(()) => relayed += 1,
            _ => return,
        }
    }
}

async fn forward(
    message: Option<Result<Message, axum::Error>>,
    destination: &mut WebSocket,
) -> Option<()> {
    let message = message?.ok()?;
    match &message {
        Message::Binary(bytes) if bytes.len() <= MAX_FRAME_BYTES => {}
        Message::Ping(_) | Message::Pong(_) => {}
        _ => return None,
    }
    destination.send(message).await.ok()
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
