mod agent;
mod error;
mod policy_log;
mod pty_io;
mod runtime;
mod stream_bridge;
mod terminal_state;
mod transcript;

pub use agent::{AgentOptions, ReportStore};
pub use error::HostError;
use policy_log::PolicyLogWriter;
use pty_io::{
    PtyWriter, forward_keyboard_input, read_pty_output, write_inputs as write_pty_inputs,
};
use stream_bridge::{MpscStreamConsumer, MpscStreamProducer};
use transcript::{transcript_bytes_since, transcript_head, transcript_snapshot_bytes};

pub struct ProcessChild {
    stdin_tx: mpsc::Sender<u8>,
    stdout_rx: Mutex<Option<mpsc::Receiver<u8>>>,
    stdout_done: watch::Receiver<Option<Result<(), String>>>,
    stderr_rx: Mutex<Option<mpsc::Receiver<u8>>>,
    stderr_done: watch::Receiver<Option<Result<(), String>>>,
    control: Arc<ChildControl>,
}

mod bindings {
    include!(concat!(env!("OUT_DIR"), "/mitb_bindgen.rs"));
}

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::process::{Child as TokioChild, ChildStdin};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, trace};
use wasmtime::component::{FutureReader, Resource, ResourceTable, StreamReader};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::{
    WasiHttpCtx as WasiHttpP3Ctx, WasiHttpCtxView as WasiHttpP3CtxView,
    WasiHttpView as WasiHttpP3View,
};

const MITB_HOME_DIR_ENV: &str = "MITB_HOME_DIR";
const MITB_SHARED_ROOT_ENV: &str = "MITB_SHARED_ROOT";
const MITB_ALIAS_ENV: &str = "MITB_ALIAS";
const MITB_SHARED_ROOT_GUEST_PATH: &str = "mitb-shared";
const PTY_COLS: u16 = 72;
const PTY_ROWS: u16 = 27;
const PTY_PIXEL_WIDTH: u16 = 640;
const PTY_PIXEL_HEIGHT: u16 = 480;

#[derive(Debug, Clone)]
pub enum HostEvent {
    KeyboardInput(Vec<u8>),
    TerminalOutput(Vec<u8>),
    SessionEnded(String),
    Disconnected,
}

#[derive(Debug)]
pub struct HostOptions {
    pub policy_component: PathBuf,
    pub command: String,
    pub command_args: Vec<String>,
    pub disable_spawn: bool,
    pub disable_networking: bool,
    pub disable_filesystem: bool,
    pub alias: Option<String>,
    pub poll_interval: Duration,
    pub max_transcript_bytes: usize,
    pub event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
    pub keyboard_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    pub shutdown: Option<Arc<AtomicBool>>,
    pub agent: Option<AgentOptions>,
}

impl HostOptions {
    pub fn new(policy_component: PathBuf, command: String, command_args: Vec<String>) -> Self {
        Self {
            policy_component,
            command,
            command_args,
            disable_spawn: false,
            disable_networking: false,
            disable_filesystem: false,
            alias: None,
            poll_interval: Duration::from_secs(5),
            max_transcript_bytes: 512 * 1024,
            event_sender: None,
            keyboard_rx: None,
            shutdown: None,
            agent: None,
        }
    }
}

struct StoreState {
    wasi: WasiCtx,
    http: HostHttpCtx,
    table: ResourceTable,
    report_store: ReportStore,
    transcript: SharedTranscript,
    disable_spawn: bool,
}

type SharedTranscript = Arc<Mutex<TranscriptBuffer>>;

#[derive(Debug, Default)]
struct TranscriptBuffer {
    start: u64,
    bytes: Vec<u8>,
}

struct ChildControl {
    child: tokio::sync::Mutex<TokioChild>,
    exit: tokio::sync::Mutex<Option<Result<bindings::mitb::host::types::ExitStatus, String>>>,
    exit_notify: Notify,
}

