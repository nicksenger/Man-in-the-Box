use crate::{
    AppState, ClientHandle, ClientMessage, ServerMessage, WS_AUTH_HANDSHAKE_TIMEOUT,
    WS_OUTBOUND_QUEUE_CAPACITY, WS_PENALTY_CHECK_INTERVAL, WS_RUNTIME_READ_TIMEOUT, ws_auth,
};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::{interval, timeout};
use tracing::{info, warn};

pub(crate) async fn handle_socket(state: AppState, mut socket: WebSocket, remote_addr: SocketAddr) {
    let auth_result = timeout(
        WS_AUTH_HANDSHAKE_TIMEOUT,
        ws_auth::authenticate_socket(&state, &mut socket, remote_addr.ip()),
    )
    .await;
    let Some((room_handle, role, client_alias)) = (match auth_result {
        Ok(result) => result,
        Err(_) => {
            warn!(%remote_addr, "authentication handshake timed out");
            return;
        }
    }) else {
        return;
    };

    let (mut sender, mut receiver) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel(WS_OUTBOUND_QUEUE_CAPACITY);
    let penalized = Arc::new(AtomicBool::new(false));
    let registration = state
        .register(
            &room_handle,
            role.clone(),
            client_alias.as_deref(),
            outbound_tx.clone(),
            Arc::clone(&penalized),
        )
        .await;
    let registration = match registration {
        Ok(registration) => registration,
        Err(error) => {
            warn!(%room_handle, %error, "failed to register websocket client");
            return;
        }
    };
    let client_id = registration.client_id.clone();

    info!(%client_id, role = ?role, "client connected");

    for message in registration.initial_messages {
        if !enqueue_local_message(&outbound_tx, &penalized, message) {
            warn!(%client_id, "disconnecting client after outbound queue overflow");
            break;
        }
    }
    broadcast_all(registration.broadcasts);
    let mut last_read_at = Instant::now();
    let mut maintenance_tick = interval(WS_PENALTY_CHECK_INTERVAL);

    loop {
        tokio::select! {
            _ = maintenance_tick.tick() => {
                if penalized.load(Ordering::Relaxed) {
                    warn!(%client_id, "disconnecting penalized client");
                    break;
                }
                if last_read_at.elapsed() >= WS_RUNTIME_READ_TIMEOUT {
                    warn!(%client_id, timeout_secs = WS_RUNTIME_READ_TIMEOUT.as_secs(), "disconnecting idle websocket client");
                    break;
                }
            }
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                if !send_outbound_message(&mut sender, &client_id, outbound).await {
                    break;
                }
            }
            inbound = receiver.next() => {
                match inbound {
                    Some(Ok(message)) => {
                        last_read_at = Instant::now();
                        if !handle_incoming_message(
                            &state,
                            &room_handle,
                            &client_id,
                            &outbound_tx,
                            &penalized,
                            message,
                        ).await {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        warn!(%client_id, %error, "websocket receive error");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    let outcome = state
        .unregister(&room_handle, role.clone(), &client_id)
        .await;
    broadcast_all(outcome.broadcasts);
    info!(%client_id, role = ?role, "client disconnected");
}

async fn send_outbound_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    client_id: &str,
    outbound: ServerMessage,
) -> bool {
    match serde_json::to_string(&outbound) {
        Ok(payload) => sender.send(Message::Text(payload.into())).await.is_ok(),
        Err(error) => {
            warn!(%client_id, %error, "failed to serialize outbound websocket message");
            true
        }
    }
}

async fn handle_incoming_message(
    state: &AppState,
    room_handle: &str,
    client_id: &str,
    outbound_tx: &mpsc::Sender<ServerMessage>,
    penalized: &Arc<AtomicBool>,
    message: Message,
) -> bool {
    match message {
        Message::Text(text) => {
            let parsed = match serde_json::from_str::<ClientMessage>(&text) {
                Ok(message) => message,
                Err(error) => {
                    return enqueue_local_message(
                        outbound_tx,
                        penalized,
                        ServerMessage::Error {
                            message: format!("invalid client message: {error}"),
                        },
                    );
                }
            };

            match parsed {
                ClientMessage::PakeInit { .. }
                | ClientMessage::PakeRegistrationRequest { .. }
                | ClientMessage::PakeRegistrationUpload { .. }
                | ClientMessage::PakeCredentialFinalization { .. } => {
                    if !enqueue_local_message(
                        outbound_tx,
                        penalized,
                        ServerMessage::Error {
                            message: String::from("client already authenticated"),
                        },
                    ) {
                        return false;
                    }
                }
                ClientMessage::Signal { peer_id, payload } => {
                    match state
                        .forward_signal(room_handle, client_id, &peer_id, payload)
                        .await
                    {
                        Ok((peer, message)) => {
                            enqueue_remote_message(&peer, message);
                        }
                        Err(message) => {
                            if !enqueue_local_message(
                                outbound_tx,
                                penalized,
                                ServerMessage::Error { message },
                            ) {
                                return false;
                            }
                        }
                    }
                }
                ClientMessage::Ping => {
                    if !enqueue_local_message(outbound_tx, penalized, ServerMessage::Pong) {
                        return false;
                    }
                }
            }
            true
        }
        Message::Binary(_) => enqueue_local_message(
            outbound_tx,
            penalized,
            ServerMessage::Error {
                message: String::from("binary websocket frames are not supported"),
            },
        ),
        Message::Close(_) => false,
        Message::Ping(_) => enqueue_local_message(outbound_tx, penalized, ServerMessage::Pong),
        Message::Pong(_) => true,
    }
}

fn broadcast_all(messages: Vec<(ClientHandle, ServerMessage)>) {
    for (peer, message) in messages {
        enqueue_remote_message(&peer, message);
    }
}

fn enqueue_local_message(
    tx: &mpsc::Sender<ServerMessage>,
    penalized: &Arc<AtomicBool>,
    message: ServerMessage,
) -> bool {
    match tx.try_send(message) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            penalized.store(true, Ordering::Relaxed);
            false
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

fn enqueue_remote_message(peer: &ClientHandle, message: ServerMessage) {
    match peer.tx.try_send(message) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            peer.penalized.store(true, Ordering::Relaxed);
        }
        Err(TrySendError::Closed(_)) => {}
    }
}
