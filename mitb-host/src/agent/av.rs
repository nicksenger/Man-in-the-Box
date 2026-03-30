use super::{AgentError, ClientMessage, SignalingPayload};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) struct State;

impl State {
    pub(super) async fn new(_server_addr: &str) -> Result<Self, AgentError> {
        Ok(Self)
    }

    pub(super) async fn handle_connected(
        &mut self,
        _api: &Arc<webrtc::api::API>,
        _signal_tx: &mpsc::UnboundedSender<ClientMessage>,
        _client_id: &str,
        _agents: &[String],
    ) -> Result<(), AgentError> {
        Ok(())
    }

    pub(super) async fn handle_peer_joined_agent(
        &mut self,
        _api: &Arc<webrtc::api::API>,
        _signal_tx: &mpsc::UnboundedSender<ClientMessage>,
        _client_id: &str,
    ) -> Result<(), AgentError> {
        Ok(())
    }

    pub(super) async fn handle_peer_left_agent(&mut self, _client_id: &str) {}

    pub(super) async fn handle_signal(
        &mut self,
        _api: &Arc<webrtc::api::API>,
        _signal_tx: &mpsc::UnboundedSender<ClientMessage>,
        _peer_id: &str,
        _payload: SignalingPayload,
    ) -> Result<bool, AgentError> {
        Ok(false)
    }

    pub(super) async fn close_all(&mut self) {}
}