const CHILD_WAIT_POLL: Duration = Duration::from_millis(50);
impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpP3View for StoreState {
    fn http(&mut self) -> WasiHttpP3CtxView<'_> {
        WasiHttpP3CtxView {
            ctx: &mut self.http,
            table: &mut self.table,
        }
    }
}

#[derive(Clone, Default)]
struct HostHttpCtx {
    disable_networking: bool,
}

impl HostHttpCtx {
    fn new(disable_networking: bool) -> Self {
        Self { disable_networking }
    }
}

impl WasiHttpP3Ctx for HostHttpCtx {
    fn is_supported_scheme(&mut self, scheme: &http::uri::Scheme) -> bool {
        if self.disable_networking {
            return false;
        }
        *scheme == http::uri::Scheme::HTTP || *scheme == http::uri::Scheme::HTTPS
    }

    fn default_scheme(&mut self) -> Option<http::uri::Scheme> {
        if self.disable_networking {
            return None;
        }
        Some(http::uri::Scheme::HTTPS)
    }
}

impl wasmtime::component::HasData for StoreState {
    type Data<'a> = &'a mut StoreState;
}

struct PtySession {
    writer: PtyWriter,
    child: Arc<Mutex<Box<dyn Child + Send>>>,
    reader_task: JoinHandle<Result<(), HostError>>,
    keyboard_task: JoinHandle<Result<(), HostError>>,
    keyboard_stop: Arc<AtomicBool>,
}

impl PtySession {
    fn spawn(
        command: String,
        command_args: Vec<String>,
        transcript: SharedTranscript,
        max_bytes: usize,
        event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
        keyboard_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    ) -> Result<Self, HostError> {
        info!(
            command = %command,
            args = ?command_args,
            "spawning PTY child process"
        );
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: PTY_ROWS,
            cols: PTY_COLS,
            pixel_width: PTY_PIXEL_WIDTH,
            pixel_height: PTY_PIXEL_HEIGHT,
        })?;

        let mut cmd = CommandBuilder::new(&command);
        for arg in &command_args {
            cmd.arg(arg);
        }
        let cwd = std::env::current_dir()?;
        info!(cwd = %cwd.display(), "setting PTY working directory");
        cmd.cwd(cwd.as_os_str());

        let child = pair.slave.spawn_command(cmd)?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let writer = Arc::new(Mutex::new(writer));
        let keyboard_stop = Arc::new(AtomicBool::new(false));

        let transcript_reader = Arc::clone(&transcript);
        let response_writer = Arc::clone(&writer);
        let keyboard_writer = Arc::clone(&writer);
        let keyboard_stop_for_task = Arc::clone(&keyboard_stop);

        let reader_task = tokio::task::spawn_blocking(move || {
            read_pty_output(
                reader,
                response_writer,
                transcript_reader,
                max_bytes,
                event_sender,
            )
        });
        let keyboard_task = tokio::task::spawn_blocking(move || {
            forward_keyboard_input(keyboard_writer, keyboard_rx, keyboard_stop_for_task)
        });

        Ok(Self {
            writer,
            child: Arc::new(Mutex::new(child)),
            reader_task,
            keyboard_task,
            keyboard_stop,
        })
    }

    async fn write_inputs(
        &self,
        inputs: Vec<bindings::mitb::host::types::Input>,
    ) -> Result<(), HostError> {
        write_pty_inputs(&self.writer, inputs).await
    }

    async fn child_exited(&self) -> Result<bool, HostError> {
        let child = Arc::clone(&self.child);
        tokio::task::spawn_blocking(move || {
            let mut child = child.lock().map_err(|_| HostError::PoisonedLock("child"))?;
            Ok(child.try_wait()?.is_some())
        })
        .await?
    }

    async fn terminate(self) -> Result<(), HostError> {
        info!("terminating PTY session");
        self.keyboard_stop.store(true, Ordering::Relaxed);
        let child = Arc::clone(&self.child);
        tokio::task::spawn_blocking(move || {
            let mut child = child.lock().map_err(|_| HostError::PoisonedLock("child"))?;
            if child.try_wait()?.is_none() {
                child.kill()?;
                let _ = child.wait()?;
            }
            Ok::<(), HostError>(())
        })
        .await??;

        self.reader_task.await??;
        self.keyboard_task.await??;
        Ok(())
    }
}

