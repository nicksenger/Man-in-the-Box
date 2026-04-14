mod animated_eye;
#[path = "av.rs"]
mod av;
mod terminal;

use animated_eye::animated_eye_cursor;
use futures_lite::stream;
use iced::font::{Style as FontStyle, Weight};
use iced::widget::{Column, Float, Stack, column, container, mouse_area, row, scrollable, text};
use iced::{Element, Fill, Font, Point, Size, Subscription, Task, Vector, event, keyboard, mouse};
use mitb_host::HostEvent;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::warn;

static HOST_EVENT_RX: std::sync::OnceLock<
    std::sync::Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<HostEvent>>>,
> = std::sync::OnceLock::new();
static MUTE_AUDIO: AtomicBool = AtomicBool::new(false);
const WINDOW_WIDTH: f32 = 640.0;
const WINDOW_HEIGHT: f32 = 480.0;
const TERMINAL_COLS: usize = 72;
const TERMINAL_ROWS: usize = 27;
const TERMINAL_FONT_SIZE: f32 = 14.0;
const TERMINAL_LINE_HEIGHT: f32 = 1.0;
const STATUS_FONT_SIZE: f32 = 12.0;
const EYE_CURSOR_SIZE: f32 = 48.0;
const EYE_CURSOR_HOTSPOT: Point = Point::new(24.0, 24.0);

