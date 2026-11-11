//! The power measurement view: capture controls, a current-over-time chart
//! with selection, and statistics, in the spirit of Nordic's Power Profiler.
//!
//! The same view is shown full size on the Power page and compact on the
//! Debug page when power is measured during a debug session.
use super::{
    assets::Lucide,
    widgets::{dot, notice},
};
use crate::{
    platform,
    power::{
        self, Capture, CaptureEvent, DURATIONS, Range, Recording, SAMPLE_RATES, Source, Stats,
    },
    worker::Level,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
    switch::Switch,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    AnyElement, App, Bounds, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, PathBuilder, Pixels,
    Render, ScrollEvent, SharedString, StatefulInteractiveElement as _, Styled, Task,
    TestSupportExt as _, Window, canvas, div, fill, point, prelude::FluentBuilder as _, px,
    relative,
};
use std::sync::mpsc;
use std::{cell::Cell, path::PathBuf, rc::Rc, time::Duration};

/// Time windows offered, in seconds; `None` shows the whole recording.
const SPANS: [Option<f64>; 6] = [
    Some(0.01),
    Some(0.1),
    Some(1.0),
    Some(10.0),
    Some(60.0),
    None,
];
/// Chart heights.
const FULL_HEIGHT: f32 = 300.;
const COMPACT_HEIGHT: f32 = 150.;
/// Width of the value labels left of the chart.
const AXIS_WIDTH: f32 = 64.;
/// The lowest and highest currents the chart shows.
const FLOOR: f64 = 1e-8;
const CEILING: f64 = 2.0;

/// Power measurement state and its view.
pub struct PowerView {
    recording: Recording,
    capture: Option<Capture>,
    source: Source,
    rate: u32,
    duration: Option<Duration>,
    error: Option<SharedString>,
    span: Option<f64>,
    /// End of the visible window; `None` follows the newest sample.
    end: Option<f64>,
    log_scale: bool,
    selection: Option<(f64, f64)>,
    /// Shift-drag in progress: where the selection started, in seconds.
    drag_from: Option<f64>,
    /// Drag in progress: where it started (pixels) and the window then.
    pan_from: Option<(f32, f64, f64)>,
    hover: Option<f64>,
    chart: Rc<Cell<Bounds<Pixels>>>,
    compact: bool,
    /// Samples from the running capture, drained by `poll`.
    events: Option<mpsc::Receiver<CaptureEvent>>,
    poll: Option<Task<()>>,
}

/// How often samples are taken over from the capture thread: also the
/// chart's refresh rate while recording.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