pub async fn run(options: HostOptions) -> Result<(), HostError> {
    runtime::run(options).await
}

impl bindings::mitb::host::types::Host for StoreState {}

impl bindings::mitb::host::terminal::Host for StoreState {
    async fn head(&mut self) -> wasmtime::Result<u64> {
        transcript_head(&self.transcript).map_err(to_wasmtime_error)
    }
}

impl bindings::mitb::host::terminal::HostWithStore for StoreState {
    async fn snapshot<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, String>> {
        let transcript = accessor.with(|mut access| access.get().transcript.clone());
        Ok(transcript_snapshot_bytes(&transcript, max_bytes as usize)
            .map_err(|error| error.to_string()))
    }

    async fn read_since<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        cursor: u64,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<(u64, Vec<u8>), String>> {
        let transcript = accessor.with(|mut access| access.get().transcript.clone());
        Ok(
            transcript_bytes_since(&transcript, cursor, max_bytes as usize)
                .map_err(|error| error.to_string()),
        )
    }
}

impl bindings::mitb::host::reporting::Host for StoreState {}

impl bindings::mitb::host::reporting::HostWithStore for StoreState {
    async fn report_reward<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        reward: f64,
    ) -> wasmtime::Result<Result<(), String>> {
        let report_store = accessor.with(|mut access| access.get().report_store.clone());
        Ok(report_store.publish(reward).await)
    }
}

impl bindings::mitb::host::process::Host for StoreState {}

impl bindings::mitb::host::process::HostChild for StoreState {
    async fn drop(&mut self, rep: Resource<ProcessChild>) -> wasmtime::Result<()> {
        let child = self.table.delete(rep).map_err(to_wasmtime_error)?;
        tokio::spawn(async move {
            let _ = kill_child(&child.control).await;
        });
        Ok(())
    }
}

impl bindings::mitb::host::process::HostWithStore for StoreState {
    async fn spawn<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        name: String,
        args: Vec<String>,
    ) -> wasmtime::Result<Result<Resource<ProcessChild>, bindings::mitb::host::types::SpawnError>>
    {
        let disable_spawn = accessor.with(|mut access| access.get().disable_spawn);
        if disable_spawn {
            return Ok(Err(spawn_disabled_error()));
        }

        let child = match spawn_process_child(name, args).await {
            Ok(child) => child,
            Err(error) => return Ok(Err(error)),
        };

        let resource = accessor
            .with(|mut access| access.get().table.push(child).map_err(to_wasmtime_error))?;
        Ok(Ok(resource))
    }
}

