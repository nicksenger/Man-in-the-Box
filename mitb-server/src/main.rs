use clap::Parser;
use mitb_server::{AppState, router};
use std::net::SocketAddr;
use tokio::runtime::Builder;
use tracing::info;
use tracing_subscriber::EnvFilter;
use url::Url;

const SERVER_THREADS_ENV: &str = "MITB_SERVER_THREADS";
const SERVER_BIND_ADDR_ENV: &str = "MITB_SERVER_BIND_ADDR";

#[derive(Debug, Parser)]
#[command(name = "mitb-server")]
#[command(about = "Serve the Man in the Box web app and signaling channels")]
struct Cli {
    #[arg(long, env = SERVER_BIND_ADDR_ENV, default_value = "0.0.0.0:3000")]
    addr: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let cli = Cli::parse();
    let bind_addr = normalize_bind_addr(&cli.addr)?;
    let worker_threads = worker_threads_from_env()?;
    let runtime = if worker_threads == 1 {
        Builder::new_current_thread().enable_all().build()?
    } else {
        Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .enable_all()
            .build()?
    };

    runtime.block_on(async_main(bind_addr, worker_threads))
}

async fn async_main(
    bind_addr: String,
    worker_threads: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState::new();
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    let runtime = if worker_threads == 1 {
        "current_thread"
    } else {
        "multi_thread"
    };

    info!(
        addr = %bind_addr,
        runtime,
        worker_threads,
        "starting mitb-server; configure TLS termination externally if needed"
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "failed to wait for shutdown signal");
    }
}

fn init_logging() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::new("mitb_server=debug,info"),
    };

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .finish();

    if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("failed to initialize tracing subscriber: {error}");
    }
}

fn worker_threads_from_env() -> Result<usize, Box<dyn std::error::Error>> {
    let default_threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let Some(value) = std::env::var(SERVER_THREADS_ENV).ok() else {
        return Ok(default_threads);
    };

    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|error| format!("invalid {SERVER_THREADS_ENV}: {error}"))?;
    if parsed == 0 {
        return Err(format!("{SERVER_THREADS_ENV} must be >= 1").into());
    }
    Ok(parsed)
}

fn normalize_bind_addr(raw: &str) -> Result<String, Box<dyn std::error::Error>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(String::from("bind address must not be empty").into());
    }
    if !trimmed.contains("://") {
        return Ok(trimmed.to_owned());
    }

    let url = Url::parse(trimmed).map_err(|error| format!("invalid bind URL: {error}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| String::from("bind URL is missing host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| String::from("bind URL is missing port"))?;
    Ok(format!("{host}:{port}"))
}
