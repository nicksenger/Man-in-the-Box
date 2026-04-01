use crate::HostEvent;
use bytes::Bytes;
use ice::mdns::MulticastDnsMode;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use tracing::{debug, warn};
use url::Url;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

const DATA_CHANNEL_LABEL: &str = "mitb";
const REPORT_BUFFER_CAPACITY: usize = 1024;
const STUN_SERVER_URL: &str = "stun:stun.l.google.com:19302";
const RECONNECT_DELAY_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(30);

mod auth;
#[path = "agent/av.rs"]
mod av;
mod protocol;
mod signaling;
mod signaling_handlers;

use protocol::{
    AgentDataMessage, AgentReportMessage, ClientMessage, ClientRole, PakeCredentialCandidate,
    ServerMessage, SignalingPayload,
};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub server_addr: String,
    pub secret_code: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReportStore {
    history: Arc<Mutex<Vec<RewardReport>>>,
    tx: broadcast::Sender<RewardReport>,
}

#[derive(Debug, Clone, Copy)]
struct RewardReport {
    reward: f64,
    reported_at_ms: u64,
}

impl Default for ReportStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportStore {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(REPORT_BUFFER_CAPACITY);
        Self {
            history: Arc::new(Mutex::new(Vec::new())),
            tx,
        }
    }

    pub async fn publish(&self, reward: f64) -> Result<(), String> {
        validate_reward(reward)?;
        let report = RewardReport {
            reward,
            reported_at_ms: current_timestamp_ms(),
        };
        let mut history = self.history.lock().await;
        history.push(report);
        drop(history);
        let _ = self.tx.send(report);
        Ok(())
    }

    async fn snapshot(&self) -> Vec<RewardReport> {
        self.history.lock().await.clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<RewardReport> {
        self.tx.subscribe()
    }
}

#[derive(Debug)]
pub struct AgentRuntime {
    pub handle: JoinHandle<()>,
    pub events: mpsc::UnboundedReceiver<AgentRuntimeEvent>,
}

#[derive(Debug)]
pub enum AgentRuntimeEvent {
    Fatal(String),
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("invalid MITB_SERVER_ADDR `{0}`")]
    InvalidServerAddr(String),
    #[error("{0}")]
    Pake(String),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    WebRtc(#[from] webrtc::error::Error),
}

impl AgentError {
    fn is_retryable(&self) -> bool {
        matches!(self, Self::WebSocket(_))
    }
}

struct AgentPeer {
    connection: Arc<RTCPeerConnection>,
    data_channel: Arc<RTCDataChannel>,
}

pub fn spawn(
    options: AgentOptions,
    reports: ReportStore,
    shutdown: Arc<AtomicBool>,
    event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
) -> AgentRuntime {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        if let Err(error) = run(options, reports, shutdown, event_sender).await {
            let _ = event_tx.send(AgentRuntimeEvent::Fatal(error.to_string()));
        }
    });

    AgentRuntime {
        handle,
        events: event_rx,
    }
}

async fn run(
    options: AgentOptions,
    reports: ReportStore,
    shutdown: Arc<AtomicBool>,
    event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
) -> Result<(), AgentError> {
    let url = signaling_url(&options.server_addr)?;
    let api = build_webrtc_api()?;
    let mut reconnect_delay = RECONNECT_DELAY_INITIAL;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        debug!(
            server = %options.server_addr,
            "connecting host agent to reporting channel"
        );

        match signaling::run_signaling_session(
            &options,
            &url,
            &api,
            &reports,
            &shutdown,
            event_sender.clone(),
        )
        .await
        {
            Ok(()) => {
                reconnect_delay = RECONNECT_DELAY_INITIAL;
                if shutdown.load(Ordering::Relaxed) {
                    return Ok(());
                }

                debug!(
                    delay_secs = reconnect_delay.as_secs(),
                    "signaling session closed; reconnecting in background"
                );
            }
            Err(error) if error.is_retryable() && !shutdown.load(Ordering::Relaxed) => {
                debug!(
                    %error,
                    delay_secs = reconnect_delay.as_secs(),
                    "signaling session lost; reconnecting in background"
                );
            }
            Err(error) => return Err(error),
        }

        sleep(reconnect_delay).await;
        reconnect_delay = next_reconnect_delay(reconnect_delay);
    }
}

