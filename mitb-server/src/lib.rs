use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, Response};
use axum::routing::get;
use mitb_pake::{ServerLoginSession, ServerPake, encode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

mod app_state;
mod ws_auth;
mod ws_runtime;

pub(crate) const WS_READ_BUFFER_SIZE: usize = 32 * 1024;
pub(crate) const WS_WRITE_BUFFER_SIZE: usize = 32 * 1024;
pub(crate) const WS_MAX_WRITE_BUFFER_SIZE: usize = 256 * 1024;
pub(crate) const WS_MAX_MESSAGE_SIZE: usize = 16 * 1024;
pub(crate) const WS_MAX_FRAME_SIZE: usize = 16 * 1024;
pub(crate) const WS_OUTBOUND_QUEUE_CAPACITY: usize = 256;
pub(crate) const WS_RUNTIME_READ_TIMEOUT: Duration = Duration::from_secs(90);
pub(crate) const WS_AUTH_READ_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const WS_AUTH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const WS_PENALTY_CHECK_INTERVAL: Duration = Duration::from_secs(1);

const DEFAULT_ALLOW_AGENT_ROOM_CREATION: bool = false;
const DEFAULT_MAX_ROOMS: usize = 512;
const DEFAULT_ROOM_TTL_SECS: u64 = 3600;
const DEFAULT_AUTH_RATE_LIMIT: usize = 30;
const DEFAULT_AUTH_RATE_WINDOW_SECS: u64 = 60;
const DEFAULT_ROOM_SIGNAL_RATE_LIMIT: usize = 300;
const DEFAULT_ROOM_SIGNAL_RATE_WINDOW_SECS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientRole {
    Agent,
    Zookeeper,
}

impl ClientRole {
    fn as_prefix(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Zookeeper => "zookeeper",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    PakeInit {
        role: ClientRole,
        room_handle: String,
        credential_request: String,
        #[serde(default)]
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
        payload: Value,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    PakeRegistrationRequired,
    PakeRegistrationResponse {
        registration_response: String,
    },
    PakeCredentialCandidates {
        candidates: Vec<PakeCredentialCandidate>,
    },
    Connected {
        client_id: String,
        role: ClientRole,
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
        payload: Value,
    },
    Error {
        message: String,
    },
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PakeCredentialCandidate {
    candidate_id: String,
    credential_response: String,
}

#[derive(Debug, Clone)]
struct ClientHandle {
    tx: mpsc::Sender<ServerMessage>,
    penalized: Arc<AtomicBool>,
}

#[derive(Debug)]
struct RoomState {
    password_file: Vec<u8>,
    agents: HashMap<String, ClientHandle>,
    zookeepers: HashMap<String, ClientHandle>,
    last_active_at: Instant,
    signal_events: VecDeque<Instant>,
}

impl Default for RoomState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            password_file: Vec::new(),
            agents: HashMap::new(),
            zookeepers: HashMap::new(),
            last_active_at: now,
            signal_events: VecDeque::new(),
        }
    }
}

#[derive(Debug, Default)]
struct StateInner {
    next_client_id: u64,
    rooms: HashMap<String, RoomState>,
    auth_events_by_ip: HashMap<IpAddr, VecDeque<Instant>>,
}

#[derive(Debug, Clone)]
struct SecurityConfig {
    allow_agent_room_creation: bool,
    max_rooms: usize,
    room_ttl: Duration,
    auth_rate_limit: usize,
    auth_rate_window: Duration,
    room_signal_rate_limit: usize,
    room_signal_rate_window: Duration,
}

impl SecurityConfig {
    fn from_env() -> Self {
        Self {
            allow_agent_room_creation: parse_env_bool(
                "MITB_ALLOW_AGENT_ROOM_CREATION",
                DEFAULT_ALLOW_AGENT_ROOM_CREATION,
            ),
            max_rooms: parse_env_usize("MITB_MAX_ROOMS", DEFAULT_MAX_ROOMS).max(1),
            room_ttl: Duration::from_secs(parse_env_u64(
                "MITB_ROOM_TTL_SECS",
                DEFAULT_ROOM_TTL_SECS,
            )),
            auth_rate_limit: parse_env_usize("MITB_AUTH_RATE_LIMIT", DEFAULT_AUTH_RATE_LIMIT)
                .max(1),
            auth_rate_window: Duration::from_secs(parse_env_u64(
                "MITB_AUTH_RATE_WINDOW_SECS",
                DEFAULT_AUTH_RATE_WINDOW_SECS,
            )),
            room_signal_rate_limit: parse_env_usize(
                "MITB_ROOM_SIGNAL_RATE_LIMIT",
                DEFAULT_ROOM_SIGNAL_RATE_LIMIT,
            )
            .max(1),
            room_signal_rate_window: Duration::from_secs(parse_env_u64(
                "MITB_ROOM_SIGNAL_RATE_WINDOW_SECS",
                DEFAULT_ROOM_SIGNAL_RATE_WINDOW_SECS,
            )),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Mutex<StateInner>>,
    pake: Arc<ServerPake>,
    config: Arc<SecurityConfig>,
}

#[derive(Debug)]
struct RegisterOutcome {
    client_id: String,
    initial_messages: Vec<ServerMessage>,
    broadcasts: Vec<(ClientHandle, ServerMessage)>,
}

#[derive(Debug)]
struct UnregisterOutcome {
    broadcasts: Vec<(ClientHandle, ServerMessage)>,
}

struct PendingLogin {
    candidate_id: String,
    room_handle: String,
    credential_response: Vec<u8>,
    login_session: ServerLoginSession,
}

#[derive(Debug)]
struct CreatedRoom {
    room_handle: String,
    password_file: Vec<u8>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner::default())),
            pake: Arc::new(ServerPake::new()),
            config: Arc::new(SecurityConfig::from_env()),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .route("/assets/mitb-pake.wasm", get(pake_wasm))
        .route("/", get(index))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn pake_wasm() -> Response {
    match Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/wasm"),
        )
        .body(axum::body::Body::from(
            include_bytes!(concat!(env!("OUT_DIR"), "/mitb-pake.wasm"))
                .as_slice()
                .to_vec(),
        )) {
        Ok(response) => response,
        Err(_) => {
            let mut response = Response::new(axum::body::Body::from(Vec::new()));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        }
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    ws.read_buffer_size(WS_READ_BUFFER_SIZE)
        .write_buffer_size(WS_WRITE_BUFFER_SIZE)
        .max_write_buffer_size(WS_MAX_WRITE_BUFFER_SIZE)
        .max_message_size(WS_MAX_MESSAGE_SIZE)
        .max_frame_size(WS_MAX_FRAME_SIZE)
        .on_upgrade(move |socket| ws_runtime::handle_socket(state, socket, remote_addr))
}

pub(crate) fn random_token() -> Result<String, String> {
    let mut bytes = [0_u8; 18];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    Ok(encode(&bytes))
}

fn parse_env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

fn parse_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn parse_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}
