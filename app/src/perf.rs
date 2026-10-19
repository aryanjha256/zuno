//! The performance monitor: CPU, memory, frames, background work and startup, in an overlay
//! toggled from the palette or the app menu.
//!
//! **Recorded always, shown on request.** Startup happens once, before anyone could open the
//! monitor, so measuring only while it is open would leave that section empty forever. The
//! per-frame cost is one `Instant::now` and an uncontended lock.
//!
//! Process-wide rather than per `Workspace`: there is one window, and boot runs before any
//! workspace exists to hold the numbers.
//!
//! **The monitor's own repaint is not a frame.** It redraws once a second to show fresh numbers;
//! counted, an idle window would read 1 fps with a frame time that keeps changing, which is the
//! monitor measuring itself. `REPAINTING` marks that one frame so `frame` skips it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{div, px, Hsla, MouseButton, MouseDownEvent, PathBuilder};

use crate::theme::Theme;
use crate::ui::{glyph, Icon};
use crate::workspace::Workspace;

/// Frames kept for the chart and "worst".
const FRAMES: usize = 60;
/// Samples kept for the CPU and memory lines: a minute at one a second.
const SAMPLES: usize = 60;
/// How often an open monitor samples and repaints.
pub const REFRESH: Duration = Duration::from_secs(1);
/// One frame at 60 Hz, the line the chart is drawn against.
const BUDGET: Duration = Duration::from_micros(16_667);
/// Fewer frames than this in the last second reads as idle rather than as a rate.
const IDLE_BELOW: usize = 2;
/// Linux reports CPU time in `USER_HZ`, which is 100 on every architecture it runs on.
#[cfg(target_os = "linux")]
const TICKS_PER_SECOND: f64 = 100.0;

