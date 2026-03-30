use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tracing::{debug, error, info, trace, warn};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

#[derive(Clone)]
pub(crate) struct PolicyLogWriter {
    state: Arc<Mutex<PolicyLogState>>,
}

struct PolicyLogState {
    stream: &'static str,
    pending: Vec<u8>,
}

#[derive(Clone)]
struct PolicyStdoutStream {
    writer: PolicyLogWriter,
}

impl PolicyLogWriter {
    pub(crate) fn new(stream: &'static str) -> Self {
        Self {
            state: Arc::new(Mutex::new(PolicyLogState {
                stream,
                pending: Vec::new(),
            })),
        }
    }

    fn write_bytes(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.pending.extend_from_slice(bytes);
        flush_complete_lines(&mut state);
    }

    fn flush_pending(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.pending.is_empty() {
            return;
        }

        let line = String::from_utf8_lossy(&state.pending)
            .trim_end()
            .to_string();
        state.pending.clear();
        if !line.is_empty() {
            emit_policy_log(state.stream, line.as_str());
        }
    }
}

impl PolicyStdoutStream {
    fn new(writer: PolicyLogWriter) -> Self {
        Self { writer }
    }
}

fn flush_complete_lines(state: &mut PolicyLogState) {
    while let Some(index) = state.pending.iter().position(|byte| *byte == b'\n') {
        let mut line = state.pending.drain(..=index).collect::<Vec<_>>();
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }

        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end();
        if !trimmed.is_empty() {
            emit_policy_log(state.stream, trimmed);
        }
    }
}

fn emit_policy_log(stream: &'static str, line: &str) {
    if let Some((level, message)) = parse_policy_log_line(line) {
        match level {
            PolicyLogLevel::Error => error!(target: "mitb_policy", stream, "{message}"),
            PolicyLogLevel::Warn => warn!(target: "mitb_policy", stream, "{message}"),
            PolicyLogLevel::Info => info!(target: "mitb_policy", stream, "{message}"),
            PolicyLogLevel::Debug => debug!(target: "mitb_policy", stream, "{message}"),
            PolicyLogLevel::Trace => trace!(target: "mitb_policy", stream, "{message}"),
        }
    } else if stream == "stderr" {
        warn!(target: "mitb_policy", stream, "{line}");
    } else {
        info!(target: "mitb_policy", stream, "{line}");
    }
}

#[derive(Clone, Copy)]
enum PolicyLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

fn parse_policy_log_line(line: &str) -> Option<(PolicyLogLevel, &str)> {
    const PREFIXES: [(&str, PolicyLogLevel); 5] = [
        ("ERROR ", PolicyLogLevel::Error),
        (" WARN ", PolicyLogLevel::Warn),
        (" INFO ", PolicyLogLevel::Info),
        ("DEBUG ", PolicyLogLevel::Debug),
        ("TRACE ", PolicyLogLevel::Trace),
    ];

    PREFIXES
        .iter()
        .find_map(|(prefix, level)| line.strip_prefix(prefix).map(|message| (*level, message)))
}

impl AsyncWrite for PolicyLogWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.write_bytes(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.flush_pending();
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.flush_pending();
        Poll::Ready(Ok(()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let mut written = 0;
        for buf in bufs {
            self.write_bytes(buf);
            written += buf.len();
        }
        Poll::Ready(Ok(written))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl IsTerminal for PolicyLogWriter {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for PolicyLogWriter {
    fn p2_stream(&self) -> Box<dyn wasmtime_wasi::p2::OutputStream> {
        Box::new(PolicyStdoutStream::new(self.clone()))
    }

    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

#[wasmtime_wasi::async_trait]
impl wasmtime_wasi::p2::Pollable for PolicyStdoutStream {
    async fn ready(&mut self) {}
}

impl wasmtime_wasi::p2::OutputStream for PolicyStdoutStream {
    fn write(&mut self, bytes: bytes::Bytes) -> wasmtime_wasi::p2::StreamResult<()> {
        self.writer.write_bytes(bytes.as_ref());
        Ok(())
    }

    fn flush(&mut self) -> wasmtime_wasi::p2::StreamResult<()> {
        self.writer.flush_pending();
        Ok(())
    }

    fn check_write(&mut self) -> wasmtime_wasi::p2::StreamResult<usize> {
        Ok(1024 * 1024)
    }
}
