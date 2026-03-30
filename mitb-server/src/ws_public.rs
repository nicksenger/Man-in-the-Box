use crate::{AppState, PublicServerMessage};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::warn;

pub(crate) async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<PublicServerMessage>();
    state.subscribe_public(outbound_tx).await;

    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                let Ok(payload) = serde_json::to_string(&outbound) else {
                    continue;
                };
                if sender.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            inbound = receiver.next() => {
                match inbound {
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Text(_)))
                    | Some(Ok(Message::Binary(_)))
                    | Some(Ok(Message::Pong(_))) => {}
                    Some(Err(error)) => {
                        warn!(%error, "public websocket receive error");
                        break;
                    }
                }
            }
        }
    }
}
