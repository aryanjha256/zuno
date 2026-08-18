//! Zuno — a native API client built around the feeling of Zed.
//!
//! Milestone 1.0: the shell. Theme, focus, and key dispatch are real; text
//! editing (M1.1) and the HTTP engine (M1.2) are not. See architecture.md §10.

mod actions;
mod request_pane;
mod request_view;
mod response_pane;
mod theme;
mod workspace;

use std::time::Instant;

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions, Window, WindowBounds,
    WindowOptions, px, size,
};

use crate::actions::{
    FocusBody, FocusNext, FocusPrev, FocusResponse, FocusUrl, Quit, SendRequest, ToggleTheme,
};
use crate::theme::{Appearance, Theme};
use crate::workspace::Workspace;

/// Startup stage timings, printed when `ZUNO_TIMING=1`.
///
/// The cold-start budget is 100ms (architecture.md §8). A budget nobody measures
/// is a budget already blown, so this exists from the first commit rather than
/// being retrofitted once startup already feels slow.
struct Boot {
    start: Instant,
    enabled: bool,
}

impl Boot {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            enabled: std::env::var_os("ZUNO_TIMING").is_some(),
        }
    }

    fn mark(&self, stage: &str) {
        if self.enabled {
            eprintln!("[zuno] {stage:<20} {:>9.2?}", self.start.elapsed());
        }
    }
}

fn main() {
    let boot = Boot::new();

    Application::new().run(move |cx: &mut App| {
        boot.mark("runtime ready");

        let mono = theme::pick_mono_font(cx);
        cx.set_global(Theme::new(Appearance::Dark, mono));
        register_keymap(cx);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        boot.mark("theme + keymap");

        let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Zuno".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(720.), px(480.))),
                app_id: Some("dev.zuno.Zuno".to_string()),
                ..Default::default()
            },
            |window: &mut Window, cx| cx.new(|cx| Workspace::new(window, cx)),
        )
        .expect("failed to open the Zuno window");

        boot.mark("window open");
        cx.activate(true);
    });
}

/// The one place to look to answer "what does this key do".
///
/// Linux/Windows use `ctrl`; the macOS `cmd` variants get added alongside these
/// when there's a macOS build. Note that GPUI's own `examples/input.rs` — the
/// basis for M1.1's text input — ships `cmd-` bindings that never fire on Linux.
fn register_keymap(cx: &mut App) {
    cx.bind_keys([
        // Focus movement
        KeyBinding::new("ctrl-l", FocusUrl, None),
        KeyBinding::new("ctrl-b", FocusBody, None),
        KeyBinding::new("ctrl-shift-r", FocusResponse, None),
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrev, None),
        // Request lifecycle. The second binding is context-scoped: bare `enter`
        // sends only while the URL bar has focus, leaving `enter` free to mean
        // "newline" once the body editor accepts keystrokes in M1.1.
        KeyBinding::new("ctrl-enter", SendRequest, None),
        KeyBinding::new("enter", SendRequest, Some("UrlBar")),
        // Application
        KeyBinding::new("ctrl-shift-t", ToggleTheme, None),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
}
