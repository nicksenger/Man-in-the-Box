use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ClientMessage {
    PakeInit {
        role: ClientRole,
        room_handle: String,
        credential_request: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_alias: Option<String>,
    },
    PakeRegistrationRequest {
        registration_request: String,
    },
    PakeRegistrationUpload {
        registration_upload: String,
        credential_request: String,
    },
    PakeCredentialFinalization {
        candidate_id: String,
        credential_finalization: String,
    },
    Signal {
        peer_id: String,
        payload: SignalingPayload,
    },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ServerMessage {
    PakeRegistrationRequired,
    PakeRegistrationResponse {
        registration_response: String,
    },
    PakeCredentialCandidates {
        candidates: Vec<PakeCredentialCandidate>,
    },
    Connected {
        client_id: String,
        agents: Vec<String>,
        zookeepers: Vec<String>,
    },
    PeerJoined {
        role: ClientRole,
        client_id: String,
    },
    PeerLeft {
        role: ClientRole,
        client_id: String,
    },
    Signal {
        peer_id: String,
        payload: SignalingPayload,
    },
    Error {
        message: String,
    },
    Pong,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PakeCredentialCandidate {
    pub(super) candidate_id: String,
    pub(super) credential_response: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ClientRole {
    Agent,
    Zookeeper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum SignalingPayload {
    Offer { sdp: String },
    Answer { sdp: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum AgentDataMessage {
    Terminate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum AgentReportMessage {
    Reward { reward: f64, reported_at_ms: u64 },
}
