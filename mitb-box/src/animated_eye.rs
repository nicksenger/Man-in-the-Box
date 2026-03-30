use iced::advanced::layout;
use iced::advanced::renderer;
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget};
use iced::mouse;
use iced::time::Instant;
use iced::widget::canvas;
use iced::window::{self, RedrawRequest};
use iced::{Color, Element, Event, Length, Point, Rectangle, Renderer, Size, Vector};
use std::f32::consts::TAU;
use std::time::Duration;

const FRAME_DURATION: Duration = Duration::from_millis(1000 / 60);
const CLOSE_DURATION: Duration = Duration::from_millis(180);
const PROHIBIT_KICK_DURATION: Duration = Duration::from_millis(160);
const SQUIRM_PERIOD: Duration = Duration::from_millis(560);
const SQUIRM_IMPACT: Duration = Duration::from_millis(65);
const SQUIRM_SETTLE: Duration = Duration::from_millis(165);
const FEAR_SACCADE_AMPLITUDE: f32 = 2.05;
const FEAR_DART_DURATION: f32 = 0.08;
const FEAR_HOLD_MIN: f32 = 0.25;
const FEAR_HOLD_MAX: f32 = 1.0;
const FEAR_HOLD_PATTERN: u32 = 24;
const FEAR_TREMOR_X: f32 = 0.28;
const FEAR_TREMOR_Y: f32 = 0.2;
const FEAR_OPEN_SPAN_BOOST: f32 = 1.15;
const RAY_COUNT: usize = 34;
const RAY_CYCLE_MIN: f32 = 0.56;
const RAY_CYCLE_MAX: f32 = 1.22;
const RAY_ACTIVE_PORTION: f32 = 0.64;
const RAY_TAIL_DELAY_MAX: f32 = 0.34;
const RAY_STITCH_DURATION: Duration = Duration::from_millis(480);
const RAY_STITCH_ATTACK_PORTION: f32 = 0.28;
const RAY_STITCH_COLLAPSE_START: f32 = 0.62;
const RAY_STITCH_COLLAPSE_END: f32 = 1.0;
const POST_STITCH_FADE_DURATION: Duration = Duration::from_millis(1000);
const OPEN_LID_SPAN: f32 = 4.9;
const CLOSED_LID_SPAN: f32 = 0.35;
const ENC_TRI_TOP_X: f32 = 16.0;
const ENC_TRI_TOP_Y: f32 = 3.1;
const ENC_TRI_RIGHT_X: f32 = 28.6;
const ENC_TRI_RIGHT_Y: f32 = 26.4;
const ENC_TRI_LEFT_X: f32 = 3.4;
const ENC_TRI_LEFT_Y: f32 = 26.4;
const ENC_BOX_LEFT: f32 = 5.2;
const ENC_BOX_TOP: f32 = 6.1;
const ENC_BOX_RIGHT: f32 = 26.8;
const ENC_BOX_BOTTOM: f32 = 27.7;

#[derive(Debug, Clone)]
pub struct AnimatedEyeCursor {
    size: f32,
    active: bool,
}

impl AnimatedEyeCursor {
    pub fn new(size: f32, active: bool) -> Self {
        Self { size, active }
    }
}

#[derive(Debug, Clone, Copy)]
struct Animation {
    active: bool,
    scan_start: Instant,
    close_start: Option<Instant>,
    ray_freeze_time: f32,
    now: Instant,
}

impl Default for Animation {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            active: false,
            scan_start: now,
            close_start: None,
            ray_freeze_time: 0.0,
            now,
        }
    }
}

impl Animation {
    fn sync_active(&mut self, active: bool, now: Instant) -> bool {
        if self.active == active {
            return false;
        }

        self.active = active;
        self.now = now;
        if active {
            self.close_start = Some(now);
            self.ray_freeze_time = now.saturating_duration_since(self.scan_start).as_secs_f32();
        } else {
            self.close_start = None;
            self.scan_start = now;
            self.ray_freeze_time = 0.0;
        }

        true
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
    }

