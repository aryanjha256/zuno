//! Zuno — a native API client built around the feeling of Zed.
//!
//! Milestone 1.4: the full loop. Editable request including a multi-line body editor,
//! live HTTP, a virtualized response viewer, and diffing against the previous run.
//! See architecture.md §10.

#[macro_use]
mod timing;

mod actions;
mod body_view;
mod engine;
mod input;
mod request_pane;
mod request_view;
mod response_pane;
mod session;
#[cfg(test)]
mod tests;
mod theme;
mod workspace;

use std::time::Instant;

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions, Window, WindowBounds,
    WindowOptions, px, size,
};

use crate::actions::{
    AddHeader, AddQuery, CancelRequest, CycleMethod, CycleMethodBack, FocusBody, FocusNext,
    CycleBodyKind, FocusPrev, FocusResponse, FocusUrl, FoldAll, Quit, RemoveRow, SendRequest,
    ToggleRow,
    ToggleTheme, UnfoldAll,
};
use crate::input::{editor, text_input};
use crate::theme::{Appearance, Theme};
use crate::workspace::Workspace;

/// Startup stage timings, printed when `ZUNO_TIMING=1`.
///
/// Measured per stage rather than end-to-end, which is what revealed that ~120ms of
/// startup is GPUI platform init before any Zuno code runs — see architecture.md §8.
struct Boot {
    start: Instant,
}

impl Boot {
    fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    fn mark(&self, stage: &str) {
        timing!("{stage:<20} {:>9.2?}", self.start.elapsed());
    }
}

fn main() {
    let boot = Boot::new();

    Application::new().run(move |cx: &mut App| {
        boot.mark("runtime ready");

        let mono = theme::pick_mono_font(cx);
        cx.set_global(Theme::new(Appearance::Dark, mono));
        register_keymap(cx);
        boot.mark("theme + keymap");

        // A failure here is reported inline on the first send rather than blocking
        // startup — an API client that won't open because a thread failed to spawn is
        // worse than one that opens and explains itself.
        if let Err(error) = engine::install(cx) {
            eprintln!("[zuno] could not start the HTTP engine: {error}");
        }
        session::install(cx);
        boot.mark("engine + session");

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
/// Two things worth knowing about the contexts here:
///
/// - GPUI's `Identifier` predicate matches only the *leaf* key context, so the
///   editing bindings below reach a `TextInput` because its own context string
///   carries both identifiers (`"TextInput UrlBar"`), not because of nesting.
/// - Bare `enter` sends under `UrlBar` and inserts a newline under `BodyEditor`. Two
///   bindings for the same key, disambiguated purely by context — the reason contexts
///   had to be set up in M1.0 rather than retrofitted.
///
/// Linux/Windows use `ctrl`; the macOS `cmd` variants get added alongside these when
/// there's a macOS build. GPUI's own `examples/input.rs` — the basis for the text
/// input — ships `cmd-` bindings that never fire on Linux, so every one of them is
/// translated to `ctrl-` below.
fn register_keymap(cx: &mut App) {
    cx.bind_keys([
        // --- Focus movement (global) ---
        KeyBinding::new("ctrl-l", FocusUrl, None),
        KeyBinding::new("ctrl-b", FocusBody, None),
        KeyBinding::new("ctrl-shift-r", FocusResponse, None),
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrev, None),
        // --- Request editing (global) ---
        KeyBinding::new("ctrl-m", CycleMethod, None),
        KeyBinding::new("ctrl-shift-m", CycleMethodBack, None),
        KeyBinding::new("ctrl-shift-h", AddHeader, None),
        KeyBinding::new("ctrl-shift-y", AddQuery, None),
        KeyBinding::new("alt-t", ToggleRow, None),
        KeyBinding::new("ctrl-shift-k", RemoveRow, None),
        KeyBinding::new("ctrl-shift-b", CycleBodyKind, None),
        // --- Response viewer ---
        KeyBinding::new("alt-f", FoldAll, None),
        KeyBinding::new("alt-e", UnfoldAll, None),
        // --- Request lifecycle ---
        KeyBinding::new("ctrl-enter", SendRequest, None),
        KeyBinding::new("enter", SendRequest, Some("UrlBar")),
        KeyBinding::new("escape", CancelRequest, None),
        // --- Application ---
        KeyBinding::new("ctrl-shift-t", ToggleTheme, None),
        KeyBinding::new("ctrl-q", Quit, None),
        // --- Text editing, scoped to any focused TextInput ---
        KeyBinding::new("backspace", text_input::Backspace, Some("TextInput")),
        KeyBinding::new("delete", text_input::Delete, Some("TextInput")),
        KeyBinding::new("left", text_input::Left, Some("TextInput")),
        KeyBinding::new("right", text_input::Right, Some("TextInput")),
        KeyBinding::new("shift-left", text_input::SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", text_input::SelectRight, Some("TextInput")),
        KeyBinding::new("home", text_input::Home, Some("TextInput")),
        KeyBinding::new("end", text_input::End, Some("TextInput")),
        KeyBinding::new("shift-home", text_input::SelectHome, Some("TextInput")),
        KeyBinding::new("shift-end", text_input::SelectEnd, Some("TextInput")),
        KeyBinding::new("ctrl-a", text_input::SelectAll, Some("TextInput")),
        KeyBinding::new("ctrl-c", text_input::Copy, Some("TextInput")),
        KeyBinding::new("ctrl-v", text_input::Paste, Some("TextInput")),
        KeyBinding::new("ctrl-x", text_input::Cut, Some("TextInput")),
        // --- Line-aware editing, only inside the multi-line body editor ---
        KeyBinding::new("up", editor::Up, Some("BodyEditor")),
        KeyBinding::new("down", editor::Down, Some("BodyEditor")),
        KeyBinding::new("shift-up", editor::SelectUp, Some("BodyEditor")),
        KeyBinding::new("shift-down", editor::SelectDown, Some("BodyEditor")),
        KeyBinding::new("enter", editor::Newline, Some("BodyEditor")),
    ]);
}