impl bindings::mitb::host::process::HostChildWithStore for StoreState {
    async fn write_stdin<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: Resource<ProcessChild>,
        data: StreamReader<u8>,
    ) -> wasmtime::Result<Result<(), String>> {
        accessor.with(|mut access| {
            let tx = access
                .get()
                .table
                .get(&self_)
                .map_err(to_wasmtime_error)?
                .stdin_tx
                .clone();
            data.pipe(&mut access, MpscStreamConsumer::new(tx));
            Ok::<(), wasmtime::Error>(())
        })?;
        Ok(Ok(()))
    }

    async fn read_stdout<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: Resource<ProcessChild>,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), String>>)> {
        accessor.with(|mut access| {
            let child = access.get().table.get(&self_).map_err(to_wasmtime_error)?;
            let rx = child.take_stdout_receiver().map_err(to_wasmtime_error)?;
            let done = child.stdout_done.clone();
            let stream = StreamReader::new(&mut access, MpscStreamProducer::new(rx));
            let future = FutureReader::new(&mut access, wait_for_stream_completion(done));
            Ok::<_, wasmtime::Error>((stream, future))
        })
    }

    async fn read_stderr<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: Resource<ProcessChild>,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), String>>)> {
        accessor.with(|mut access| {
            let child = access.get().table.get(&self_).map_err(to_wasmtime_error)?;
            let rx = child.take_stderr_receiver().map_err(to_wasmtime_error)?;
            let done = child.stderr_done.clone();
            let stream = StreamReader::new(&mut access, MpscStreamProducer::new(rx));
            let future = FutureReader::new(&mut access, wait_for_stream_completion(done));
            Ok::<_, wasmtime::Error>((stream, future))
        })
    }

    async fn wait<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: Resource<ProcessChild>,
    ) -> wasmtime::Result<Result<bindings::mitb::host::types::ExitStatus, String>> {
        let control = accessor.with(|mut access| {
            access
                .get()
                .table
                .get(&self_)
                .map(|child| child.control.clone())
                .map_err(to_wasmtime_error)
        })?;
        Ok(wait_for_child(&control).await)
    }

    async fn wait_timeout<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: Resource<ProcessChild>,
        timeout_ns: u64,
    ) -> wasmtime::Result<Result<Option<bindings::mitb::host::types::ExitStatus>, String>> {
        let control = accessor.with(|mut access| {
            access
                .get()
                .table
                .get(&self_)
                .map(|child| child.control.clone())
                .map_err(to_wasmtime_error)
        })?;
        Ok(wait_for_child_timeout(&control, timeout_ns).await)
    }

    async fn kill<T>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: Resource<ProcessChild>,
    ) -> wasmtime::Result<Result<(), String>> {
        let control = accessor.with(|mut access| {
            access
                .get()
                .table
                .get(&self_)
                .map(|child| child.control.clone())
                .map_err(to_wasmtime_error)
        })?;
        Ok(kill_child(&control).await)
    }
}

impl ProcessChild {
    fn take_stdout_receiver(&self) -> Result<mpsc::Receiver<u8>, String> {
        take_stream_receiver(&self.stdout_rx, "stdout")
    }

    fn take_stderr_receiver(&self) -> Result<mpsc::Receiver<u8>, String> {
        take_stream_receiver(&self.stderr_rx, "stderr")
    }
}

fn take_stream_receiver(
    slot: &Mutex<Option<mpsc::Receiver<u8>>>,
    stream_name: &str,
) -> Result<mpsc::Receiver<u8>, String> {
    let mut guard = slot
        .lock()
        .map_err(|_| format!("{stream_name} stream lock poisoned"))?;
    guard
        .take()
        .ok_or_else(|| format!("{stream_name} stream already taken"))
}

