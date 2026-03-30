use crate::{
    AppState, ClientMessage, ClientRole, CreatedRoom, PakeCredentialCandidate, PendingLogin,
    ServerMessage, WS_AUTH_READ_TIMEOUT, random_token,
};
use axum::extract::ws::{Message, WebSocket};
use mitb_pake::{ServerLoginSession, decode, encode};
use std::net::IpAddr;
use tokio::time::timeout;
use tracing::warn;

pub(crate) async fn authenticate_socket(
    state: &AppState,
    socket: &mut WebSocket,
    remote_ip: IpAddr,
) -> Option<(String, ClientRole, Option<String>)> {
    if let Err(error) = state.check_auth_rate_limit(remote_ip).await {
        send_socket_error(socket, error).await;
        return None;
    }

    let (role, room_handle, credential_request, client_alias) =
        match receive_message(socket).await? {
            ClientMessage::PakeInit {
                role,
                room_handle,
                credential_request,
                client_alias,
            } => {
                let room_handle = match normalize_room_handle(room_handle) {
                    Ok(room_handle) => room_handle,
                    Err(error) => {
                        send_socket_error(socket, error).await;
                        return None;
                    }
                };
                (
                    role,
                    room_handle,
                    credential_request,
                    client_alias
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty()),
                )
            }
            _ => {
                send_socket_error(
                    socket,
                    String::from("expected PAKE init message before other client messages"),
                )
                .await;
                return None;
            }
        };

    let credential_request = match decode(&credential_request) {
        Ok(bytes) => bytes,
        Err(error) => {
            send_socket_error(socket, error).await;
            return None;
        }
    };

    let mut pending_logins = Vec::new();
    let existing_pending = match state
        .candidate_login_for_room(&room_handle, &credential_request)
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            send_socket_error(socket, error).await;
            return None;
        }
    };
    if let Some(pending) = existing_pending {
        pending_logins.push(pending);
    }
    let mut created_room = None;

    if pending_logins.is_empty() {
        if role == ClientRole::Zookeeper {
            send_socket_error(socket, String::from("unknown secret code")).await;
            return None;
        }
        if !state.allows_agent_room_creation() {
            send_socket_error(
                socket,
                String::from("agent-driven room creation is disabled by server policy"),
            )
            .await;
            return None;
        }
        if let Err(error) = state.ensure_room_capacity().await {
            send_socket_error(socket, error).await;
            return None;
        }

        let _ = send_socket_message(socket, &ServerMessage::PakeRegistrationRequired).await;

        let registration_request = match receive_message(socket).await? {
            ClientMessage::PakeRegistrationRequest {
                registration_request,
            } => match decode(&registration_request) {
                Ok(bytes) => bytes,
                Err(error) => {
                    send_socket_error(socket, error).await;
                    return None;
                }
            },
            _ => {
                send_socket_error(socket, String::from("expected PAKE registration request")).await;
                return None;
            }
        };

        let registration_response = match state
            .pake
            .registration_response(room_handle.as_bytes(), &registration_request)
        {
            Ok(bytes) => bytes,
            Err(error) => {
                send_socket_error(socket, error).await;
                return None;
            }
        };
        let _ = send_socket_message(
            socket,
            &ServerMessage::PakeRegistrationResponse {
                registration_response: encode(&registration_response),
            },
        )
        .await;

        let (registration_upload, credential_request) = match receive_message(socket).await? {
            ClientMessage::PakeRegistrationUpload {
                registration_upload,
                credential_request,
            } => {
                let registration_upload = match decode(&registration_upload) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        send_socket_error(socket, error).await;
                        return None;
                    }
                };
                let credential_request = match decode(&credential_request) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        send_socket_error(socket, error).await;
                        return None;
                    }
                };
                (registration_upload, credential_request)
            }
            _ => {
                send_socket_error(socket, String::from("expected PAKE registration upload")).await;
                return None;
            }
        };

        let password_file = match state.pake.finish_registration(&registration_upload) {
            Ok(bytes) => bytes,
            Err(error) => {
                send_socket_error(socket, error).await;
                return None;
            }
        };
        let (response, login_session) = match state.pake.start_login(
            room_handle.as_bytes(),
            &password_file,
            &credential_request,
        ) {
            Ok(result) => result,
            Err(error) => {
                send_socket_error(socket, error).await;
                return None;
            }
        };

        let candidate_id = match random_token() {
            Ok(candidate_id) => candidate_id,
            Err(error) => {
                send_socket_error(socket, error).await;
                return None;
            }
        };
        pending_logins.push(PendingLogin {
            candidate_id: candidate_id.clone(),
            room_handle: room_handle.clone(),
            credential_response: response,
            login_session,
        });
        created_room = Some(CreatedRoom {
            room_handle,
            password_file,
        });
    }

    let candidates = pending_logins
        .iter()
        .map(|pending| PakeCredentialCandidate {
            candidate_id: pending.candidate_id.clone(),
            credential_response: encode(&pending.credential_response),
        })
        .collect::<Vec<_>>();
    let _ = send_socket_message(
        socket,
        &ServerMessage::PakeCredentialCandidates { candidates },
    )
    .await;

    let (candidate_id, credential_finalization) = match receive_message(socket).await? {
        ClientMessage::PakeCredentialFinalization {
            candidate_id,
            credential_finalization,
        } => match decode(&credential_finalization) {
            Ok(bytes) => (candidate_id, bytes),
            Err(error) => {
                send_socket_error(socket, error).await;
                return None;
            }
        },
        _ => {
            send_socket_error(
                socket,
                String::from("expected PAKE credential finalization"),
            )
            .await;
            return None;
        }
    };

    let Some(pending_login) = pending_logins
        .into_iter()
        .find(|pending| pending.candidate_id == candidate_id)
    else {
        send_socket_error(socket, String::from("unknown PAKE login candidate")).await;
        return None;
    };

    if let Err(error) = finish_login(pending_login.login_session, &credential_finalization).await {
        send_socket_error(socket, error).await;
        return None;
    }

    if let Some(created_room) = created_room
        && created_room.room_handle == pending_login.room_handle
        && let Err(error) = state
            .store_room(created_room.room_handle.clone(), created_room.password_file)
            .await
    {
        send_socket_error(socket, error).await;
        return None;
    }

    let client_alias = if role == ClientRole::Agent {
        client_alias
    } else {
        None
    };

    Some((pending_login.room_handle, role, client_alias))
}