fn build_webrtc_api() -> Result<Arc<webrtc::api::API>, AgentError> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::QueryOnly);
    Ok(Arc::new(
        APIBuilder::new()
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .build(),
    ))
}

fn next_reconnect_delay(delay: Duration) -> Duration {
    std::cmp::min(delay.saturating_mul(2), RECONNECT_DELAY_MAX)
}

async fn ensure_peer(
    api: &Arc<webrtc::api::API>,
    signal_tx: &mpsc::UnboundedSender<ClientMessage>,
    reports: &ReportStore,
    shutdown: &Arc<AtomicBool>,
    event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
    peers: &mut HashMap<String, AgentPeer>,
    zookeeper_id: &str,
) -> Result<(), AgentError> {
    if peers.contains_key(zookeeper_id) {
        return Ok(());
    }

    let connection = Arc::new(api.new_peer_connection(peer_connection_config()).await?);
    install_peer_callbacks(
        &connection,
        zookeeper_id,
        reports,
        shutdown,
        event_sender.clone(),
    );

    let data_channel = connection
        .create_data_channel(DATA_CHANNEL_LABEL, None)
        .await?;
    install_data_channel_callbacks(
        &data_channel,
        zookeeper_id,
        reports,
        shutdown,
        event_sender.clone(),
    );

    let mut gathering_complete = connection.gathering_complete_promise().await;
    let offer = connection.create_offer(None).await?;
    connection.set_local_description(offer).await?;
    let _ = gathering_complete.recv().await;
    let Some(local_description) = connection.local_description().await else {
        return Err(AgentError::InvalidServerAddr(String::from(
            "missing local description after offer",
        )));
    };
    let _ = signal_tx.send(ClientMessage::Signal {
        peer_id: zookeeper_id.to_owned(),
        payload: SignalingPayload::Offer {
            sdp: local_description.sdp,
        },
    });

    peers.insert(
        zookeeper_id.to_owned(),
        AgentPeer {
            connection,
            data_channel,
        },
    );

    Ok(())
}

fn install_peer_callbacks(
    connection: &Arc<RTCPeerConnection>,
    zookeeper_id: &str,
    reports: &ReportStore,
    shutdown: &Arc<AtomicBool>,
    event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
) {
    let peer_id = zookeeper_id.to_owned();
    connection.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let peer_id = peer_id.clone();
        Box::pin(async move {
            debug!(peer_id = %peer_id, %state, "agent peer connection state changed");
        })
    }));

    let peer_id = zookeeper_id.to_owned();
    connection.on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
        let peer_id = peer_id.clone();
        Box::pin(async move {
            debug!(peer_id = %peer_id, %state, "agent ICE connection state changed");
        })
    }));

    let peer_id = zookeeper_id.to_owned();
    let reports = reports.clone();
    let shutdown = Arc::clone(shutdown);
    connection.on_data_channel(Box::new(move |channel| {
        let peer_id = peer_id.clone();
        let reports = reports.clone();
        let shutdown = Arc::clone(&shutdown);
        let event_sender = event_sender.clone();
        Box::pin(async move {
            if channel.label() == DATA_CHANNEL_LABEL {
                install_data_channel_callbacks(
                    &channel,
                    &peer_id,
                    &reports,
                    &shutdown,
                    event_sender,
                );
            }
        })
    }));
}

