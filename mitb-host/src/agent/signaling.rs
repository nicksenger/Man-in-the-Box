use super::signaling_handlers::{ServerMessageContext, handle_server_message};
use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::time::interval;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub(super) async fn run_signaling_session(
    options: &AgentOptions,
    url: &Url,
    api: &Arc<webrtc::api::API>,
    reports: &ReportStore,
    shutdown: &Arc<AtomicBool>,
    event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
) -> Result<(), AgentError> {
    let (mut websocket, _) = connect_async(url.as_str()).await?;
    let initial_message = auth::authenticate_websocket(&mut websocket, options).await?;
    let (mut ws_write, mut ws_read) = websocket.split();
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let writer = tokio::spawn(async move {
        while let Some(message) = signal_rx.recv().await {
            let text = serde_json::to_string(&message)?;
            ws_write.send(WsMessage::Text(text.into())).await?;
        }
        Ok::<(), AgentError>(())
    });

    let mut peers = HashMap::<String, AgentPeer>::new();
    let mut av_state = av::State::new(options.server_addr.as_str()).await?;
    let mut report_rx = reports.subscribe();
    let mut shutdown_tick = interval(Duration::from_millis(100));

    debug!(
        server = %options.server_addr,
        "host agent connected to reporting channel"
    );

    let mut context = ServerMessageContext {
        api,
        signal_tx: &signal_tx,
        reports,
        shutdown,
        event_sender: event_sender.clone(),
        peers: &mut peers,
        av_state: &mut av_state,
    };
    handle_server_message(initial_message, &mut context).await?;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        if matches!(
            process_next_loop_event(
                &mut ws_read,
                &mut report_rx,
                &mut shutdown_tick,
                &mut context
            )
            .await?,
            SessionDirective::Break
        ) {
            break;
        }
    }

    drop(context);
    for (_, peer) in peers.drain() {
        let _ = peer.connection.close().await;
    }
    av_state.close_all().await;

    writer.abort();
    Ok(())
}

enum SessionDirective {
    Continue,
    Break,
}

async fn process_next_loop_event(
    ws_read: &mut (
             impl futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
             + Unpin
         ),
    report_rx: &mut broadcast::Receiver<RewardReport>,
    shutdown_tick: &mut tokio::time::Interval,
    context: &mut ServerMessageContext<'_>,
) -> Result<SessionDirective, AgentError> {
    tokio::select! {
        message = ws_read.next() => handle_websocket_event(message, context).await,
        report = report_rx.recv() => Ok(handle_report_event(report, &*context.peers).await),
        _ = shutdown_tick.tick() => Ok(SessionDirective::Continue),
    }
}

async fn handle_websocket_event(
    message: Option<Result<WsMessage, tokio_tungstenite::tungstenite::Error>>,
    context: &mut ServerMessageContext<'_>,
) -> Result<SessionDirective, AgentError> {
    match message {
        Some(Ok(WsMessage::Text(text))) => {
            let server_message = serde_json::from_str::<ServerMessage>(&text)?;
            handle_server_message(server_message, context).await?;
            Ok(SessionDirective::Continue)
        }
        Some(Ok(WsMessage::Close(_))) | None => Ok(SessionDirective::Break),
        Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => {
            Ok(SessionDirective::Continue)
        }
        Some(Ok(WsMessage::Binary(_))) => {
            warn!("ignoring binary websocket frame from signaling server");
            Ok(SessionDirective::Continue)
        }
        Some(Ok(WsMessage::Frame(_))) => Ok(SessionDirective::Continue),
        Some(Err(error)) => Err(AgentError::WebSocket(error)),
    }
}

async fn handle_report_event(
    report: Result<RewardReport, broadcast::error::RecvError>,
    peers: &HashMap<String, AgentPeer>,
) -> SessionDirective {
    match report {
        Ok(message) => {
            broadcast_report(peers, message).await;
            SessionDirective::Continue
        }
        Err(broadcast::error::RecvError::Lagged(count)) => {
            warn!(lagged = count, "agent report stream lagged");
            SessionDirective::Continue
        }
        Err(broadcast::error::RecvError::Closed) => SessionDirective::Break,
    }
}