impl PowerView {
    /// An idle view with no recording.
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            recording: Recording::new(SAMPLE_RATES[1]),
            capture: None,
            source: Source::default(),
            rate: SAMPLE_RATES[1],
            duration: None,
            error: None,
            span: Some(10.0),
            end: None,
            log_scale: true,
            selection: None,
            drag_from: None,
            pan_from: None,
            hover: None,
            chart: Rc::new(Cell::new(Bounds::default())),
            compact: false,
            events: None,
            poll: None,
        }
    }

    // ---- State ----------------------------------------------------------------

    /// The samples recorded so far.
    pub fn recording(&self) -> &Recording {
        &self.recording
    }

    /// Whether a capture is running.
    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// Why the last capture failed, if it did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The selected part of the recording, in seconds.
    pub fn selection(&self) -> Option<(f64, f64)> {
        self.selection
    }

    /// Shows the compact layout used on the Debug page.
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact != compact {
            self.compact = compact;
            cx.notify();
        }
    }

    /// Chooses where samples come from.
    pub fn set_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if !self.is_capturing() {
            self.source = source;
            self.error = None;
            cx.notify();
        }
    }

    /// Starts a new recording.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.is_capturing() {
            return;
        }
        self.recording = Recording::new(self.rate);
        self.selection = None;
        self.drag_from = None;
        self.end = None;
        self.error = None;
        let (sender, receiver) = mpsc::channel();
        self.capture = Some(Capture::start(
            self.source,
            self.rate,
            self.duration,
            sender,
        ));
        self.events = Some(receiver);
        // Samples are picked up on a timer rather than by waking the UI from
        // the capture thread, so updates arrive in steady batches.
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let running = this.update(cx, |this, cx| this.drain(cx)).unwrap_or(false);
                if !running {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Shows a finished recording, replacing the current one.
    pub fn show_recording(&mut self, recording: Recording, cx: &mut Context<Self>) {
        if self.is_capturing() {
            return;
        }
        self.recording = recording;
        self.selection = None;
        self.span = Some(10.0);
        self.end = None;
        cx.notify();
    }

    /// The visible time window, in seconds.
    pub fn window(&self) -> (f64, f64) {
        self.visible()
    }

    /// Stops the recording; the samples are kept.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(capture) = &self.capture {
            capture.stop();
        }
        cx.notify();
    }

    /// Takes over what the capture produced; false once it has ended.
    fn drain(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(events) = &self.events else {
            return false;
        };
        let mut changed = false;
        let mut running = true;
        loop {
            match events.try_recv() {
                Ok(CaptureEvent::Samples(samples)) => {
                    self.recording.extend(&samples);
                    changed = true;
                }
                Ok(CaptureEvent::Ended(result)) => {
                    self.error = result.err().map(Into::into);
                    running = false;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
        }
        if !running {
            self.capture = None;
            self.events = None;
            changed = true;
        }
        if changed {
            cx.notify();
        }
        running
    }

    /// The visible time window, in seconds.
    fn visible(&self) -> (f64, f64) {
        let total = self.recording.duration();
        match self.span {
            None => (0.0, total.max(1e-3)),
            Some(span) => {
                let end = self.end.unwrap_or(total.max(span));
                (end - span, end)
            }
        }
    }

    /// Time under a window position, when it is over the chart.
    fn time_at(&self, x: Pixels) -> Option<f64> {
        let bounds = self.chart.get();
        let width = f64::from(bounds.size.width);
        if width <= 0.0 {
            return None;
        }
        let fraction = (f64::from(x - bounds.origin.x) / width).clamp(0.0, 1.0);
        let (from, to) = self.visible();
        Some((from + fraction * (to - from)).clamp(0.0, self.recording.duration()))
    }

    fn set_span(&mut self, span: Option<f64>, cx: &mut Context<Self>) {
        self.span = span;
        self.end = None;
        cx.notify();
    }

    /// Shortest window the chart zooms to, in seconds.
    fn min_span(&self) -> f64 {
        (8.0 / f64::from(self.recording.rate())).max(0.001)
    }

    /// Shows `from..to` seconds. Reaching the newest sample follows it
    /// again; covering everything shows "All".
    fn show_window(&mut self, from: f64, to: f64) {
        let total = self.recording.duration();
        let span = to - from;
        if total > 0.0 && span >= total {
            self.span = None;
            self.end = None;
        } else {
            self.span = Some(span);
            self.end = (to < total - span * 1e-6).then_some(to);
        }
    }

    /// Like Nordic's Power Profiler: the wheel zooms around the pointer,
    /// sideways scrolling pans.
    fn on_scroll(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(20.));
        let (dx, dy) = (f64::from(delta.x), f64::from(delta.y));
        let (from, to) = self.visible();
        let limit = self.recording.duration().max(to - from);
        let width = f64::from(self.chart.get().size.width).max(1.0);
        // The chart has the wheel: the page around it must not scroll too.
        cx.stop_propagation();
        let window = if dy.abs() >= dx.abs() && dy != 0.0 {
            let Some(anchor) = self.time_at(event.position.x) else {
                return;
            };
            // One wheel notch (20 px) zooms by a fifth.
            let factor = 1.2f64.powf(dy / 20.0);
            zoom(from, to, anchor, factor, self.min_span(), limit)
        } else if dx != 0.0 {
            pan(from, to, -dx / width * (to - from), limit)
        } else {
            return;
        };
        self.show_window(window.0, window.1);
        cx.notify();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        if event.modifiers.shift {
            if let Some(time) = self.time_at(event.position.x) {
                self.drag_from = Some(time);
                self.selection = None;
            }
        } else {
            let (from, to) = self.visible();
            self.pan_from = Some((f32::from(event.position.x), from, to));
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let time = self.time_at(event.position.x);
        self.hover = time;
        if event.pressed_button == Some(MouseButton::Left) {
            if let (Some(from), Some(time)) = (self.drag_from, time) {
                self.selection = Some((from.min(time), from.max(time)));
            } else if let Some((x, from, to)) = self.pan_from {
                let width = f64::from(self.chart.get().size.width).max(1.0);
                let moved = f64::from(f32::from(event.position.x) - x);
                let limit = self.recording.duration().max(to - from);
                let (from, to) = pan(from, to, -moved / width * (to - from), limit);
                self.show_window(from, to);
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, cx: &mut Context<Self>) {
        self.pan_from = None;
        if self.drag_from.take().is_some()
            && let Some((from, to)) = self.selection
        {
            // A Shift-click without a drag clears the selection.
            let (start, end) = self.visible();
            if to - from < (end - start) / 500.0 {
                self.selection = None;
            }
        }
        cx.notify();
    }

    /// Saves the selection, or everything, as CSV.
    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recording.is_empty() {
            return;
        }
        let (from, to) = self.selection.unwrap_or((0.0, self.recording.duration()));
        let directory = platform::data_dir().unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&directory);
        let receiver = cx.prompt_for_new_path(&directory, Some("power.csv"));
        let recording = self.recording.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            let written = cx
                .background_executor()
                .spawn({
                    let path: PathBuf = path.clone();
                    async move {
                        let file = std::fs::File::create(&path)?;
                        let mut out = std::io::BufWriter::new(file);
                        recording.write_csv(from, to, &mut out)?;
                        std::io::Write::flush(&mut out)
                    }
                })
                .await;
            let _ = cx.update(|window, cx| {
                let note = match written {
                    Ok(()) => Notification::success(format!("Saved {}", path.display())),
                    Err(error) => {
                        Notification::error(format!("Cannot save {}: {error}", path.display()))
                    }
                };
                use gpui_kit::component::WindowExt as _;
                window.push_notification(note, cx);
            });
        })
        .detach();
    }

    // ---- Rendering ----------------------------------------------------------------

    fn range_color(range: Range, cx: &App) -> Hsla {
        let theme = cx.theme();
        match range {
            Range::Low => theme.green,
            Range::Mid => theme.yellow,
            Range::High => theme.magenta,
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let capturing = self.is_capturing();
        let source_index = match self.source {
            Source::SoulInjector => 0,
            Source::Simulated => 1,
        };
        let rate_index = SAMPLE_RATES
            .iter()
            .position(|r| *r == self.rate)
            .unwrap_or(1);
        let duration_index = DURATIONS
            .iter()
            .position(|d| *d == self.duration)
            .unwrap_or(0);

        let segmented = |id: &'static str, labels: Vec<SharedString>, selected: usize| {
            TabBar::new(id)
                .segmented()
                .small()
                .selected_index(selected)
                .children(labels.into_iter().map(|label| Tab::new().label(label)))
        };

        let source = segmented(
            "power-source",
            vec!["Soul Injector".into(), "Simulated preview".into()],
            source_index,
        )
        .when(!capturing, |this| {
            this.on_click(cx.listener(|this, index: &usize, _, cx| {
                let source = if *index == 0 {
                    Source::SoulInjector
                } else {
                    Source::Simulated
                };
                this.set_source(source, cx);
            }))
        });
        let rate = segmented(
            "power-rate",
            SAMPLE_RATES
                .iter()
                .map(|r| {
                    if *r >= 1000 {
                        format!("{} kS/s", f64::from(*r) / 1000.0).into()
                    } else {
                        format!("{r} S/s").into()
                    }
                })
                .collect(),
            rate_index,
        )
        .when(!capturing, |this| {
            this.on_click(cx.listener(|this, index: &usize, _, cx| {
                this.rate = SAMPLE_RATES[*index];
                cx.notify();
            }))
        });
        let duration = segmented(
            "power-duration",
            DURATIONS
                .iter()
                .map(|d| match d {
                    None => "Until stopped".into(),
                    Some(d) if d.as_secs() >= 60 => format!("{} min", d.as_secs() / 60).into(),
                    Some(d) => format!("{} s", d.as_secs()).into(),
                })
                .collect(),
            duration_index,
        )
        .when(!capturing, |this| {
            this.on_click(cx.listener(|this, index: &usize, _, cx| {
                this.duration = DURATIONS[*index];
                cx.notify();
            }))
        });

        let start = if capturing {
            Button::new("power-start")
                .danger()
                .icon(Lucide::CircleStop)
                .label("Stop")
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("power-start")
                .primary()
                .icon(Lucide::CirclePlay)
                .label("Start")
                .on_click(cx.listener(|this, _, _, cx| this.start(cx)))
        };

        let label = |text: &'static str| {
            div()
                .w(px(96.))
                .flex_none()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_3()
                    .child(label("Measure from"))
                    .child(source)
                    .child(div().flex_1())
                    .child(start),
            )
            .when(!self.compact, |this| {
                this.child(
                    h_flex()
                        .gap_3()
                        .flex_wrap()
                        .child(label("Sample rate"))
                        .child(rate)
                        .child(div().w(px(12.)))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Record"),
                        )
                        .child(duration),
                )
            })
            .into_any_element()
    }

    fn render_readouts(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let last = self.recording.last();
        let current: SharedString = last
            .map(|s| power::format_current(f64::from(s.current)).into())
            .unwrap_or_else(|| "–".into());
        let supply: SharedString = last
            .map(|s| format!("{:.2} V", s.supply).into())
            .unwrap_or_else(|| "–".into());
        let range = last.map(|s| s.range);
        let readout = |label: &'static str, value: SharedString, large: bool| {
            v_flex()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .when(large, |this| this.text_xl())
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(value),
                )
        };
        h_flex()
            .gap_8()
            .items_end()
            .child(readout("Current", current, true))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Range"),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when_some(range, |this, range| {
                                this.child(dot(Self::range_color(range, cx)))
                            })
                            .child(range.map(|r| r.label()).unwrap_or("–")),
                    ),
            )
            .child(readout("Supply (VPP in)", supply, false))
            .child(readout(
                "Recorded",
                format!(
                    "{} · {} samples",
                    power::format_time(self.recording.duration()),
                    self.recording.len()
                )
                .into(),
                false,
            ))
            .into_any_element()
    }

    fn render_chart(&self, height: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (from, to) = self.visible();
        let columns = (f32::from(self.chart.get().size.width) as usize).clamp(100, 4000);
        let samples_visible = self.recording.indices(from, to).len();

        // Fewer samples than pixels: draw them as a line through each sample.
        let plot = if samples_visible <= columns {
            let rate = f64::from(self.recording.rate());
            let range = self.recording.indices(from, to);
            let start = range.start.saturating_sub(1);
            let end = (range.end + 1).min(self.recording.len());
            Plot::Line(
                (start..end)
                    .filter_map(|i| {
                        let sample = self.recording.sample(i)?;
                        Some(((i as f64 / rate - from) / (to - from), sample.current))
                    })
                    .collect(),
            )
        } else {
            Plot::Envelope(self.recording.envelope(from, to, columns))
        };
        let (low, high) = value_range(&plot, self.log_scale);
        let scale = Scale {
            low,
            high,
            log: self.log_scale,
        };
        let value_ticks = scale.ticks();
        let time_ticks = time_ticks(from, to);

        let selection = self.selection.map(|(a, b)| {
            (
                ((a - from) / (to - from)) as f32,
                ((b - from) / (to - from)) as f32,
            )
        });
        let hover = self.hover.map(|t| ((t - from) / (to - from)) as f32);
        let colors = ChartColors {
            line: theme.chart_1,
            grid: theme.border.opacity(0.6),
            selection: theme.chart_1.opacity(0.15),
            hover: theme.muted_foreground.opacity(0.6),
        };
        let bounds_cell = self.chart.clone();
        let tick_fractions: Vec<f32> = value_ticks.iter().map(|(f, _)| *f).collect();
        let time_fractions: Vec<f32> = time_ticks.iter().map(|(f, _)| *f).collect();
        let painted = canvas(
            move |bounds, _, _| bounds_cell.set(bounds),
            move |bounds, _, window, _| {
                paint_chart(
                    bounds,
                    window,
                    &plot,
                    &scale,
                    &tick_fractions,
                    &time_fractions,
                    selection,
                    hover,
                    &colors,
                )
            },
        )
        .size_full();

        let hover_text = self.hover.and_then(|t| {
            let sample = self.recording.sample_at(t)?;
            Some(format!(
                "{}  ·  {}  ·  {}",
                power::format_time(t),
                power::format_current(f64::from(sample.current)),
                sample.range.label()
            ))
        });
        let empty = self.recording.is_empty();

        let value_labels = div()
            .relative()
            .flex_none()
            .w(px(AXIS_WIDTH))
            .h(px(height))
            .children(value_ticks.into_iter().map(|(fraction, label)| {
                div()
                    .absolute()
                    .right(px(8.))
                    .top(px((1.0 - fraction) * height - 7.0))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(label)
            }));
        let chart = div()
            .id("power-chart")
            .relative()
            .w_full()
            .h(px(height))
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .overflow_hidden()
            .cursor_crosshair()
            .child(painted)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, _, cx| this.on_mouse_down(event, cx)),
            )
            .on_mouse_move(cx.listener(|this, event, _, cx| this.on_mouse_move(event, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event, _, cx| this.on_mouse_up(event, cx)),
            )
            .on_scroll(cx.listener(|this, event, _, cx| this.on_scroll(event, cx)))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered {
                    this.hover = None;
                    cx.notify();
                }
            }))
            .when_some(hover_text, |this, text| {
                this.child(
                    div()
                        .absolute()
                        .top_1()
                        .right_2()
                        .px_2()
                        .py_0p5()
                        .rounded(theme.radius)
                        .bg(theme.popover)
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .child(text),
                )
            })
            .when(empty, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Press Start to record the target's current."),
                )
            })
            .test_support();

        let time_labels = div()
            .relative()
            .h(px(16.))
            .children(time_ticks.into_iter().map(|(fraction, label)| {
                div()
                    .absolute()
                    .left(relative(fraction))
                    .ml(px(-40.))
                    .w(px(80.))
                    .text_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(label)
            }));

        // Which range each part of the window was measured in.
        let strip_plot = self.recording.envelope(from, to, columns.min(800));
        let strip_colors = [
            Self::range_color(Range::Low, cx),
            Self::range_color(Range::Mid, cx),
            Self::range_color(Range::High, cx),
        ];
        let strip = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let columns = strip_plot.len().max(1) as f32;
                let width = f32::from(bounds.size.width) / columns;
                for (index, envelope) in strip_plot.iter().enumerate() {
                    let Some(envelope) = envelope else { continue };
                    let color = strip_colors[envelope.range as usize];
                    let x = bounds.origin.x + px(index as f32 * width);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(x, bounds.origin.y),
                            gpui_kit::size(px(width + 0.5), bounds.size.height),
                        ),
                        color,
                    ));
                }
            },
        )
        .w_full()
        .h(px(4.));

        h_flex()
            .w_full()
            .items_start()
            .child(value_labels)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(chart)
                    .child(div().rounded_sm().overflow_hidden().child(strip))
                    .child(time_labels),
            )
            .into_any_element()
    }

    fn render_chart_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let span_index = SPANS.iter().position(|s| *s == self.span);
        let spans = TabBar::new("power-span")
            .segmented()
            .small()
            .when_some(span_index, |this, index| this.selected_index(index))
            .children(SPANS.iter().map(|span| {
                Tab::new().label(match span {
                    None => SharedString::from("All"),
                    Some(s) if *s < 1.0 => format!("{} ms", (s * 1000.0).round()).into(),
                    Some(s) if *s >= 60.0 => format!("{} min", s / 60.0).into(),
                    Some(s) => format!("{s} s").into(),
                })
            }))
            .on_click(cx.listener(|this, index: &usize, _, cx| this.set_span(SPANS[*index], cx)));

        h_flex()
            .gap_3()
            .flex_wrap()
            .child(spans)
            .child(
                Switch::new("power-log")
                    .small()
                    .checked(self.log_scale)
                    .label("Log scale")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.log_scale = *checked;
                        cx.notify();
                    })),
            )
            .when(self.end.is_some(), |this| {
                this.child(
                    Button::new("power-follow")
                        .ghost()
                        .small()
                        .label("Back to newest")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.end = None;
                            cx.notify();
                        })),
                )
            })
            .when(self.selection.is_some(), |this| {
                this.child(
                    Button::new("power-clear-selection")
                        .ghost()
                        .small()
                        .label("Clear selection")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.selection = None;
                            cx.notify();
                        })),
                )
            })
            .child(div().flex_1())
            .when(!self.compact, |this| {
                this.child(
                    Button::new("power-zero")
                        .ghost()
                        .small()
                        .label("Zero offset…")
                        .disabled(true)
                        .tooltip("Measures the offset with no target attached, then subtracts it. Arrives with the power protocol."),
                )
                .child(
                    Button::new("power-export")
                        .outline()
                        .small()
                        .icon(Lucide::Download)
                        .label(if self.selection.is_some() {
                            "Export selection"
                        } else {
                            "Export CSV"
                        })
                        .disabled(self.recording.is_empty() || self.is_capturing())
                        .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
                )
            })
            .into_any_element()
    }

    fn render_stats(&self, cx: &mut Context<Self>) -> AnyElement {
        let (from, to) = self.visible();
        let window = self.recording.stats(from, to);
        let selection = self.selection.and_then(|(a, b)| self.recording.stats(a, b));
        let card = |title: &'static str, stats: Option<Stats>, empty: &'static str| {
            let theme = cx.theme();
            let row = |label: &'static str, value: String| {
                h_flex()
                    .justify_between()
                    .text_sm()
                    .child(div().text_color(theme.muted_foreground).child(label))
                    .child(div().font_weight(FontWeight::MEDIUM).child(value))
            };
            let mut body = v_flex().gap_1();
            match stats {
                Some(stats) => {
                    body = body
                        .child(row("Average", power::format_current(stats.average)))
                        .child(row("Maximum", power::format_current(f64::from(stats.max))))
                        .child(row("Minimum", power::format_current(f64::from(stats.min))))
                        .child(row("Charge", power::format_charge(stats.charge)))
                        .child(row("Energy", power::format_energy(stats.energy)))
                        .child(row("Duration", power::format_time(stats.duration)));
                }
                None => {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(empty),
                    )
                }
            }
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .p_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(body)
        };
        h_flex()
            .gap_3()
            .items_start()
            .child(card("Window", window, "No samples in view."))
            .child(card(
                "Selection",
                selection,
                "Shift-drag across the chart to measure part of it.",
            ))
            .into_any_element()
    }

    fn render_legend(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let item = |range: Range, text: &'static str| {
            h_flex()
                .gap_1p5()
                .child(dot(Self::range_color(range, cx)))
                .child(text)
        };
        h_flex()
            .gap_4()
            .flex_wrap()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child("Range:")
            .child(item(Range::Low, "LOW, up to ~216 µA"))
            .child(item(Range::Mid, "MID, up to ~8.8 mA"))
            .child(item(Range::High, "HIGH, up to 1.6 A"))
            .child(div().flex_1())
            .child(if cfg!(target_os = "macos") {
                "Scroll to zoom · drag to pan · ⇧-drag to select"
            } else {
                "Scroll to zoom · drag to pan · Shift+drag to select"
            })
            .into_any_element()
    }
}