    fn visual(&self) -> Visual {
        if self.active {
            let close_start = self.close_start.unwrap_or(self.now);
            let raw = progress_between(close_start, self.now, CLOSE_DURATION);
            let close = ease_out_quart(raw);
            let thread_snap = clamp01((close - 0.05) * 4.5);
            let close_elapsed = self.now.saturating_duration_since(close_start);
            let post_stitch_elapsed = close_elapsed.saturating_sub(RAY_STITCH_DURATION);
            let post_stitch_t = post_stitch_elapsed.as_secs_f32();
            let post_fade = ease_in_cubic(post_stitch_t / POST_STITCH_FADE_DURATION.as_secs_f32());
            let (squirm_dx, squirm_dy, squirm_warp) = active_squirm(close_elapsed, close);
            let widget_jitter_x = prohibited_kick_x(close_elapsed);
            Visual {
                pupil_dx: 0.0,
                close,
                is_active: true,
                thread_snap,
                squirm_dx,
                squirm_dy,
                squirm_warp,
                widget_jitter_x,
                widget_jitter_y: 0.0,
                open_span_boost: 0.0,
                pupil_dilation: 0.0,
                ray_time: close_elapsed.as_secs_f32(),
                ray_visibility: 1.0,
                ray_freeze_time: self.ray_freeze_time,
                post_fade,
            }
        } else {
            let elapsed = self.now.saturating_duration_since(self.scan_start);
            let fearful = fearful_idle(elapsed);
            Visual {
                pupil_dx: fearful.pupil_dx,
                close: 0.0,
                is_active: false,
                thread_snap: 0.0,
                squirm_dx: 0.0,
                squirm_dy: 0.0,
                squirm_warp: 0.0,
                widget_jitter_x: fearful.jitter_x,
                widget_jitter_y: fearful.jitter_y,
                open_span_boost: fearful.open_span_boost,
                pupil_dilation: fearful.pupil_dilation,
                ray_time: elapsed.as_secs_f32(),
                ray_visibility: 1.0,
                ray_freeze_time: elapsed.as_secs_f32(),
                post_fade: 0.0,
            }
        }
    }
}

#[derive(Debug, Default)]
struct State {
    animation: Animation,
    cache: canvas::Cache,
    bootstrapped: bool,
}

#[derive(Debug, Clone, Copy)]
struct Visual {
    pupil_dx: f32,
    close: f32,
    is_active: bool,
    thread_snap: f32,
    squirm_dx: f32,
    squirm_dy: f32,
    squirm_warp: f32,
    widget_jitter_x: f32,
    widget_jitter_y: f32,
    open_span_boost: f32,
    pupil_dilation: f32,
    ray_time: f32,
    ray_visibility: f32,
    ray_freeze_time: f32,
    post_fade: f32,
}

#[derive(Debug, Clone, Copy)]
struct FearVisual {
    pupil_dx: f32,
    jitter_x: f32,
    jitter_y: f32,
    open_span_boost: f32,
    pupil_dilation: f32,
}

impl<Message, Theme> Widget<Message, Theme, Renderer> for AnimatedEyeCursor
where
    Message: Clone + 'static,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fixed(self.size),
            height: Length::Fixed(self.size),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();

        if !state.bootstrapped {
            state.bootstrapped = true;
            shell.request_redraw();
        }

        let now = Instant::now();
        if state.animation.sync_active(self.active, now) {
            state.cache.clear();
            shell.request_redraw();
        }

        if let Event::Window(window::Event::RedrawRequested(now)) = *event {
            state.animation.tick(now);
            state.cache.clear();
            shell.request_redraw_at(RedrawRequest::At(now + FRAME_DURATION));
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;

        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        let visual = state.animation.visual();
        let geometry = state.cache.draw(renderer, bounds.size(), |frame| {
            let scale = frame.width().min(frame.height()) / 32.0;
            frame.scale(scale);
            draw_eye(frame, visual);
        });

        renderer.with_translation(
            Vector::new(
                bounds.x + visual.widget_jitter_x,
                bounds.y + visual.widget_jitter_y,
            ),
            |renderer| {
                use iced::advanced::graphics::geometry::Renderer as _;
                renderer.draw_geometry(geometry);
            },
        );
    }
}