#[derive(Debug, Error)]
pub enum BoxError {
    #[error("mitb-box event receiver is already initialized")]
    ReceiverAlreadyInitialized,
    #[error("mitb-box UI error: {0}")]
    Iced(#[from] iced::Error),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub mute_audio: bool,
}

#[derive(Debug, Clone)]
enum Message {
    HostEvent(HostEvent),
    #[allow(dead_code)]
    Av(av::Message),
    Input(InputEvent),
    AppCursorMoved(Point),
    AppCursorLeft,
    TerminalHoverChanged(bool),
}

#[derive(Debug, Clone, Copy)]
enum InputEvent {
    LeftMousePressed,
    LeftMouseReleased,
    KeyPressed(keyboard::key::Physical),
    KeyReleased(keyboard::key::Physical),
}

pub fn run(
    event_rx: mpsc::UnboundedReceiver<HostEvent>,
    options: RunOptions,
) -> Result<(), BoxError> {
    HOST_EVENT_RX
        .set(std::sync::Arc::new(tokio::sync::Mutex::new(event_rx)))
        .map_err(|_| BoxError::ReceiverAlreadyInitialized)?;
    MUTE_AUDIO.store(options.mute_audio, Ordering::Relaxed);

    iced::application(App::new, App::update, App::view)
        .subscription(App::subscription)
        .default_font(Font::MONOSPACE)
        .window_size(Size::new(WINDOW_WIDTH, WINDOW_HEIGHT))
        .title("Man in the Box")
        .run()
        .map_err(BoxError::from)
}

struct App {
    terminal: terminal::Terminal,
    av: av::State,
    status: Option<String>,
    app_cursor_position: Option<Point>,
    terminal_hovered: bool,
    left_mouse_down: bool,
    pressed_keys: HashSet<keyboard::key::Physical>,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        (
            Self {
                terminal: terminal::Terminal::new(TERMINAL_COLS, TERMINAL_ROWS),
                av: av::State::new(MUTE_AUDIO.load(Ordering::Relaxed)),
                status: None,
                app_cursor_position: None,
                terminal_hovered: false,
                left_mouse_down: false,
                pressed_keys: HashSet::new(),
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::HostEvent(event) => match event {
                HostEvent::TerminalOutput(data) => {
                    self.terminal.feed(&data);
                    Task::none()
                }
                HostEvent::SessionEnded(reason) => {
                    self.status = Some(format!("Session ended: {reason}"));
                    iced::exit()
                }
                HostEvent::Disconnected => {
                    self.status = Some(String::from("Disconnected"));
                    iced::exit()
                }
            },
            Message::Av(message) => {
                self.av.update(message);
                Task::none()
            }
            Message::Input(input_event) => {
                match input_event {
                    InputEvent::LeftMousePressed => {
                        self.left_mouse_down = true;
                        warn!("denied");
                    }
                    InputEvent::LeftMouseReleased => {
                        self.left_mouse_down = false;
                    }
                    InputEvent::KeyPressed(physical_key) => {
                        if self.pressed_keys.insert(physical_key) {
                            warn!("denied");
                        }
                    }
                    InputEvent::KeyReleased(physical_key) => {
                        self.pressed_keys.remove(&physical_key);
                    }
                }
                Task::none()
            }
            Message::AppCursorMoved(position) => {
                self.app_cursor_position = Some(position);
                Task::none()
            }
            Message::AppCursorLeft => {
                self.app_cursor_position = None;
                self.terminal_hovered = false;
                Task::none()
            }
            Message::TerminalHoverChanged(is_hovered) => {
                self.terminal_hovered = is_hovered;
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let lines = self.terminal.lines();
        let rows: Vec<Element<'_, Message>> = lines
            .iter()
            .map(|line| {
                let spans: Vec<Element<'_, Message>> = line
                    .spans
                    .iter()
                    .map(|span| {
                        let font = Font {
                            family: Font::MONOSPACE.family,
                            weight: if span.bold {
                                Weight::Bold
                            } else {
                                Weight::Normal
                            },
                            stretch: Font::MONOSPACE.stretch,
                            style: if span.italic {
                                FontStyle::Italic
                            } else {
                                FontStyle::Normal
                            },
                        };

                        let text_widget = text(span.text.clone())
                            .color(span.fg_color)
                            .size(TERMINAL_FONT_SIZE)
                            .line_height(TERMINAL_LINE_HEIGHT)
                            .wrapping(iced::widget::text::Wrapping::None)
                            .shaping(iced::widget::text::Shaping::Basic)
                            .font(font);

                        if let Some(bg_color) = span.bg_color {
                            container(text_widget)
                                .style(move |_| {
                                    iced::widget::container::Style::default().background(bg_color)
                                })
                                .into()
                        } else {
                            text_widget.into()
                        }
                    })
                    .collect();
                row(spans).into()
            })
            .collect();

        let terminal_content: Element<'_, Message> = scrollable(Column::with_children(rows))
            .width(Fill)
            .height(Fill)
            .into();
        let terminal_content = mouse_area(terminal_content)
            .on_enter(Message::TerminalHoverChanged(true))
            .on_exit(Message::TerminalHoverChanged(false))
            .interaction(mouse::Interaction::Hidden);

        let content: Element<'_, Message> = if let Some(status) = &self.status {
            Element::from(
                column![
                    terminal_content,
                    text(status)
                        .size(STATUS_FONT_SIZE)
                        .line_height(TERMINAL_LINE_HEIGHT)
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .shaping(iced::widget::text::Shaping::Basic)
                ]
                .spacing(8)
                .padding(10),
            )
        } else {
            Element::from(column![terminal_content].padding(10))
        };

        let content = container(content).width(Fill).height(Fill);
        let content = mouse_area(content)
            .on_move(Message::AppCursorMoved)
            .on_exit(Message::AppCursorLeft);

        let mut layered = Stack::new().width(Fill).height(Fill).push(content);
        if let Some(overlay) = self.av.overlay_view() {
            layered = layered.push(overlay);
        }
        let layered: Element<'_, Message> = layered.into();

        if let Some(position) = self.app_cursor_position.filter(|_| self.terminal_hovered) {
            let eye_active = self.left_mouse_down || !self.pressed_keys.is_empty();

            Stack::new()
                .width(Fill)
                .height(Fill)
                .push(layered)
                .push(
                    Float::new(animated_eye_cursor(EYE_CURSOR_SIZE, eye_active)).translate(
                        move |bounds, _viewport| {
                            Vector::new(
                                position.x - EYE_CURSOR_HOTSPOT.x - bounds.x,
                                position.y - EYE_CURSOR_HOTSPOT.y - bounds.y,
                            )
                        },
                    ),
                )
                .into()
        } else {
            layered
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch(vec![
            Subscription::run(host_event_stream),
            self.av.subscription(),
            event::listen_with(|event, _status, _window| match event {
                iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                    Some(Message::Input(InputEvent::LeftMousePressed))
                }
                iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                    Some(Message::Input(InputEvent::LeftMouseReleased))
                }
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    physical_key,
                    repeat,
                    ..
                }) if !repeat => Some(Message::Input(InputEvent::KeyPressed(physical_key))),
                iced::Event::Keyboard(keyboard::Event::KeyReleased { physical_key, .. }) => {
                    Some(Message::Input(InputEvent::KeyReleased(physical_key)))
                }
                _ => None,
            }),
        ])
    }
}

fn host_event_stream() -> impl futures_lite::Stream<Item = Message> {
    stream::unfold(None::<HostEvent>, |pending| async move {
        let rx = HOST_EVENT_RX.get()?.clone();
        let mut guard = rx.lock().await;
        let mut next_pending = None;
        let event = match pending {
            Some(event) => event,
            None => guard.recv().await?,
        };

        let event = match event {
            HostEvent::TerminalOutput(mut data) => {
                loop {
                    match guard.try_recv() {
                        Ok(HostEvent::TerminalOutput(next_data)) => {
                            data.extend(next_data);
                        }
                        Ok(other) => {
                            next_pending = Some(other);
                            break;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            next_pending = Some(HostEvent::Disconnected);
                            break;
                        }
                    }
                }
                HostEvent::TerminalOutput(data)
            }
            other => other,
        };

        Some((Message::HostEvent(event), next_pending))
    })
}
