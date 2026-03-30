use super::*;

pub(super) struct ServerMessageContext<'a> {
    pub(super) api: &'a Arc<webrtc::api::API>,
    pub(super) signal_tx: &'a mpsc::UnboundedSender<ClientMessage>,
    pub(super) reports: &'a ReportStore,
    pub(super) shutdown: &'a Arc<AtomicBool>,
    pub(super) event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
    pub(super) peers: &'a mut HashMap<String, AgentPeer>,
    pub(super) av_state: &'a mut av::State,
}

pub(super) async fn handle_server_message(
    message: ServerMessage,
    context: &mut ServerMessageContext<'_>,
) -> Result<(), AgentError> {
    if is_ignored_pake_message(&message) {
        warn!("ignoring unexpected PAKE message after authentication");
        return Ok(());
    }

    match message {
        ServerMessage::Connected {
            client_id,
            agents,
            zookeepers,
        } => handle_connected(context, client_id, agents, zookeepers).await?,
        ServerMessage::PeerJoined { role, client_id } => {
            handle_peer_joined(context, role, client_id).await?
        }
        ServerMessage::PeerLeft { role, client_id } => {
            handle_peer_left(context, role, client_id).await
        }
        ServerMessage::Signal { peer_id, payload } => {
            handle_signal(context, peer_id, payload).await?
        }
        ServerMessage::Error { message } => {
            warn!(%message, "signaling server returned an error");
        }
        ServerMessage::Pong => {
            debug!("received signaling pong");
        }
        ServerMessage::PakeRegistrationRequired
        | ServerMessage::PakeRegistrationResponse { .. }
        | ServerMessage::PakeCredentialCandidates { .. } => unreachable!(),
    }

    Ok(())
}

fn is_ignored_pake_message(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::PakeRegistrationRequired
            | ServerMessage::PakeRegistrationResponse { .. }
            | ServerMessage::PakeCredentialCandidates { .. }
    )
}

async fn handle_connected(
    context: &mut ServerMessageContext<'_>,
    client_id: String,
    agents: Vec<String>,
    zookeepers: Vec<String>,
) -> Result<(), AgentError> {
    debug!(
        %client_id,
        zookeepers = zookeepers.len(),
        "agent signaling session established"
    );
    context
        .av_state
        .handle_connected(context.api, context.signal_tx, client_id.as_str(), &agents)
        .await?;
    for zookeeper_id in zookeepers {
        ensure_peer(
            context.api,
            context.signal_tx,
            context.reports,
            context.shutdown,
            context.event_sender.clone(),
            context.peers,
            &zookeeper_id,
        )
        .await?;
    }
    Ok(())
}

async fn handle_peer_joined(
    context: &mut ServerMessageContext<'_>,
    role: ClientRole,
    client_id: String,
) -> Result<(), AgentError> {
    match role {
        ClientRole::Zookeeper => {
            ensure_peer(
                context.api,
                context.signal_tx,
                context.reports,
                context.shutdown,
                context.event_sender.clone(),
                context.peers,
                &client_id,
            )
            .await?;
        }
        ClientRole::Agent => {
            context
                .av_state
                .handle_peer_joined_agent(context.api, context.signal_tx, client_id.as_str())
                .await?;
        }
    }
    Ok(())
}

async fn handle_peer_left(
    context: &mut ServerMessageContext<'_>,
    role: ClientRole,
    client_id: String,
) {
    match role {
        ClientRole::Zookeeper => {
            if let Some(peer) = context.peers.remove(&client_id) {
                let _ = peer.connection.close().await;
            }
        }
        ClientRole::Agent => {
            context
                .av_state
                .handle_peer_left_agent(client_id.as_str())
                .await;
        }
    }
}

async fn handle_signal(
    context: &mut ServerMessageContext<'_>,
    peer_id: String,
    payload: SignalingPayload,
) -> Result<(), AgentError> {
    if let Some(peer) = context.peers.get(&peer_id) {
        match payload {
            SignalingPayload::Answer { sdp } => {
                peer.connection
                    .set_remote_description(RTCSessionDescription::answer(sdp)?)
                    .await?;
            }
            SignalingPayload::Offer { .. } => {
                warn!(peer_id = %peer_id, "ignoring unexpected offer on agent side");
            }
        }
        return Ok(());
    }

    let handled = context
        .av_state
        .handle_signal(context.api, context.signal_tx, peer_id.as_str(), payload)
        .await?;
    if !handled {
        warn!(peer_id = %peer_id, "ignoring signal for unknown peer");
    }
    Ok(())
}