impl<'a, Message, Theme> From<AnimatedEyeCursor> for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a + 'static,
    Theme: 'a,
{
    fn from(widget: AnimatedEyeCursor) -> Self {
        Self::new(widget)
    }
}

pub fn animated_eye_cursor<'a, Message: Clone + 'a + 'static>(
    size: f32,
    active: bool,
) -> Element<'a, Message> {
    AnimatedEyeCursor::new(size, active).into()
}

fn draw_eye(frame: &mut canvas::Frame<Renderer>, visual: Visual) {
    let white = Color::from_rgb8(246, 246, 246);
    let ink = Color::from_rgb8(10, 10, 10);
    let alpha_scale = clamp01(1.0 - visual.post_fade);
    if alpha_scale <= 0.001 {
        return;
    }
    let white = with_alpha(white, alpha_scale);
    let ink = with_alpha(ink, alpha_scale);

    if !visual.is_active && visual.ray_visibility > 0.01 {
        draw_rays(frame, white, visual, enclosure_morph(visual));
    }

    let enclosure = canvas::Path::new(|builder| {
        let vertices = enclosure_vertices(enclosure_morph(visual));
        builder.move_to(vertices[0]);
        builder.line_to(vertices[1]);
        builder.line_to(vertices[2]);
        builder.line_to(vertices[3]);
        builder.close();
    });
    frame.fill(&enclosure, ink);
    frame.stroke(
        &enclosure,
        canvas::Stroke::default().with_color(white).with_width(lerp(
            1.6,
            1.4,
            enclosure_morph(visual),
        )),
    );

    let seam_y = 16.95;
    let open_lid_span = OPEN_LID_SPAN + visual.open_span_boost;
    let lid_span = lerp(open_lid_span, CLOSED_LID_SPAN, visual.close);
    let upper = seam_y - lid_span;
    let lower = seam_y + lid_span;

    let eye = canvas::Path::new(|builder| {
        builder.move_to(Point::new(8.8, seam_y));
        builder.quadratic_curve_to(Point::new(16.0, upper), Point::new(23.2, seam_y));
        builder.quadratic_curve_to(Point::new(16.0, lower), Point::new(8.8, seam_y));
        builder.close();
    });
    frame.fill(&eye, ink);
    frame.stroke(
        &eye,
        canvas::Stroke::default().with_color(white).with_width(1.0),
    );

    // Keep the upper eye curve fixed to the open-eye position, even when sewn shut.
    let upper_curve_span = OPEN_LID_SPAN + visual.open_span_boost;
    let upper_crease = canvas::Path::new(|builder| {
        builder.move_to(Point::new(9.7, seam_y - upper_curve_span * 0.6));
        builder.quadratic_curve_to(
            Point::new(16.0, seam_y - upper_curve_span - 1.0),
            Point::new(22.3, seam_y - upper_curve_span * 0.6),
        );
    });
    frame.stroke(
        &upper_crease,
        canvas::Stroke::default()
            .with_color(with_alpha(white, lerp(0.85, 1.0, visual.close)))
            .with_width(1.0),
    );

    // Keep the lower eye curve fixed to the open-eye position, even when sewn shut.
    let lower_curve_span = OPEN_LID_SPAN + visual.open_span_boost;
    let lower_crease = canvas::Path::new(|builder| {
        builder.move_to(Point::new(10.0, seam_y + lower_curve_span * 0.55));
        builder.quadratic_curve_to(
            Point::new(16.0, seam_y + lower_curve_span + 0.85),
            Point::new(22.0, seam_y + lower_curve_span * 0.55),
        );
    });
    frame.stroke(
        &lower_crease,
        canvas::Stroke::default()
            .with_color(with_alpha(white, lerp(0.85, 1.0, visual.close)))
            .with_width(0.95),
    );

    if visual.close < 0.65 {
        let iris_center = Point::new(16.0 + visual.pupil_dx * (1.0 - visual.close), seam_y);
        let iris_radius = 2.75 * (1.0 - 0.5 * visual.close);
        let pupil_radius = (1.08 + visual.pupil_dilation) * (1.0 - 0.8 * visual.close);
        frame.fill(&canvas::Path::circle(iris_center, iris_radius), white);
        frame.fill(&canvas::Path::circle(iris_center, pupil_radius), ink);
        frame.fill(
            &canvas::Path::circle(
                Point::new(iris_center.x + 1.05, iris_center.y - 0.95),
                0.52 * (1.0 - 0.8 * visual.close) * (1.0 - visual.pupil_dilation * 0.18),
            ),
            white,
        );
    }

    let stitch_progress = clamp01(visual.ray_time / RAY_STITCH_DURATION.as_secs_f32());
    if visual.is_active && stitch_progress < 0.995 {
        draw_activation_threads(frame, white, visual, seam_y, enclosure_morph(visual));
    }

    if visual.close > 0.08 {
        let close = visual.close;
        let seam_color = with_alpha(white, close);
        let seam_center_x = 16.0 + visual.squirm_dx * 0.6;
        let seam_line_y = seam_y + visual.squirm_dy;
        let seam_left = Point::new(9.0 + visual.squirm_dx * 0.35, seam_line_y);
        let seam_right = Point::new(23.0 + visual.squirm_dx * 0.35, seam_line_y);

        frame.stroke(
            &canvas::Path::line(seam_left, seam_right),
            canvas::Stroke::default()
                .with_color(seam_color)
                .with_width(lerp(0.7, 1.35, close)),
        );

        let stitch_len = lerp(0.2, 3.6, close);
        let stitch_width = lerp(0.45, 1.15, close);
        let anchors = [10.1_f32, 11.8, 13.5, 15.2, 16.8, 18.5, 20.2, 21.9];
        let len_variation = [1.25_f32, 0.78, 1.08, 0.67, 1.35, 0.84, 1.18, 0.73];
        let y_jitter = [-0.15_f32, 0.10, -0.08, 0.16, -0.12, 0.09, -0.05, 0.14];
        let tilt = [0.62_f32, 0.44, 0.58, 0.40, 0.66, 0.47, 0.54, 0.42];
        let warp_profile = [-0.85_f32, 0.55, -0.35, 0.8, -0.75, 0.5, -0.3, 0.7];
        for (index, x) in anchors.into_iter().enumerate() {
            let center_bias = (x - seam_center_x) * 0.08;
            let half = stitch_len
                * (len_variation[index] + visual.squirm_warp * 0.18 * warp_profile[index])
                * 0.5;
            let y =
                seam_line_y + y_jitter[index] + visual.squirm_warp * (0.42 * warp_profile[index]);
            let dx = tilt[index] + center_bias * visual.squirm_warp * 0.3;
            let (start, end) = if index % 2 == 0 {
                (Point::new(x - dx, y - half), Point::new(x + dx, y + half))
            } else {
                (Point::new(x - dx, y + half), Point::new(x + dx, y - half))
            };

            frame.stroke(
                &canvas::Path::line(start, end),
                canvas::Stroke::default()
                    .with_color(seam_color)
                    .with_width(stitch_width),
            );
        }

        let thread_start = Point::new(
            lerp(30.8, 22.4, visual.thread_snap),
            lerp(9.3, 16.2, visual.thread_snap),
        );
        let thread_target = Point::new(
            20.9 + visual.squirm_dx * 0.45,
            seam_line_y + 0.1 + visual.squirm_warp * 0.15,
        );
        frame.stroke(
            &canvas::Path::line(thread_start, thread_target),
            canvas::Stroke::default()
                .with_color(with_alpha(white, visual.thread_snap))
                .with_width(1.15),
        );

        let stray = clamp01((close - 0.84) / 0.16);
        if stray > 0.0 {
            frame.stroke(
                &canvas::Path::line(
                    Point::new(
                        19.2 + visual.squirm_dx * 0.12,
                        20.2 + visual.squirm_dy * 0.2,
                    ),
                    Point::new(
                        19.8 + visual.squirm_dx * 0.18,
                        20.2 + 1.4 * stray + visual.squirm_dy * 0.25,
                    ),
                ),
                canvas::Stroke::default()
                    .with_color(with_alpha(white, stray))
                    .with_width(0.95),
            );
        }
    }
}

