use super::*;
use crate::terminal_state::PtyTerminalState;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};
use tracing::{debug, trace};

const TEXT_CHUNK_BYTES: usize = 96;
const TEXT_CHUNK_DELAY: Duration = Duration::from_millis(20);
const KEY_DELAY_AFTER_TEXT: Duration = Duration::from_millis(250);
const ENTER_DELAY_AFTER_TEXT: Duration = Duration::from_millis(700);
const KEY_DELAY_BETWEEN_KEYS: Duration = Duration::from_millis(120);

pub(crate) type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub(crate) async fn write_inputs(
    writer: &PtyWriter,
    inputs: Vec<bindings::mitb::host::types::Input>,
) -> Result<(), HostError> {
    debug!(inputs = inputs.len(), "writing inputs to PTY");
    for (index, input) in inputs.iter().enumerate() {
        match input {
            bindings::mitb::host::types::Input::Text(text) => {
                trace!(index, bytes = text.len(), "writing text input");
            }
            bindings::mitb::host::types::Input::Key(key) => {
                trace!(index, key = ?key, "writing key input");
            }
        }
        if index > 0 && matches!(input, bindings::mitb::host::types::Input::Key(_)) {
            let previous = &inputs[index - 1];
            if matches!(previous, bindings::mitb::host::types::Input::Text(_)) {
                let delay = match input {
                    bindings::mitb::host::types::Input::Key(
                        bindings::mitb::host::types::Key::Enter,
                    ) => ENTER_DELAY_AFTER_TEXT,
                    _ => KEY_DELAY_AFTER_TEXT,
                };
                sleep(delay).await;
            } else if matches!(previous, bindings::mitb::host::types::Input::Key(_)) {
                sleep(KEY_DELAY_BETWEEN_KEYS).await;
            }
        }

        match input {
            bindings::mitb::host::types::Input::Text(text) => {
                write_text_streamed(writer, text).await?;
            }
            bindings::mitb::host::types::Input::Key(key) => {
                let payload = key_bytes(key).to_vec();
                if !payload.is_empty() {
                    write_payload(writer, payload).await?;
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn read_pty_output(
    mut reader: Box<dyn Read + Send>,
    writer: PtyWriter,
    transcript: SharedTranscript,
    max_bytes: usize,
    event_sender: Option<mpsc::UnboundedSender<HostEvent>>,
) -> Result<(), HostError> {
    let mut chunk = [0_u8; 4096];
    let mut terminal = PtyTerminalState::new(80, 24);

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                trace!(bytes = count, "read PTY output chunk");
                for response in terminal.feed(&chunk[..count]) {
                    debug!(
                        response = %String::from_utf8_lossy(&response),
                        "replying to PTY cursor position request"
                    );
                    write_payload_blocking(&writer, &response)?;
                }
                if let Some(tx) = &event_sender {
                    let _ = tx.send(HostEvent::TerminalOutput(chunk[..count].to_vec()));
                }

                let mut transcript = transcript
                    .lock()
                    .map_err(|_| HostError::PoisonedLock("transcript"))?;
                transcript.bytes.extend_from_slice(&chunk[..count]);
                trim_transcript(&mut transcript, max_bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            Err(error) => return Err(HostError::Io(error)),
        }
    }
}

async fn write_text_streamed(writer: &PtyWriter, text: &str) -> Result<(), HostError> {
    if text.is_empty() {
        return Ok(());
    }

    for chunk in text.as_bytes().chunks(TEXT_CHUNK_BYTES) {
        write_payload(writer, chunk.to_vec()).await?;
        sleep(TEXT_CHUNK_DELAY).await;
    }

    Ok(())
}

async fn write_payload(writer: &PtyWriter, payload: Vec<u8>) -> Result<(), HostError> {
    if payload.is_empty() {
        return Ok(());
    }

    let writer = Arc::clone(writer);
    tokio::task::spawn_blocking(move || write_payload_blocking(&writer, &payload)).await??;

    Ok(())
}

fn write_payload_blocking(writer: &PtyWriter, payload: &[u8]) -> Result<(), HostError> {
    let mut writer = writer
        .lock()
        .map_err(|_| HostError::PoisonedLock("writer"))?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn trim_transcript(transcript: &mut TranscriptBuffer, max_bytes: usize) {
    if transcript.bytes.len() > max_bytes {
        let overflow = transcript.bytes.len().saturating_sub(max_bytes);
        transcript.bytes.drain(..overflow);
        transcript.start = transcript.start.saturating_add(overflow as u64);
    }
}

fn key_bytes(key: &bindings::mitb::host::types::Key) -> &'static [u8] {
    match key {
        bindings::mitb::host::types::Key::Enter => b"\r",
        bindings::mitb::host::types::Key::Tab => b"\t",
        bindings::mitb::host::types::Key::Backspace => b"\x7f",
        bindings::mitb::host::types::Key::Escape => b"\x1b",
        bindings::mitb::host::types::Key::ArrowUp => b"\x1b[A",
        bindings::mitb::host::types::Key::ArrowDown => b"\x1b[B",
        bindings::mitb::host::types::Key::ArrowLeft => b"\x1b[D",
        bindings::mitb::host::types::Key::ArrowRight => b"\x1b[C",
        bindings::mitb::host::types::Key::CtrlC => b"\x03",
        bindings::mitb::host::types::Key::CtrlD => b"\x04",
    }
}
