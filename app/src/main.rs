//! Zuno — a native API client built around the feeling of Zed.
//!
//! Milestone 1.4: the full loop. Editable request including a multi-line body editor,
//! live HTTP, a virtualized response viewer, and diffing against the previous run.
//! See architecture.md §10.

#[macro_use]
mod timing;

mod actions;
mod body_view;
mod chrome;
mod collections;
mod commands;
mod engine;
mod input;
mod picker;
mod request_pane;
mod request_view;
mod response_pane;
mod session;
mod settings_panel;
#[cfg(test)]
mod tests;
mod theme;
mod ui;
mod workspace;

use std::time::Instant;

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions, Window, WindowBounds,
    WindowDecorations, WindowOptions, px, size,
};

use crate::actions::{
    AddFormField, AddHeader, AddMultipartField, AddQuery, CancelRequest, ChooseBodyFile, CloseTab,
    CopyResponse, FocusBody, FocusNext,
    FocusPrev, FocusResponse, FocusUrl, FoldAll, ImportCurl, NewTab, NextRequestTab, NextTab,
    OpenBodyType, PrevRequestTab,
    OpenMethod, OpenPalette, OpenRequest, OpenSettings, PickerConfirm, PickerDismiss, PickerNext,
    PickerPrev, PrevTab, Quit, RemoveRow, SaveRequest, SaveResponse, SendRequest, SettingConfirm,
    SettingDecrease, SettingIncrease, SettingNext, SettingPrev, SettingsDismiss, ShowHistory,
    SwitchEnvironment, ToggleResponseView, ToggleRow, ToggleTheme, UnfoldAll,
    CloseFind, CopyAsCurl, CopyRowPath, CopyRowValue, FindInResponse, FindNext, FindPrev,
    ResponseRowNext, ResponseRowPrev,
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

    // The asset source is what makes `svg()` able to load anything at all — without it every
    // icon renders as nothing, silently, because `paint_svg` swallows a miss with `log_err`.
    Application::new().with_assets(ui::Assets).run(move |cx: &mut App| {
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
        collections::install(cx);

        // Without this, closing the last window leaves the process running with nothing
        // on screen — GPUI does not quit on last-window-close by default. Quitting here
        // is also what makes `Workspace`'s `on_app_quit` save hook fire on that path.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

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
                // Explicit, because the default is `None` — which left the window
                // client-decorated with nothing drawing the decorations: no buttons and
                // no resize. `chrome.rs` draws both.
                window_decorations: Some(WindowDecorations::Client),
                app_id: Some("dev.zuno.Zuno".to_string()),
                ..Default::default()
            },
            |window: &mut Window, cx| {
                timing!("decorations          {:?}", window.window_decorations());
                cx.new(|cx| Workspace::new(window, cx))
            },
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
        // --- Buffers (global) ---
        //
        // `ctrl-tab` is a distinct keystroke from bare `tab` above, so tab-cycling focus
        // within a request and cycling between requests don't collide.
        KeyBinding::new("ctrl-t", NewTab, None),
        KeyBinding::new("ctrl-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        // --- Request editing (global) ---
        // Opens the method picker. Was cycling until M4; a dropdown replaces it, so
        // ctrl-shift-m is now free.
        KeyBinding::new("ctrl-m", OpenMethod, None),
        KeyBinding::new("ctrl-shift-h", AddHeader, None),
        KeyBinding::new("ctrl-shift-y", AddQuery, None),
        KeyBinding::new("alt-t", ToggleRow, None),
        KeyBinding::new("ctrl-shift-k", RemoveRow, None),
        // Opens the body-type picker. Was cycling `RawKind` only, which could never reach a
        // form body.
        KeyBinding::new("ctrl-shift-b", OpenBodyType, None),
        KeyBinding::new("ctrl-shift-f", AddFormField, None),
        KeyBinding::new("ctrl-shift-o", ChooseBodyFile, None),
        // Free since the method picker replaced CycleMethodBack.
        KeyBinding::new("ctrl-shift-m", AddMultipartField, None),
        // Paste-special: import a curl command from the clipboard.
        KeyBinding::new("ctrl-shift-v", ImportCurl, None),
        // Headers ⇄ Params ⇄ Body, cycling forward and back like `ctrl-tab` does for buffers.
        // Not `alt-tab`, which the compositor's window switcher takes before we ever see it;
        // `alt-q` sits beside `alt-r` for the response pane's equivalent.
        KeyBinding::new("alt-q", NextRequestTab, None),
        KeyBinding::new("alt-shift-q", PrevRequestTab, None),
        // --- Response viewer ---
        // Body ⇄ headers. `alt-` rather than `ctrl-`, to sit with the other two viewer
        // bindings; `alt-r` is free where `ctrl-shift-r` already focuses this pane.
        KeyBinding::new("alt-r", ToggleResponseView, None),
        KeyBinding::new("alt-f", FoldAll, None),
        KeyBinding::new("alt-e", UnfoldAll, None),
        // Moving a selection through the body. Scoped to the pane rather than global: `up` and
        // `down` are the editor's and the picker's too, and a context predicate matches only the
        // leaf, so the three cannot collide.
        KeyBinding::new("down", ResponseRowNext, Some("ResponsePane")),
        KeyBinding::new("up", ResponseRowPrev, Some("ResponsePane")),
        // `ctrl-c` finally means copy here. It is bound to `text_input::Copy` under
        // `TextInput`, and leaf-only matching keeps the two apart — which is why this can be
        // the obvious key while `CancelRequest` had to settle for `escape`.
        KeyBinding::new("ctrl-c", CopyRowValue, Some("ResponsePane")),
        KeyBinding::new("alt-c", CopyRowPath, Some("ResponsePane")),
        // Getting the response back out. `ctrl-c` is taken by text-input copy, scoped to
        // `TextInput`; these are global because the response pane has no input to type in.
        KeyBinding::new("ctrl-shift-c", CopyResponse, None),
        KeyBinding::new("ctrl-shift-s", SaveResponse, None),
        // Export, mirroring `ctrl-shift-v`'s import. `ctrl-shift-c` is already the response body,
        // so the request-as-curl gets its own key rather than overloading one.
        KeyBinding::new("ctrl-shift-x", CopyAsCurl, None),
        // --- Request lifecycle ---
        KeyBinding::new("ctrl-s", SaveRequest, None),
        KeyBinding::new("ctrl-enter", SendRequest, None),
        KeyBinding::new("enter", SendRequest, Some("UrlBar")),
        KeyBinding::new("escape", CancelRequest, None),
        // --- Application ---
        KeyBinding::new("ctrl-shift-t", ToggleTheme, None),
        KeyBinding::new("ctrl-q", Quit, None),
        // --- The picker ---
        //
        // ORDER MATTERS HERE, and not for the reason you'd guess. `Keymap::binding_enabled`
        // gives a context-less binding `depth = contexts.len()` — the *maximum* — so a
        // global binding does not lose to a leaf-context one, it **ties**. The tiebreak is
        // `ix_b.cmp(ix_a)`: later registration wins. So `escape` below only beats the
        // global `escape` -> CancelRequest because it is registered after it. Move this
        // block above the Application section and Esc stops closing the picker.
        KeyBinding::new("ctrl-p", OpenRequest, None),
        KeyBinding::new("ctrl-k", OpenPalette, None),
        KeyBinding::new("ctrl-e", SwitchEnvironment, None),
        KeyBinding::new("ctrl-h", ShowHistory, None),
        KeyBinding::new("down", PickerNext, Some("Picker")),
        KeyBinding::new("up", PickerPrev, Some("Picker")),
        KeyBinding::new("enter", PickerConfirm, Some("Picker")),
        KeyBinding::new("escape", PickerDismiss, Some("Picker")),
        // --- Find in the response ---
        //
        // Below the globals for the third time and the same reason: `escape` here has to be
        // registered after `escape` -> CancelRequest or it merely ties and loses, and closing
        // the find bar would cancel an in-flight request instead. `ctrl-f` is global so the bar
        // opens from anywhere; `enter` is scoped because it already means send in the URL bar
        // and newline in the body editor.
        KeyBinding::new("ctrl-f", FindInResponse, None),
        KeyBinding::new("enter", FindNext, Some("ResponseSearch")),
        KeyBinding::new("shift-enter", FindPrev, Some("ResponseSearch")),
        KeyBinding::new("escape", CloseFind, Some("ResponseSearch")),
        // --- The settings panel ---
        //
        // Registered after the globals for the same reason as the picker block above: a
        // context-less binding ties on depth, and later registration breaks the tie.
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("down", SettingNext, Some("SettingsPanel")),
        KeyBinding::new("up", SettingPrev, Some("SettingsPanel")),
        KeyBinding::new("right", SettingIncrease, Some("SettingsPanel")),
        KeyBinding::new("left", SettingDecrease, Some("SettingsPanel")),
        KeyBinding::new("enter", SettingConfirm, Some("SettingsPanel")),
        KeyBinding::new("escape", SettingsDismiss, Some("SettingsPanel")),
        // --- Text editing, scoped to any focused TextInput ---
        KeyBinding::new("backspace", text_input::Backspace, Some("TextInput")),
        KeyBinding::new("delete", text_input::Delete, Some("TextInput")),
        KeyBinding::new("left", text_input::Left, Some("TextInput")),
        KeyBinding::new("right", text_input::Right, Some("TextInput")),
        KeyBinding::new("shift-left", text_input::SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", text_input::SelectRight, Some("TextInput")),
        // Word-level movement, missing until an audit of the hand-rolled editor. Scoped to
        // `TextInput`, whose identifier the body editor's leaf context also carries, so one
        // binding serves the URL bar, every table cell, the find bar, and the body editor.
        KeyBinding::new("ctrl-left", text_input::WordLeft, Some("TextInput")),
        KeyBinding::new("ctrl-right", text_input::WordRight, Some("TextInput")),
        KeyBinding::new("ctrl-shift-left", text_input::SelectWordLeft, Some("TextInput")),
        KeyBinding::new("ctrl-shift-right", text_input::SelectWordRight, Some("TextInput")),
        // Word deletion, reusing the same boundaries as the movement above.
        KeyBinding::new("ctrl-backspace", text_input::DeleteWordLeft, Some("TextInput")),
        KeyBinding::new("ctrl-delete", text_input::DeleteWordRight, Some("TextInput")),
        // Document ends. In a single-line input these are Home/End again; in the body editor
        // they are the difference between the line and the document.
        KeyBinding::new("ctrl-home", text_input::DocStart, Some("TextInput")),
        KeyBinding::new("ctrl-end", text_input::DocEnd, Some("TextInput")),
        KeyBinding::new("ctrl-shift-home", text_input::SelectDocStart, Some("TextInput")),
        KeyBinding::new("ctrl-shift-end", text_input::SelectDocEnd, Some("TextInput")),
        // Undo/redo, per text surface — each entity keeps its own history, so undoing in the
        // URL bar cannot reach into the body. Both redo spellings, since Linux ships both.
        KeyBinding::new("ctrl-z", text_input::Undo, Some("TextInput")),
        KeyBinding::new("ctrl-shift-z", text_input::Redo, Some("TextInput")),
        KeyBinding::new("ctrl-y", text_input::Redo, Some("TextInput")),
        // Paging is the editor's alone: a single-line input has no page to move by.
        KeyBinding::new("pageup", editor::PageUp, Some("BodyEditor")),
        KeyBinding::new("pagedown", editor::PageDown, Some("BodyEditor")),
        KeyBinding::new("shift-pageup", editor::SelectPageUp, Some("BodyEditor")),
        KeyBinding::new("shift-pagedown", editor::SelectPageDown, Some("BodyEditor")),
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