fn draw_rays(
    frame: &mut canvas::Frame<Renderer>,
    white: Color,
    visual: Visual,
    enclosure_morph: f32,
) {
    let center = Point::new(16.0, 16.2);
    let visibility = clamp01(visual.ray_visibility);

    for i in 0..RAY_COUNT {
        let index = i as u32;
        let angle = ((i as f32 + 0.35) / RAY_COUNT as f32) * TAU - TAU * 0.25;
        let dir_x = angle.cos();
        let dir_y = angle.sin();
        let border =
            ray_enclosure_intersection(center, dir_x, dir_y, enclosure_morph).unwrap_or(12.6);
        let Some(sample) = ray_sample(index, border, visual.ray_time) else {
            continue;
        };

        let start = Point::new(
            center.x + dir_x * sample.start_r,
            center.y + dir_y * sample.start_r,
        );
        let end = Point::new(
            center.x + dir_x * sample.end_r,
            center.y + dir_y * sample.end_r,
        );
        frame.stroke(
            &canvas::Path::line(start, end),
            canvas::Stroke::default()
                .with_color(Color {
                    a: visibility * sample.alpha,
                    ..white
                })
                .with_width(sample.width),
        );

        let front_tip = Point::new(
            center.x + dir_x * sample.front_r,
            center.y + dir_y * sample.front_r,
        );
        frame.fill(
            &canvas::Path::circle(front_tip, lerp(0.18, 0.07, sample.front_t)),
            Color {
                a: visibility * lerp(0.92, 0.35, sample.front_t),
                ..white
            },
        );
    }
}