async fn finish_login(
    login_session: ServerLoginSession,
    credential_finalization: &[u8],
) -> Result<(), String> {
    login_session.finish(credential_finalization)
}

async fn receive_message(socket: &mut WebSocket) -> Option<ClientMessage> {
    let message = match timeout(WS_AUTH_READ_TIMEOUT, socket.recv()).await {
        Ok(message) => message,
        Err(_) => {
            send_socket_error(
                socket,
                format!(
                    "authentication timed out waiting for next message ({}s)",
                    WS_AUTH_READ_TIMEOUT.as_secs()
                ),
            )
            .await;
            return None;
        }
    };

    let message = match message {
        Some(Ok(message)) => message,
        Some(Err(error)) => {
            warn!(%error, "websocket receive error before authentication");
            return None;
        }
        None => return None,
    };

    match message {
        Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text) {
            Ok(message) => Some(message),
            Err(error) => {
                send_socket_error(socket, format!("invalid client message: {error}")).await;
                None
            }
        },
        Message::Close(_) => None,
        Message::Ping(payload) => {
            let _ = socket.send(Message::Pong(payload)).await;
            send_socket_error(
                socket,
                String::from("expected text websocket frames during authentication"),
            )
            .await;
            None
        }
        Message::Pong(_) | Message::Binary(_) => {
            send_socket_error(
                socket,
                String::from("expected text websocket frames during authentication"),
            )
            .await;
            None
        }
    }
}

async fn send_socket_message(
    socket: &mut WebSocket,
    message: &ServerMessage,
) -> Result<(), String> {
    let payload = serde_json::to_string(message).map_err(|error| error.to_string())?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .map_err(|error| error.to_string())
}

async fn send_socket_error(socket: &mut WebSocket, message: String) {
    let _ = send_socket_message(socket, &ServerMessage::Error { message }).await;
}

fn normalize_room_handle(room_handle: String) -> Result<String, String> {
    const MAX_ROOM_HANDLE_LEN: usize = 64;

    let room_handle = room_handle.trim();
    if room_handle.is_empty() {
        return Err(String::from("room handle must not be empty"));
    }
    if room_handle.len() > MAX_ROOM_HANDLE_LEN {
        return Err(format!(
            "room handle must be at most {MAX_ROOM_HANDLE_LEN} characters"
        ));
    }
    if !room_handle
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(String::from(
            "room handle must use only letters, numbers, '-' or '_'",
        ));
    }

    Ok(room_handle.to_owned())
}

#[cfg(test)]
mod tests {
    use super::normalize_room_handle;

    #[test]
    fn normalize_room_handle_accepts_expected_format() {
        let normalized = normalize_room_handle(String::from("room-abc123_DEF"));
        assert_eq!(normalized, Ok(String::from("room-abc123_DEF")));
    }

    #[test]
    fn normalize_room_handle_rejects_empty_and_invalid_values() {
        assert!(normalize_room_handle(String::from("   ")).is_err());
        assert!(normalize_room_handle(String::from("room with spaces")).is_err());
        assert!(normalize_room_handle(String::from("room/slash")).is_err());
    }
}
