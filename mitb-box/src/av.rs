use futures_lite::stream;
use iced::widget::shader;
use iced::{Element, Fill, Subscription};
use mitb_av::{AvEvent, Program, Yuv};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use std::sync::{Arc, OnceLock};

static AV_EVENT_RX: OnceLock<Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<AvEvent>>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) enum Message {
    Frame(Yuv),
    Ended,
    Error(String),
}

pub(crate) struct State {
    program: Option<Program>,
}

impl State {
    pub(crate) fn new(mute_audio: bool) -> Self {
        if AV_EVENT_RX.get().is_none() {
            let media_path = match mitb_av::default_media_path() {
                Ok(path) => path,
                Err(error) => {
                    warn!(%error, "failed to resolve AV media path");
                    return Self { program: None };
                }
            };

            if media_path.exists() {
                let receiver = mitb_av::spawn_with_options(
                    media_path,
                    mitb_av::PlaybackOptions { mute_audio },
                );
                let _ = AV_EVENT_RX.set(Arc::new(tokio::sync::Mutex::new(receiver)));
            } else {
                debug!("AV media file not found; overlay playback disabled");
            }
        }

        Self { program: None }
    }

    pub(crate) fn update(&mut self, message: Message) {
        match message {
            Message::Frame(frame) => {
                if let Some(program) = self.program.as_mut() {
                    program.update_frame(frame);
                } else {
                    self.program = Some(Program::new(frame));
                }
            }
            Message::Ended => {
                debug!("AV playback finished");
            }
            Message::Error(error) => {
                warn!(%error, "AV playback error");
            }
        }
    }

    pub(crate) fn overlay_view(&self) -> Option<Element<'_, super::Message>> {
        let program = self.program.as_ref()?;
        Some(shader(program).width(Fill).height(Fill).into())
    }

    pub(crate) fn subscription(&self) -> Subscription<super::Message> {
        if AV_EVENT_RX.get().is_some() {
            Subscription::run(av_event_stream).map(super::Message::Av)
        } else {
            Subscription::none()
        }
    }
}

fn av_event_stream() -> impl futures_lite::Stream<Item = Message> {
    stream::unfold((), |_| async move {
        let receiver = AV_EVENT_RX.get()?.clone();
        let mut guard = receiver.lock().await;
        let event = guard.recv().await?;

        let message = match event {
            AvEvent::VideoFrame(frame) => Message::Frame(frame),
            AvEvent::PlaybackEnded => Message::Ended,
            AvEvent::PlaybackError(error) => Message::Error(error),
        };

        Some((message, ()))
    })
}