fn draw_activation_threads(
    frame: &mut canvas::Frame<Renderer>,
    white: Color,
    visual: Visual,
    seam_y: f32,
    enclosure_morph: f32,
) {
    let center = Point::new(16.0, 16.2);
    let visibility = 1.0;
    let progress = clamp01(visual.ray_time / RAY_STITCH_DURATION.as_secs_f32());
    let attack = ease_out_quart(clamp01(progress / RAY_STITCH_ATTACK_PORTION));
    let collapse = ease_in_cubic(clamp01(
        (progress - RAY_STITCH_COLLAPSE_START)
            / (RAY_STITCH_COLLAPSE_END - RAY_STITCH_COLLAPSE_START),
    ));

    for i in 0..RAY_COUNT {
        let index = i as u32;
        let angle = ((i as f32 + 0.35) / RAY_COUNT as f32) * TAU - TAU * 0.25;
        let dir_x = angle.cos();
        let dir_y = angle.sin();
        let border =
            ray_enclosure_intersection(center, dir_x, dir_y, enclosure_morph).unwrap_or(12.6);
        let Some(frozen) = ray_sample(index, border, visual.ray_freeze_time) else {
            continue;
        };

        let inner = Point::new(
            center.x + dir_x * frozen.start_r,
            center.y + dir_y * frozen.start_r,
        );
        let outer = Point::new(
            center.x + dir_x * frozen.end_r,
            center.y + dir_y * frozen.end_r,
        );
        let target_x = lerp(9.4, 22.6, hash01(index, 607));
        let target_y = seam_y + (hash01(index, 613) - 0.5) * 0.95;
        let mut tip = lerp_point(inner, Point::new(target_x, target_y), attack);

        let pulse = ((visual.ray_time * 88.0) + index as f32 * 0.67).floor() as i32;
        let sign = if pulse & 1 == 0 { -1.0 } else { 1.0 };
        let violence = sign * (1.0 - attack) * (0.35 + hash01(index, 617) * 0.55);
        tip.x += -dir_y * violence;
        tip.y += dir_x * violence;

        let back = lerp_point(outer, tip, collapse);
        let dx = tip.x - back.x;
        let dy = tip.y - back.y;
        if (dx * dx + dy * dy) < 0.0025 {
            continue;
        }

        let retract = clamp01(1.0 - collapse * 1.08);
        let dissolve = clamp01(1.0 - ((progress - 0.9).max(0.0) / 0.1));
        let alpha = visibility * frozen.alpha * lerp(0.8, 1.0, attack) * retract * dissolve;
        if alpha <= 0.01 {
            continue;
        }

        frame.stroke(
            &canvas::Path::line(back, tip),
            canvas::Stroke::default()
                .with_color(with_alpha(white, alpha))
                .with_width(lerp(frozen.width + 0.05, 0.55, collapse)),
        );

        if attack > 0.2 && collapse < 0.92 {
            frame.fill(
                &canvas::Path::circle(tip, lerp(0.12, 0.06, collapse)),
                with_alpha(white, alpha * 0.92),
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RaySample {
    start_r: f32,
    end_r: f32,
    front_r: f32,
    alpha: f32,
    width: f32,
    front_t: f32,
}

fn ray_sample(index: u32, border: f32, ray_time: f32) -> Option<RaySample> {
    let outer_base = border + 2.7 + hash01(index, 433) * 4.6;
    let cycle = lerp(RAY_CYCLE_MIN, RAY_CYCLE_MAX, hash01(index, 457));
    let phase_offset = hash01(index, 491);
    let phase = ((ray_time / cycle) + phase_offset).fract();
    if phase >= RAY_ACTIVE_PORTION {
        return None;
    }

    let active = phase / RAY_ACTIVE_PORTION;
    let front_speed = lerp(0.95, 1.55, hash01(index, 503));
    let tail_speed = lerp(0.72, 1.62, hash01(index, 509));
    let tail_delay = hash01(index, 521) * RAY_TAIL_DELAY_MAX;

    let front_t = clamp01(active * front_speed);
    let tail_t = if active <= tail_delay {
        0.0
    } else {
        clamp01((active - tail_delay) / (1.0 - tail_delay) * tail_speed)
    };

    let front_r = lerp(outer_base, border, ease_out_quart(front_t));
    let tail_r = lerp(outer_base, border, ease_in_cubic(tail_t));
    let start_r = front_r.min(tail_r);
    let end_r = front_r.max(tail_r);
    if (end_r - start_r) <= 0.015 {
        return None;
    }

    let fade = clamp01(1.0 - ((active - 0.84).max(0.0) / 0.16));
    let alpha = lerp(0.62, 1.0, clamp01(front_t * 1.15)) * fade;
    if alpha <= 0.01 {
        return None;
    }

    Some(RaySample {
        start_r,
        end_r,
        front_r,
        alpha,
        width: lerp(0.45, 1.02, hash01(index, 487)),
        front_t,
    })
}

fn fearful_idle(elapsed: Duration) -> FearVisual {
    let t = elapsed.as_secs_f32();
    let cycle_duration = fear_cycle_duration();
    let local_t = if cycle_duration > 0.0 {
        t % cycle_duration
    } else {
        0.0
    };

    let (step, step_start, _) = fear_step_at(local_t);
    let step_t = local_t - step_start;

    // Even steps target left, odd steps target right.
    let to_side = if step & 1 == 0 { -1.0 } else { 1.0 };
    let from_side = -to_side;
    let from = from_side * FEAR_SACCADE_AMPLITUDE;
    let to = to_side * FEAR_SACCADE_AMPLITUDE;

    let base_dx = if step_t < FEAR_DART_DURATION {
        let snap = ease_out_quart(step_t / FEAR_DART_DURATION);
        let overshoot = (1.0 - snap) * to_side * 0.14;
        lerp(from, to, snap) + overshoot
    } else {
        to
    };

    let tremor_x =
        ((t * TAU * 5.4).sin() * FEAR_TREMOR_X) + ((t * TAU * 9.7).sin() * FEAR_TREMOR_X * 0.48);
    let tremor_y =
        ((t * TAU * 6.3).sin() * FEAR_TREMOR_Y) + ((t * TAU * 11.1).sin() * FEAR_TREMOR_Y * 0.5);

    let dilation = 0.08 + 0.08 * ((t * TAU * 2.0).sin() * 0.5 + 0.5);
    let span_pulse = 0.18 * ((t * TAU * 2.7).sin() * 0.5 + 0.5);

    FearVisual {
        pupil_dx: base_dx + tremor_x * 0.25,
        jitter_x: tremor_x,
        jitter_y: tremor_y,
        open_span_boost: FEAR_OPEN_SPAN_BOOST + span_pulse,
        pupil_dilation: dilation,
    }
}

fn fear_cycle_duration() -> f32 {
    let mut total = 0.0;
    for step in 0..FEAR_HOLD_PATTERN {
        total += fear_step_duration(step);
    }
    total
}

fn fear_step_at(local_t: f32) -> (u32, f32, f32) {
    let mut cursor = 0.0;
    for step in 0..FEAR_HOLD_PATTERN {
        let duration = fear_step_duration(step);
        if local_t <= cursor + duration || step == FEAR_HOLD_PATTERN - 1 {
            return (step, cursor, duration);
        }
        cursor += duration;
    }

    let duration = fear_step_duration(0);
    (0, 0.0, duration)
}

fn fear_step_duration(step: u32) -> f32 {
    let hold = lerp(FEAR_HOLD_MIN, FEAR_HOLD_MAX, hash01(step, 751));
    FEAR_DART_DURATION + hold
}

fn hash01(seed: u32, salt: u32) -> f32 {
    let value = seed
        .wrapping_mul(1_664_525)
        .wrapping_add(1_013_904_223 ^ salt)
        ^ salt.rotate_left(seed & 15);
    ((value >> 8) & 0xffff) as f32 / 65_535.0
}

fn enclosure_morph(visual: Visual) -> f32 {
    if !visual.is_active {
        return 0.0;
    }
    ease_out_cubic(clamp01((visual.close - 0.03) / 0.97))
}

fn enclosure_vertices(morph: f32) -> [Point; 4] {
    let t = clamp01(morph);
    let tri_top = Point::new(ENC_TRI_TOP_X, ENC_TRI_TOP_Y);
    let tri_right = Point::new(ENC_TRI_RIGHT_X, ENC_TRI_RIGHT_Y);
    let tri_left = Point::new(ENC_TRI_LEFT_X, ENC_TRI_LEFT_Y);
    let box_tl = Point::new(ENC_BOX_LEFT, ENC_BOX_TOP);
    let box_tr = Point::new(ENC_BOX_RIGHT, ENC_BOX_TOP);
    let box_br = Point::new(ENC_BOX_RIGHT, ENC_BOX_BOTTOM);
    let box_bl = Point::new(ENC_BOX_LEFT, ENC_BOX_BOTTOM);

    [
        lerp_point(tri_top, box_tl, t),
        lerp_point(tri_top, box_tr, t),
        lerp_point(tri_right, box_br, t),
        lerp_point(tri_left, box_bl, t),
    ]
}

fn ray_enclosure_intersection(origin: Point, dir_x: f32, dir_y: f32, morph: f32) -> Option<f32> {
    let vertices = enclosure_vertices(morph);

    let mut nearest = f32::MAX;
    for (s0, s1) in [
        (vertices[0], vertices[1]),
        (vertices[1], vertices[2]),
        (vertices[2], vertices[3]),
        (vertices[3], vertices[0]),
    ] {
        if let Some(t) = ray_segment_intersection(origin, dir_x, dir_y, s0, s1) {
            nearest = nearest.min(t);
        }
    }

    if nearest.is_finite() {
        Some(nearest)
    } else {
        None
    }
}

fn ray_segment_intersection(
    origin: Point,
    dir_x: f32,
    dir_y: f32,
    s0: Point,
    s1: Point,
) -> Option<f32> {
    let seg_x = s1.x - s0.x;
    let seg_y = s1.y - s0.y;
    let denom = cross(dir_x, dir_y, seg_x, seg_y);
    if denom.abs() < 1e-5 {
        return None;
    }

    let rel_x = s0.x - origin.x;
    let rel_y = s0.y - origin.y;
    let t = cross(rel_x, rel_y, seg_x, seg_y) / denom;
    let u = cross(rel_x, rel_y, dir_x, dir_y) / denom;
    if t >= 0.0 && (0.0..=1.0).contains(&u) {
        Some(t)
    } else {
        None
    }
}

fn cross(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    (ax * by) - (ay * bx)
}

fn progress_between(start: Instant, now: Instant, duration: Duration) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }

    let elapsed = now.saturating_duration_since(start).as_secs_f32();
    clamp01(elapsed / duration.as_secs_f32())
}

fn ease_out_quart(t: f32) -> f32 {
    let u = 1.0 - clamp01(t);
    1.0 - (u * u * u * u)
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * clamp01(t)
}

fn lerp_point(from: Point, to: Point, t: f32) -> Point {
    let x = lerp(from.x, to.x, t);
    let y = lerp(from.y, to.y, t);
    Point::new(x, y)
}

fn clamp01(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

fn with_alpha(color: Color, alpha: f32) -> Color {
    Color {
        a: color.a * clamp01(alpha),
        ..color
    }
}

fn active_squirm(elapsed: Duration, close: f32) -> (f32, f32, f32) {
    let close_gate = clamp01((close - 0.45) / 0.55);
    if close_gate <= 0.0 {
        return (0.0, 0.0, 0.0);
    }

    let period = SQUIRM_PERIOD.as_secs_f32();
    if period <= 0.0 {
        return (0.0, 0.0, 0.0);
    }

    let t = elapsed.as_secs_f32();
    let beat = (t / period).floor() as u32;
    let beat_t = t % period;
    let impact = SQUIRM_IMPACT.as_secs_f32();
    let settle = SQUIRM_SETTLE.as_secs_f32();

    let burst = if beat_t <= impact {
        1.0
    } else if beat_t <= impact + settle {
        1.0 - ease_out_cubic((beat_t - impact) / settle)
    } else {
        0.0
    };
    let amount = burst * close_gate;
    if amount <= 0.0 {
        return (0.0, 0.0, 0.0);
    }

    let dx = signed_hash(beat, 11) * 0.55 * amount;
    let dy = signed_hash(beat, 23) * 0.38 * amount;
    let warp = signed_hash(beat, 37) * 1.0 * amount;
    (dx, dy, warp)
}

fn signed_hash(seed: u32, salt: u32) -> f32 {
    let value = seed
        .wrapping_mul(1_664_525)
        .wrapping_add(1_013_904_223 ^ salt)
        ^ salt.rotate_left(seed & 15);
    let unit = ((value >> 8) & 0xffff) as f32 / 65_535.0;
    unit * 2.0 - 1.0
}

fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - clamp01(t);
    1.0 - (u * u * u)
}

fn ease_in_cubic(t: f32) -> f32 {
    let x = clamp01(t);
    x * x * x
}

fn prohibited_kick_x(elapsed: Duration) -> f32 {
    if elapsed >= PROHIBIT_KICK_DURATION {
        return 0.0;
    }

    let ms = elapsed.as_millis() as u64;
    match ms {
        0..=26 => -3.0,
        27..=50 => 3.0,
        51..=74 => -2.0,
        75..=98 => 2.0,
        99..=124 => -1.0,
        125..=159 => 1.0,
        _ => 0.0,
    }
}