impl Render for PowerView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx);
        let readouts = self.render_readouts(cx);
        let height = if self.compact {
            COMPACT_HEIGHT
        } else {
            FULL_HEIGHT
        };
        let chart = self.render_chart(height, cx);
        let controls = self.render_chart_controls(cx);
        let stats = (!self.compact).then(|| self.render_stats(cx));
        let legend = self.render_legend(cx);
        let simulated = self.source == Source::Simulated;
        v_flex()
            .w_full()
            .gap_4()
            .child(toolbar)
            .when(simulated, |this| {
                this.child(notice(
                    Level::Warning,
                    "Simulated preview: these numbers are generated, not measured.",
                    cx,
                ))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(notice(Level::Info, error, cx))
            })
            .child(readouts)
            .child(chart)
            .child(controls)
            .child(legend)
            .children(stats)
    }
}

/// What the chart draws.
enum Plot {
    /// Samples as (position 0..1 across the window, current).
    Line(Vec<(f64, f32)>),
    /// Min/max per pixel column.
    Envelope(Vec<Option<power::Envelope>>),
}

/// Maps currents to heights.
#[derive(Clone, Copy)]
struct Scale {
    low: f64,
    high: f64,
    log: bool,
}

impl Scale {
    /// Height fraction from the bottom for `value`.
    fn fraction(&self, value: f64) -> f32 {
        let f = if self.log {
            let v = value.max(self.low).log10();
            (v - self.low.log10()) / (self.high.log10() - self.low.log10())
        } else {
            (value - self.low) / (self.high - self.low)
        };
        f.clamp(0.0, 1.0) as f32
    }