fn spawn_output_reader<R>(
    stream_name: &'static str,
    mut reader: R,
) -> (
    mpsc::Receiver<u8>,
    watch::Receiver<Option<Result<(), String>>>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(8 * 1024);
    let (done_tx, done_rx) = watch::channel(None);
    tokio::spawn(async move {
        let result = async {
            let mut buffer = [0_u8; 4096];
            loop {
                let count = reader
                    .read(&mut buffer)
                    .await
                    .map_err(|error| format!("failed reading child {stream_name}: {error}"))?;
                if count == 0 {
                    return Ok(());
                }

                for byte in &buffer[..count] {
                    if tx.send(*byte).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        .await;
        let _ = done_tx.send(Some(result));
    });
    (rx, done_rx)
}

fn spawn_stdin_writer(mut stdin: ChildStdin) -> mpsc::Sender<u8> {
    let (tx, mut rx) = mpsc::channel(8 * 1024);
    tokio::spawn(async move {
        let mut buffer = Vec::with_capacity(4096);
        while let Some(first) = rx.recv().await {
            buffer.clear();
            buffer.push(first);

            while buffer.len() < 4096 {
                match rx.try_recv() {
                    Ok(byte) => buffer.push(byte),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                }
            }

            if stdin.write_all(&buffer).await.is_err() {
                return;
            }
            if stdin.flush().await.is_err() {
                return;
            }
        }
    });
    tx
}

async fn spawn_process_child(
    name: String,
    args: Vec<String>,
) -> Result<ProcessChild, bindings::mitb::host::types::SpawnError> {
    if name.trim().is_empty() {
        return Err(bindings::mitb::host::types::SpawnError {
            kind: bindings::mitb::host::types::SpawnErrorKind::InvalidCommand,
            message: String::from("process name cannot be empty"),
        });
    }

    debug!(command = %name, args = ?args, "spawn invoked by guest");
    let mut command = Command::new(&name);
    command.args(&args);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| bindings::mitb::host::types::SpawnError {
            kind: spawn_error_kind_for_io(&error),
            message: format!("failed spawning process `{name}`: {error}"),
        })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| bindings::mitb::host::types::SpawnError {
            kind: bindings::mitb::host::types::SpawnErrorKind::Other,
            message: format!("process `{name}` did not expose stdin"),
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| bindings::mitb::host::types::SpawnError {
            kind: bindings::mitb::host::types::SpawnErrorKind::Other,
            message: format!("process `{name}` did not expose stdout"),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| bindings::mitb::host::types::SpawnError {
            kind: bindings::mitb::host::types::SpawnErrorKind::Other,
            message: format!("process `{name}` did not expose stderr"),
        })?;

    let control = Arc::new(ChildControl {
        child: tokio::sync::Mutex::new(child),
        exit: tokio::sync::Mutex::new(None),
        exit_notify: Notify::new(),
    });
    let stdin_tx = spawn_stdin_writer(stdin);
    let (stdout_rx, stdout_done) = spawn_output_reader("stdout", stdout);
    let (stderr_rx, stderr_done) = spawn_output_reader("stderr", stderr);

    Ok(ProcessChild {
        stdin_tx,
        stdout_rx: Mutex::new(Some(stdout_rx)),
        stdout_done,
        stderr_rx: Mutex::new(Some(stderr_rx)),
        stderr_done,
        control,
    })
}

fn spawn_disabled_error() -> bindings::mitb::host::types::SpawnError {
    bindings::mitb::host::types::SpawnError {
        kind: bindings::mitb::host::types::SpawnErrorKind::PermissionDenied,
        message: String::from("process spawning disabled by host configuration"),
    }
}

fn spawn_error_kind_for_io(error: &std::io::Error) -> bindings::mitb::host::types::SpawnErrorKind {
    match error.kind() {
        std::io::ErrorKind::NotFound => bindings::mitb::host::types::SpawnErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => {
            bindings::mitb::host::types::SpawnErrorKind::PermissionDenied
        }
        std::io::ErrorKind::InvalidInput => {
            bindings::mitb::host::types::SpawnErrorKind::InvalidCommand
        }
        std::io::ErrorKind::Other => bindings::mitb::host::types::SpawnErrorKind::Other,
        _ => bindings::mitb::host::types::SpawnErrorKind::Io,
    }
}

async fn wait_for_stream_completion(
    mut done: watch::Receiver<Option<Result<(), String>>>,
) -> Result<Result<(), String>, wasmtime::Error> {
    loop {
        if let Some(result) = done.borrow().clone() {
            return Ok(result);
        }
        if done.changed().await.is_err() {
            return Ok(Ok(()));
        }
    }
}

async fn wait_for_child(
    control: &Arc<ChildControl>,
) -> Result<bindings::mitb::host::types::ExitStatus, String> {
    loop {
        if let Some(result) = control.exit.lock().await.clone() {
            return result;
        }

        let maybe_status = {
            let mut child = control.child.lock().await;
            child
                .try_wait()
                .map_err(|error| format!("failed polling child exit status: {error}"))?
        };

        if let Some(status) = maybe_status {
            let result = Ok(exit_status_from_std(status));
            return store_child_exit(control, result).await;
        }

        tokio::select! {
            _ = control.exit_notify.notified() => {}
            _ = sleep(CHILD_WAIT_POLL) => {}
        }
    }
}

async fn wait_for_child_timeout(
    control: &Arc<ChildControl>,
    timeout_ns: u64,
) -> Result<Option<bindings::mitb::host::types::ExitStatus>, String> {
    match tokio::time::timeout(Duration::from_nanos(timeout_ns), wait_for_child(control)).await {
        Ok(result) => result.map(Some),
        Err(_) => Ok(None),
    }
}

async fn store_child_exit(
    control: &Arc<ChildControl>,
    result: Result<bindings::mitb::host::types::ExitStatus, String>,
) -> Result<bindings::mitb::host::types::ExitStatus, String> {
    let mut exit = control.exit.lock().await;
    if let Some(existing) = exit.clone() {
        return existing;
    }
    *exit = Some(result.clone());
    drop(exit);
    control.exit_notify.notify_waiters();
    result
}

async fn kill_child(control: &Arc<ChildControl>) -> Result<(), String> {
    if control.exit.lock().await.is_some() {
        return Ok(());
    }

    let mut child = control.child.lock().await;
    if child
        .try_wait()
        .map_err(|error| format!("failed polling child before kill: {error}"))?
        .is_some()
    {
        return Ok(());
    }

    child
        .start_kill()
        .map_err(|error| format!("failed killing child process: {error}"))?;
    Ok(())
}

fn install_tls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn exit_status_from_std(
    status: std::process::ExitStatus,
) -> bindings::mitb::host::types::ExitStatus {
    bindings::mitb::host::types::ExitStatus {
        success: status.success(),
        code: status.code().map(|code| code as u32),
    }
}

fn to_wasmtime_error(error: impl core::fmt::Display) -> wasmtime::Error {
    wasmtime::Error::msg(error.to_string())
}

fn host_home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

fn host_shared_root_dir() -> Option<PathBuf> {
    host_home_dir().map(|home| home.join(".mitb").join("shared"))
}

#[cfg(test)]
mod tests {
    use super::{kill_child, spawn_disabled_error, spawn_process_child, wait_for_child_timeout};
    use crate::terminal_state::PtyTerminalState;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_for_child_timeout_returns_none_when_process_runs_too_long() -> Result<(), String>
    {
        let child = spawn_process_child(
            "bash".to_string(),
            vec!["-lc".to_string(), "sleep 5".to_string()],
        )
        .await
        .map_err(|error| format!("{error:?}"))?;

        let result =
            wait_for_child_timeout(&child.control, Duration::from_millis(20).as_nanos() as u64)
                .await?;

        assert!(result.is_none());
        kill_child(&child.control).await?;
        Ok(())
    }

    #[tokio::test]
    async fn wait_for_child_timeout_returns_exit_status_when_process_finishes() -> Result<(), String>
    {
        let child = spawn_process_child(
            "bash".to_string(),
            vec!["-lc".to_string(), "exit 0".to_string()],
        )
        .await
        .map_err(|error| format!("{error:?}"))?;

        let result =
            wait_for_child_timeout(&child.control, Duration::from_secs(1).as_nanos() as u64)
                .await?;

        let status = result.ok_or_else(|| String::from("process should finish before timeout"))?;
        assert!(status.success);
        assert_eq!(status.code, Some(0));
        Ok(())
    }

    #[test]
    fn pty_terminal_state_replies_to_cursor_position_requests() {
        let mut terminal = PtyTerminalState::new(80, 24);
        let responses = terminal.feed(b"hello\r\n\x1b[12;34H\x1b[6n");
        assert_eq!(responses, vec![b"\x1b[12;34R".to_vec()]);
    }

    #[test]
    fn pty_terminal_state_handles_split_cursor_position_requests() {
        let mut terminal = PtyTerminalState::new(80, 24);
        assert!(terminal.feed(b"\x1b[4;9H\x1b[").is_empty());
        let responses = terminal.feed(b"6n");
        assert_eq!(responses, vec![b"\x1b[4;9R".to_vec()]);
    }

    #[test]
    fn spawn_disabled_error_is_permission_denied() {
        let error = spawn_disabled_error();
        assert!(matches!(
            error.kind,
            crate::bindings::mitb::host::types::SpawnErrorKind::PermissionDenied
        ));
        assert!(error.message.contains("disabled"));
    }
}