fn install_data_channel_callbacks(
    data_channel: &Arc<RTCDataChannel>,
    zookeeper_id: &str,
    reports: &ReportStore,
    shutdown: &Arc<AtomicBool>,
    event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
) {
    let replay_channel = Arc::clone(data_channel);
    let reports_for_open = reports.clone();
    data_channel.on_open(Box::new(move || {
        let replay_channel = Arc::clone(&replay_channel);
        let reports = reports_for_open.clone();
        Box::pin(async move {
            for message in reports.snapshot().await {
                if let Err(error) = send_report(&replay_channel, message).await {
                    warn!(%error, "failed replaying historical report");
                    break;
                }
            }
        })
    }));

    let zookeeper_id = zookeeper_id.to_owned();
    let shutdown = Arc::clone(shutdown);
    data_channel.on_message(Box::new(move |message: DataChannelMessage| {
        let zookeeper_id = zookeeper_id.clone();
        let shutdown = Arc::clone(&shutdown);
        let event_sender = event_sender.clone();
        Box::pin(async move {
            if !message.is_string {
                return;
            }

            let Ok(text) = std::str::from_utf8(&message.data) else {
                return;
            };

            match serde_json::from_str::<AgentDataMessage>(text) {
                Ok(AgentDataMessage::Terminate) => {
                    shutdown.store(true, Ordering::Relaxed);
                    if let Some(tx) = &event_sender {
                        let _ = tx.send(HostEvent::SessionEnded(format!(
                            "zookeeper `{zookeeper_id}` requested terminate"
                        )));
                    }
                }
                Err(error) => {
                    warn!(zookeeper_id = %zookeeper_id, %error, "failed to decode zookeeper data channel message");
                }
            }
        })
    }));
}

async fn broadcast_report(peers: &HashMap<String, AgentPeer>, report: RewardReport) {
    for (zookeeper_id, peer) in peers {
        if peer.data_channel.ready_state() != RTCDataChannelState::Open {
            continue;
        }

        if let Err(error) = send_report(&peer.data_channel, report).await {
            warn!(zookeeper_id = %zookeeper_id, %error, "failed sending report to zookeeper");
        }
    }
}

async fn send_report(
    data_channel: &Arc<RTCDataChannel>,
    report: RewardReport,
) -> Result<(), String> {
    let payload = serde_json::to_vec(&AgentReportMessage::Reward {
        reward: report.reward,
        reported_at_ms: report.reported_at_ms,
    })
    .map_err(|error| format!("failed serializing reward report: {error}"))?;
    data_channel
        .send(&Bytes::from(payload))
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn validate_reward(reward: f64) -> Result<(), String> {
    if !reward.is_finite() {
        return Err(String::from("reward must be finite"));
    }
    if !(0.0..=1.0).contains(&reward) {
        return Err(format!("reward must be in [0.0, 1.0], got {reward}"));
    }
    Ok(())
}

fn current_timestamp_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis().min(u128::from(u64::MAX)) as u64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::validate_reward;

    #[test]
    fn validate_reward_accepts_normalized_bounds() {
        assert!(validate_reward(0.0).is_ok());
        assert!(validate_reward(0.5).is_ok());
        assert!(validate_reward(1.0).is_ok());
    }

    #[test]
    fn validate_reward_rejects_out_of_range_values() {
        assert!(validate_reward(-0.1).is_err());
        assert!(validate_reward(1.1).is_err());
    }

    #[test]
    fn validate_reward_rejects_non_finite_values() {
        assert!(validate_reward(f64::NAN).is_err());
        assert!(validate_reward(f64::INFINITY).is_err());
        assert!(validate_reward(f64::NEG_INFINITY).is_err());
    }
}

fn peer_connection_config() -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec![String::from(STUN_SERVER_URL)],
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn signaling_url(server_addr: &str) -> Result<Url, AgentError> {
    let normalized = if server_addr.starts_with("wss://") {
        server_addr.to_owned()
    } else if let Some(rest) = server_addr.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server_addr.strip_prefix("ws://") {
        format!("ws://{rest}")
    } else if let Some(rest) = server_addr.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if server_addr.contains("://") {
        return Err(AgentError::InvalidServerAddr(server_addr.to_owned()));
    } else {
        format!("ws://{server_addr}")
    };

    let mut url = Url::parse(&normalized)?;
    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/ws");
    }
    Ok(url)
}