    /// Gridlines with labels: each decade, or about five round steps.
    fn ticks(&self) -> Vec<(f32, String)> {
        if self.log {
            let mut ticks = Vec::new();
            let mut value = self.low;
            while value <= self.high * 1.0001 {
                ticks.push((self.fraction(value), tick_label(value)));
                value *= 10.0;
            }
            ticks
        } else {
            let step = nice_step(self.high / 4.0);
            (0..)
                .map(|i| i as f64 * step)
                .take_while(|v| *v <= self.high * 1.0001)
                .map(|v| (self.fraction(v), tick_label(v)))
                .collect()
        }
    }
}

/// `from..to` zoomed by `factor` (above 1 zooms in) with `anchor` kept at
/// the same place on screen, kept within `0..limit` and at least `min_span`
/// long.
fn zoom(from: f64, to: f64, anchor: f64, factor: f64, min_span: f64, limit: f64) -> (f64, f64) {
    let span = to - from;
    let new_span = (span / factor).clamp(min_span.min(limit), limit);
    let fraction = if span > 0.0 {
        ((anchor - from) / span).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let start = anchor - fraction * new_span;
    let start = start.clamp(0.0, (limit - new_span).max(0.0));
    (start, start + new_span)
}

/// `from..to` moved by `shift` seconds, kept within `0..limit`.
fn pan(from: f64, to: f64, shift: f64, limit: f64) -> (f64, f64) {
    let span = to - from;
    let start = (from + shift).clamp(0.0, (limit - span).max(0.0));
    (start, start + span)
}

/// Lowest and highest value shown for `plot`.
fn value_range(plot: &Plot, log: bool) -> (f64, f64) {
    let (mut min, mut max) = (f64::INFINITY, 0.0f64);
    match plot {
        Plot::Line(points) => {
            for (_, value) in points {
                min = min.min(f64::from(*value));
                max = max.max(f64::from(*value));
            }
        }
        Plot::Envelope(columns) => {
            for envelope in columns.iter().flatten() {
                min = min.min(f64::from(envelope.min));
                max = max.max(f64::from(envelope.max));
            }
        }
    }
    if log {
        if !min.is_finite() || max <= 0.0 {
            return (1e-6, 1e-2);
        }
        let low = 10f64.powf(min.max(FLOOR).log10().floor());
        let high = 10f64.powf(max.max(low * 10.0).min(CEILING).log10().ceil());
        (low, high.max(low * 100.0))
    } else {
        let high = if max > 0.0 { max * 1.15 } else { 1e-3 };
        let step = nice_step(high / 4.0);
        (0.0, (high / step).ceil() * step)
    }
}

/// 1, 2 or 5 times a power of ten, at least `rough`.
fn nice_step(rough: f64) -> f64 {
    if rough <= 0.0 {
        return 1.0;
    }
    let power = 10f64.powf(rough.log10().floor());
    let mantissa = rough / power;
    let nice = if mantissa <= 1.0 {
        1.0
    } else if mantissa <= 2.0 {
        2.0
    } else if mantissa <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * power
}

/// A short current label such as "10 µA" or "2.5 mA".
fn tick_label(value: f64) -> String {
    let (scale, unit) = if value == 0.0 || value >= 1.0 {
        (1.0, "A")
    } else if value >= 1e-3 {
        (1e3, "mA")
    } else if value >= 1e-6 {
        (1e6, "µA")
    } else {
        (1e9, "nA")
    };
    let scaled = value * scale;
    if (scaled - scaled.round()).abs() < 1e-6 {
        format!("{} {unit}", scaled.round())
    } else {
        format!("{scaled:.1} {unit}")
    }
}

/// Time gridlines across `from..to` seconds, about six of them.
fn time_ticks(from: f64, to: f64) -> Vec<(f32, String)> {
    let span = to - from;
    if span <= 0.0 {
        return Vec::new();
    }
    let step = nice_step(span / 6.0);
    let decimals = (-step.log10().floor()).max(0.0) as usize;
    let first = (from / step).ceil() as i64;
    let last = (to / step).floor() as i64;
    (first..=last)
        .map(|i| {
            let time = i as f64 * step;
            let label = if step >= 60.0 {
                power::format_time(time)
            } else {
                format!("{time:.decimals$} s")
            };
            (((time - from) / span) as f32, label)
        })
        .collect()
}

struct ChartColors {
    line: Hsla,
    grid: Hsla,
    selection: Hsla,
    hover: Hsla,
}

#[allow(clippy::too_many_arguments)]
fn paint_chart(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    plot: &Plot,
    scale: &Scale,
    value_ticks: &[f32],
    time_ticks: &[f32],
    selection: Option<(f32, f32)>,
    hover: Option<f32>,
    colors: &ChartColors,
) {
    let left = f32::from(bounds.origin.x);
    let top = f32::from(bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let x = |fraction: f32| px(left + fraction * width);
    let y = |fraction: f32| px(top + (1.0 - fraction) * (height - 1.0));

    let mut grid = PathBuilder::stroke(px(1.));
    for fraction in value_ticks {
        grid.move_to(point(x(0.0), y(*fraction)));
        grid.line_to(point(x(1.0), y(*fraction)));
    }
    for fraction in time_ticks {
        grid.move_to(point(x(*fraction), px(top)));
        grid.line_to(point(x(*fraction), px(top + height)));
    }
    if let Ok(path) = grid.build() {
        window.paint_path(path, colors.grid);
    }

    if let Some((start, end)) = selection {
        let (start, end) = (start.clamp(0.0, 1.0), end.clamp(0.0, 1.0));
        window.paint_quad(fill(
            Bounds::from_corners(point(x(start), px(top)), point(x(end), px(top + height))),
            colors.selection,
        ));
    }

    let mut line = PathBuilder::stroke(px(1.));
    match plot {
        Plot::Line(points) => {
            let mut first = true;
            for (fraction, value) in points {
                let p = point(x(*fraction as f32), y(scale.fraction(f64::from(*value))));
                if first {
                    line.move_to(p);
                    first = false;
                } else {
                    line.line_to(p);
                }
            }
        }
        Plot::Envelope(columns) => {
            let count = columns.len().max(1) as f32;
            for (index, envelope) in columns.iter().enumerate() {
                let Some(envelope) = envelope else { continue };
                let column_x = x((index as f32 + 0.5) / count);
                let top_y = y(scale.fraction(f64::from(envelope.max)));
                let mut bottom_y = y(scale.fraction(f64::from(envelope.min)));
                if f32::from(bottom_y - top_y) < 1.0 {
                    bottom_y = top_y + px(1.0);
                }
                line.move_to(point(column_x, top_y));
                line.line_to(point(column_x, bottom_y));
            }
        }
    }
    if let Ok(path) = line.build() {
        window.paint_path(path, colors.line);
    }

    if let Some(fraction) = hover.filter(|f| (0.0..=1.0).contains(f)) {
        let mut cursor = PathBuilder::stroke(px(1.));
        cursor.move_to(point(x(fraction), px(top)));
        cursor.line_to(point(x(fraction), px(top + height)));
        if let Ok(path) = cursor.build() {
            window.paint_path(path, colors.hover);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_scale_spans_whole_decades() {
        let plot = Plot::Line(vec![(0.0, 4.2e-6), (1.0, 3.1e-3)]);
        let (low, high) = value_range(&plot, true);
        assert!((low - 1e-6).abs() < 1e-12, "{low}");
        assert!((high - 1e-2).abs() < 1e-9, "{high}");
        let scale = Scale {
            low,
            high,
            log: true,
        };
        let labels: Vec<String> = scale.ticks().into_iter().map(|(_, l)| l).collect();
        assert_eq!(labels, ["1 µA", "10 µA", "100 µA", "1 mA", "10 mA"]);
        assert_eq!(scale.fraction(1e-6), 0.0);
        assert_eq!(scale.fraction(1e-2), 1.0);
    }

    #[test]
    fn linear_scale_uses_round_steps() {
        let plot = Plot::Line(vec![(0.0, 0.0), (1.0, 3.1e-3)]);
        let (low, high) = value_range(&plot, false);
        assert_eq!(low, 0.0);
        let scale = Scale {
            low,
            high,
            log: false,
        };
        let labels: Vec<String> = scale.ticks().into_iter().map(|(_, l)| l).collect();
        assert_eq!(labels, ["0 A", "1 mA", "2 mA", "3 mA", "4 mA"]);
    }

    #[test]
    fn zooming_keeps_the_pointer_still() {
        // Zoom in 2x around t = 3 in a 0..10 s window of a 10 s recording.
        let (from, to) = zoom(0.0, 10.0, 3.0, 2.0, 0.01, 10.0);
        assert!(
            (from - 1.5).abs() < 1e-9 && (to - 6.5).abs() < 1e-9,
            "{from}..{to}"
        );
        // The pointer stays at 30 % across the window.
        assert!(((3.0 - from) / (to - from) - 0.3).abs() < 1e-9);
        // Zooming out stops at the whole recording.
        assert_eq!(zoom(2.0, 4.0, 3.0, 0.01, 0.01, 10.0), (0.0, 10.0));
        // Zooming in stops at the shortest window.
        let (from, to) = zoom(0.0, 1.0, 0.5, 1e6, 0.008, 10.0);
        assert!((to - from - 0.008).abs() < 1e-12);
    }

    #[test]
    fn panning_stays_within_the_recording() {
        assert_eq!(pan(2.0, 4.0, 1.0, 10.0), (3.0, 5.0));
        assert_eq!(pan(2.0, 4.0, -5.0, 10.0), (0.0, 2.0));
        assert_eq!(pan(2.0, 4.0, 50.0, 10.0), (8.0, 10.0));
    }

    #[test]
    fn time_labels_suit_the_window() {
        let ticks = time_ticks(12.0, 12.01);
        assert!(ticks.len() >= 4, "{ticks:?}");
        assert_eq!(ticks[0].1, "12.000 s");
        let minutes = time_ticks(0.0, 600.0);
        assert_eq!(minutes[1].1, "1:40");
    }
}