#[derive(Clone, Copy)]
pub struct BodyIndex {
    pub took: Duration,
    pub bytes: usize,
    pub rows: usize,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub boot: Vec<(&'static str, Duration)>,
    pub first_frame: Option<Duration>,
    pub frames: Vec<Duration>,
    pub fps: usize,
    pub body_index: Option<BodyIndex>,
    pub diff: Option<Duration>,
    pub session_save: Option<Duration>,
}

struct Stats {
    start: Option<Instant>,
    boot: Vec<(&'static str, Duration)>,
    first_frame: Option<Duration>,
    /// When each frame finished and how long it took.
    frames: VecDeque<(Instant, Duration)>,
    body_index: Option<BodyIndex>,
    diff: Option<Duration>,
    session_save: Option<Duration>,
}

static STATS: Mutex<Stats> = Mutex::new(Stats {
    start: None,
    boot: Vec::new(),
    first_frame: None,
    frames: VecDeque::new(),
    body_index: None,
    diff: None,
    session_save: None,
});

static REPAINTING: AtomicBool = AtomicBool::new(false);

fn with<R>(f: impl FnOnce(&mut Stats) -> R) -> R {
    // A panic elsewhere while holding the lock should not take the monitor down with it.
    let mut stats = STATS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut stats)
}

/// The process clock that startup stages and the first frame are measured from.
pub fn started(at: Instant) {
    with(|stats| stats.start = Some(at));
}

pub fn boot_stage(stage: &'static str, elapsed: Duration) {
    with(|stats| stats.boot.push((stage, elapsed)));
}

/// One frame's CPU time: from the root view's `render` to the last element painted.
pub fn frame(took: Duration) {
    let own_repaint = REPAINTING.swap(false, Ordering::Relaxed);
    with(|stats| stats.record_frame(took, own_repaint, Instant::now()));
}

impl Stats {
    fn record_frame(&mut self, took: Duration, own_repaint: bool, now: Instant) {
        if self.first_frame.is_none() {
            self.first_frame = self.start.map(|start| now.duration_since(start));
        }
        if own_repaint {
            return;
        }
        if self.frames.len() == FRAMES {
            self.frames.pop_front();
        }
        self.frames.push_back((now, took));
    }
}

/// The next frame is the monitor refreshing itself; leave it out.
pub fn repainting() {
    REPAINTING.store(true, Ordering::Relaxed);
}

pub fn body_indexed(took: Duration, bytes: usize, rows: usize) {
    with(|stats| stats.body_index = Some(BodyIndex { took, bytes, rows }));
}

pub fn diffed(took: Duration) {
    with(|stats| stats.diff = Some(took));
}

pub fn session_saved(took: Duration) {
    with(|stats| stats.session_save = Some(took));
}

pub fn snapshot() -> Snapshot {
    let now = Instant::now();
    with(|stats| Snapshot {
        boot: stats.boot.clone(),
        first_frame: stats.first_frame,
        frames: stats.frames.iter().map(|(_, took)| *took).collect(),
        fps: stats
            .frames
            .iter()
            .filter(|(at, _)| now.duration_since(*at) <= Duration::from_secs(1))
            .count(),
        body_index: stats.body_index,
        diff: stats.diff,
        session_save: stats.session_save,
    })
}

/// What one refresh reads from the OS. Taken off the UI thread.
pub struct Sample {
    at: Instant,
    cpu_ticks: Option<u64>,
    memory: Option<u64>,
}

pub fn sample() -> Sample {
    Sample {
        at: Instant::now(),
        cpu_ticks: cpu_ticks(),
        memory: memory_bytes(),
    }
}

/// Memory as GNOME System Monitor counts it: resident minus shared, which is `RssAnon`. Plain
/// resident memory (`VmRSS`) also counts shared libraries and the GPU driver's mappings in full,
/// and read well above what the system monitor showed.
///
/// A file read on Linux; macOS and Windows need per-platform calls, and say "n/a" until they have
/// them.
fn memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("RssAnon:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// User plus system CPU time, in clock ticks, from `/proc/self/stat`.
fn cpu_ticks() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // The command name is parenthesised and may hold spaces, so count fields after its end.
        // `utime` and `stime` are fields 14 and 15; the state, field 3, is the first after it.
        let rest = &stat[stat.rfind(')')? + 1..];
        let mut fields = rest.split_whitespace().skip(11);
        let user: u64 = fields.next()?.parse().ok()?;
        let system: u64 = fields.next()?.parse().ok()?;
        Some(user + system)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// An open monitor. Dropping it stops the refresh task.
pub struct Monitor {
    /// CPU as a share of the whole machine, like GNOME System Monitor, so 100% means every core.
    cpu: VecDeque<f32>,
    memory: VecDeque<u64>,
    last: Option<Sample>,
    pub _refresh: gpui::Task<()>,
}

impl Monitor {
    pub fn new(refresh: gpui::Task<()>) -> Self {
        Self {
            cpu: VecDeque::new(),
            memory: VecDeque::new(),
            last: None,
            _refresh: refresh,
        }
    }

    pub fn record(&mut self, sample: Sample) {
        if let Some(memory) = sample.memory {
            push(&mut self.memory, memory);
        }
        if let (Some(previous), Some(ticks)) = (&self.last, sample.cpu_ticks)
            && let Some(before) = previous.cpu_ticks
        {
            push(&mut self.cpu, cpu_percent(ticks.saturating_sub(before), sample.at - previous.at));
        }
        self.last = Some(sample);
    }

    pub fn memory(&self) -> Option<u64> {
        self.memory.back().copied()
    }
}

fn push<T>(series: &mut VecDeque<T>, value: T) {
    if series.len() == SAMPLES {
        series.pop_front();
    }
    series.push_back(value);
}

#[cfg(target_os = "linux")]
fn cpu_percent(ticks: u64, over: Duration) -> f32 {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let seconds = over.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    (ticks as f64 / TICKS_PER_SECOND / seconds / cores * 100.0) as f32
}

#[cfg(not(target_os = "linux"))]
fn cpu_percent(_: u64, _: Duration) -> f32 {
    0.0
}

fn ms(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

fn size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes >= KB * KB * KB {
        format!("{:.2} GB", bytes / (KB * KB * KB))
    } else if bytes >= KB * KB {
        format!("{:.1} MB", bytes / (KB * KB))
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// A line over the last minute, right-aligned so a fresh monitor fills in from the right.
fn sparkline(values: Vec<f32>, color: Hsla) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            if values.len() < 2 {
                return;
            }
            let low = values.iter().copied().fold(f32::INFINITY, f32::min);
            let high = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            // A flat series sits mid-height rather than dividing by zero.
            let span = (high - low).max(f32::EPSILON);
            let step = f32::from(bounds.size.width) / (SAMPLES - 1) as f32;
            let first = SAMPLES - values.len();
            let inset = 2.0;
            let height = f32::from(bounds.size.height) - inset * 2.0;
            let mut path = PathBuilder::stroke(px(1.5));
            for (ix, value) in values.iter().enumerate() {
                let share = if high == low { 0.5 } else { (value - low) / span };
                let point = gpui::point(
                    bounds.origin.x + px(step * (first + ix) as f32),
                    bounds.origin.y + px(inset + height * (1.0 - share)),
                );
                if ix == 0 {
                    path.move_to(point);
                } else {
                    path.line_to(point);
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        },
    )
    .w_full()
    .h(px(28.))
}

pub fn render(monitor: &Monitor, theme: &Theme, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let stats = snapshot();
    let mono = theme.mono.clone();
    let none = || "–".to_string();

    let figure = |value: String, unit: &str| {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_1()
            .child(div().font_family(mono.clone()).text_lg().text_color(theme.text).child(value))
            .child(div().text_xs().text_color(theme.text_muted).child(unit.to_string()))
    };
    let label = |text: &str| div().text_color(theme.text_muted).child(text.to_string());
    let value = |text: String| div().font_family(mono.clone()).text_color(theme.text).child(text);
    let section = || {
        div()
            .px_3()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
    };

    let pill = |text: &str, color: Hsla, bg: Hsla| {
        div()
            .px(px(7.))
            .py(px(1.))
            .rounded_full()
            .bg(bg)
            .text_color(color)
            .child(text.to_string())
    };
    let build = if cfg!(debug_assertions) {
        pill(
            "debug build · ~4× slower",
            theme.status_client_error,
            theme.status_client_error.opacity(0.14),
        )
    } else {
        pill("release", theme.text_muted, theme.bg_hover)
    };

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .pl_3()
        .pr_2()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child("Performance"),
        )
        .child(build)
        .child(div().flex_1())
        .child(
            div()
                .id("perf-monitor-close")
                .debug_selector(|| "perf-monitor-close".to_string())
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_pointer()
                .group(crate::ui::ICON_GROUP)
                .hover(|style| style.bg(theme.bg_hover))
                .active(|style| style.bg(theme.bg))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.close_perf_monitor(cx);
                    }),
                )
                .child(glyph(Icon::Close, theme.text_faint, theme.text, 12.)),
        );

    // CPU and memory side by side, each a number over its last minute.
    let cpu_now = monitor.cpu.back().copied();
    let cell = |name: &str, figure: gpui::Div, line: gpui::AnyElement| {
        div()
            .flex_1()
            .min_w(px(0.))
            .px_3()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(label(name))
            .child(figure)
            .child(line)
    };
    let (memory_value, memory_unit) = match monitor.memory() {
        Some(bytes) => {
            let text = size(bytes);
            let (number, unit) = text.split_once(' ').unwrap_or((&text, ""));
            (number.to_string(), unit.to_string())
        }
        None => ("n/a".to_string(), String::new()),
    };
    let usage = div()
        .flex()
        .flex_row()
        .border_b_1()
        .border_color(theme.border)
        .child(cell(
            "CPU",
            figure(
                cpu_now.map_or_else(|| if cfg!(target_os = "linux") { none() } else { "n/a".into() }, |cpu| format!("{cpu:.1}")),
                if cpu_now.is_some() { "%" } else { "" },
            ),
            sparkline(monitor.cpu.iter().copied().collect(), theme.method_get).into_any_element(),
        ))
        .child(div().w(px(1.)).bg(theme.border))
        .child(cell(
            "Memory",
            figure(memory_value, &memory_unit),
            sparkline(
                monitor.memory.iter().map(|bytes| *bytes as f32).collect(),
                theme.method_patch,
            )
            .into_any_element(),
        ));

    // Frames: the rate, or idle, then the last and worst against the 60 Hz budget.
    let rate = if stats.fps < IDLE_BELOW {
        pill("idle, nothing redrawing", theme.text_muted, theme.bg_hover).into_any_element()
    } else {
        figure(stats.fps.to_string(), "fps").into_any_element()
    };
    let stat = |name: &str, text: String| {
        div()
            .flex()
            .flex_row()
            .gap_1()
            .child(div().text_color(theme.text_muted).child(name.to_string()))
            .child(value(text))
    };
    let chart_height = 36.0;
    let budget_at = 30.0;
    let bars = stats.frames.iter().map(|took| {
        let share = took.as_secs_f32() / BUDGET.as_secs_f32();
        let height = (share * budget_at).clamp(2.0, chart_height);
        let color = if *took > BUDGET / 2 {
            theme.status_client_error
        } else {
            theme.accent
        };
        div().flex_1().h(px(height)).rounded_t(px(1.)).bg(color)
    });
    let frames = section()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(label("Frames"))
                .child(div().flex_1())
                .child(rate),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .text_xs()
                .child(stat("last", stats.frames.last().map_or_else(none, |took| ms(*took))))
                .child(stat("worst", stats.frames.iter().max().map_or_else(none, |took| ms(*took))))
                .child(stat("budget", ms(BUDGET))),
        )
        .child(
            div()
                .relative()
                .h(px(chart_height))
                .flex()
                .flex_row()
                .items_end()
                .gap(px(1.))
                .children(bars)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(budget_at))
                        .border_t_1()
                        .border_dashed()
                        .border_color(theme.text_faint),
                ),
        );

    let job = |name: &str, detail: Option<String>, took: Option<Duration>| {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_2()
            .child(div().text_color(theme.text).child(name.to_string()))
            .children(detail.map(|detail| div().text_xs().text_color(theme.text_faint).child(detail)))
            .child(div().flex_1())
            .child(value(took.map_or_else(none, ms)))
    };
    let background = section()
        .child(label("Background work"))
        .child(job(
            "Body index",
            stats
                .body_index
                .map(|index| format!("{} · {} rows", size(index.bytes as u64), index.rows)),
            stats.body_index.map(|index| index.took),
        ))
        .child(job("Response diff", None, stats.diff))
        .child(job("Session save", None, stats.session_save));

    // Startup as one bar split by stage. The marks are times since launch, so each stage's share
    // is the gap to the one before it.
    let mut marks = stats.boot.clone();
    if let Some(first) = stats.first_frame {
        marks.push(("first frame", first));
    }
    let colors = [
        theme.text_faint,
        theme.method_patch,
        theme.method_get,
        theme.kind_grpc,
        theme.status_client_error,
    ];
    let stages: Vec<(&'static str, Duration, Hsla)> = marks
        .iter()
        .enumerate()
        .map(|(ix, (stage, at))| {
            let before = ix.checked_sub(1).map_or(Duration::ZERO, |prev| marks[prev].1);
            (*stage, at.saturating_sub(before), colors[ix % colors.len()])
        })
        .collect();
    let total = marks.last().map(|(_, at)| *at);
    let startup = div()
        .px_3()
        .pt_3()
        .pb(px(14.))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .child(label("Startup"))
                .child(div().flex_1())
                .child(value(total.map_or_else(none, ms))),
        )
        .child(
            div()
                .h(px(8.))
                .flex()
                .flex_row()
                .gap(px(1.))
                .rounded_sm()
                .overflow_hidden()
                .children(stages.iter().map(|(_, took, color)| {
                    div().flex_grow().flex_basis(px(took.as_secs_f32() * 1000.0)).bg(*color)
                })),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .text_xs()
                .children(stages.iter().map(|(stage, took, color)| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(div().size(px(8.)).rounded(px(2.)).bg(*color))
                        .child(div().text_color(theme.text_muted).child(stage.to_string()))
                        .child(div().flex_1())
                        .child(value(ms(*took)))
                })),
        );

    div()
        .id("perf-monitor")
        .debug_selector(|| "perf-monitor".to_string())
        .occlude()
        .absolute()
        .top(px(crate::chrome::TITLEBAR_HEIGHT + 8.))
        .right(px(16.))
        .w(px(320.))
        .rounded_md()
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_xs()
        .flex()
        .flex_col()
        .child(header)
        .child(usage)
        .child(frames)
        .child(background)
        .child(startup)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counted, the once-a-second refresh made an idle window read 1 fps with a frame time that
    /// never settled: the monitor measuring itself.
    #[test]
    fn the_monitors_own_repaint_is_not_counted_as_a_frame() {
        let start = Instant::now();
        let mut stats = Stats {
            start: Some(start),
            boot: Vec::new(),
            first_frame: None,
            frames: VecDeque::new(),
            body_index: None,
            diff: None,
            session_save: None,
        };
        stats.record_frame(Duration::from_millis(3), true, start + Duration::from_millis(5));
        assert!(stats.frames.is_empty());
        assert!(stats.first_frame.is_some(), "the first paint still marks startup's end");

        stats.record_frame(Duration::from_millis(4), false, start + Duration::from_millis(9));
        assert_eq!(stats.frames.len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_and_memory_are_read_on_linux() {
        let bytes = memory_bytes().expect("RssAnon should be readable");
        assert!(bytes > 1024 * 1024, "a running test binary uses more than 1 MB: {bytes}");
        // Burn some CPU so the tick count has to move.
        let before = cpu_ticks().expect("utime and stime should be readable");
        let spin = Instant::now();
        let mut x = 0u64;
        while spin.elapsed() < Duration::from_millis(100) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        assert!(cpu_ticks().unwrap() > before, "100 ms of spinning should show as CPU time");
    }
}
