use super::*;
use futures_util::{SinkExt, StreamExt};
use mitb_pake::{ClientSession as PakeClientSession, decode, encode};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub(super) async fn authenticate_websocket<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
    options: &AgentOptions,
) -> Result<ServerMessage, AgentError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut client = PakeClientSession::new(&options.secret_code).map_err(AgentError::Pake)?;
    let login = client.start_login().map_err(AgentError::Pake)?;
    send_ws_message(
        websocket,
        &ClientMessage::PakeInit {
            role: ClientRole::Agent,
            room_handle: derive_room_handle(&options.secret_code),
            credential_request: encode(&login.credential_request),
            client_alias: options.alias.clone(),
        },
    )
    .await?;

    while let Some(message) = websocket.next().await {
        match message {
            Ok(WsMessage::Text(text)) => {
                let message = serde_json::from_str::<ServerMessage>(&text)?;
                match message {
                    ServerMessage::PakeRegistrationRequired => {
                        let registration = client.start_registration().map_err(AgentError::Pake)?;
                        send_ws_message(
                            websocket,
                            &ClientMessage::PakeRegistrationRequest {
                                registration_request: encode(&registration.registration_request),
                            },
                        )
                        .await?;
                    }
                    ServerMessage::PakeRegistrationResponse {
                        registration_response,
                    } => {
                        let registration = client
                            .finish_registration(
                                &decode(&registration_response).map_err(AgentError::Pake)?,
                            )
                            .map_err(AgentError::Pake)?;
                        send_ws_message(
                            websocket,
                            &ClientMessage::PakeRegistrationUpload {
                                registration_upload: encode(&registration.registration_upload),
                                credential_request: encode(&registration.credential_request),
                            },
                        )
                        .await?;
                    }
                    ServerMessage::PakeCredentialCandidates { candidates } => {
                        let (candidate_id, login) =
                            select_login_candidate(&mut client, candidates)?;
                        send_ws_message(
                            websocket,
                            &ClientMessage::PakeCredentialFinalization {
                                candidate_id,
                                credential_finalization: encode(&login.credential_finalization),
                            },
                        )
                        .await?;
                    }
                    connected @ ServerMessage::Connected { .. } => return Ok(connected),
                    ServerMessage::Error { message } => return Err(AgentError::Pake(message)),
                    ServerMessage::Pong => {}
                    other => {
                        return Err(AgentError::Pake(format!(
                            "unexpected server message during authentication: {other:?}"
                        )));
                    }
                }
            }
            Ok(WsMessage::Ping(payload)) => {
                websocket.send(WsMessage::Pong(payload)).await?;
            }
            Ok(WsMessage::Pong(_)) => {}
            Ok(WsMessage::Close(_)) | Ok(WsMessage::Frame(_)) => {
                return Err(AgentError::Pake(String::from(
                    "signaling socket closed during authentication",
                )));
            }
            Ok(WsMessage::Binary(_)) => {
                return Err(AgentError::Pake(String::from(
                    "unexpected binary websocket frame during authentication",
                )));
            }
            Err(error) => return Err(AgentError::WebSocket(error)),
        }
    }

    Err(AgentError::Pake(String::from(
        "signaling socket closed during authentication",
    )))
}

fn derive_room_handle(secret_code: &str) -> String {
    let hash = Sha256::digest(secret_code.as_bytes());
    let mut handle = String::from("room-");
    for byte in hash.iter().take(12) {
        use std::fmt::Write as _;
        let _ = write!(&mut handle, "{byte:02x}");
    }
    handle
}

async fn send_ws_message<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: &ClientMessage,
) -> Result<(), AgentError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let text = serde_json::to_string(message)?;
    websocket.send(WsMessage::Text(text.into())).await?;
    Ok(())
}

fn select_login_candidate(
    client: &mut PakeClientSession,
    candidates: Vec<PakeCredentialCandidate>,
) -> Result<(String, mitb_pake::ClientLoginFinish), AgentError> {
    let mut last_error = None;

    for candidate in candidates {
        let credential_response =
            decode(&candidate.credential_response).map_err(AgentError::Pake)?;
        match client.finish_login(&credential_response) {
            Ok(login) => return Ok((candidate.candidate_id, login)),
            Err(error) => last_error = Some(error),
        }
    }

    Err(AgentError::Pake(
        last_error.unwrap_or_else(|| String::from("unknown secret code")),
    ))
}
