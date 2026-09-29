//! Zuno — a native API client built around the feeling of Zed.
//!
//! Milestone 1.4: the full loop. Editable request including a multi-line body editor,
//! live HTTP, a virtualized response viewer, and diffing against the previous run.
//! See architecture.md §10.

// A GUI program on Windows, or it opens a console window beside itself on every launch. Release
// only, so a debug build keeps its console for `ZUNO_TIMING` and panics.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[macro_use]
mod timing;

mod actions;
mod body_view;
mod cert_panel;
mod chrome;
mod cookie_panel;
mod app_state;
mod close_panel;
mod collection_panel;
mod environment_panel;
mod flow_panel;
mod import_panel;
mod collections;
mod commands;
mod context_menu;
mod engine;
mod kinds;
mod input;
mod paths;
mod picker;
mod platform_keys;
mod run_panel;
mod request_pane;
mod request_view;
mod response_pane;
mod session;
mod settings_panel;
#[cfg(test)]
mod tests;
mod theme;
mod ui;
mod update;
mod workspace;
mod workspace_panel;

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
    OpenAppMenu, OpenMethod, OpenPalette, OpenRequest, OpenSettings, PickerConfirm, PickerDismiss, PickerNext,
    PickerPrev, PrevTab, Quit, RemoveRow, SaveRequest, SaveResponse, SendRequest, SettingConfirm,
    SuggestConfirm, SuggestDismiss, SuggestNext, SuggestPrev, CompleteNext, CompletePrev,
    CompleteAccept, CompleteConfirm, CompleteDismiss, TriggerCompletion,
    SettingDecrease, SettingIncrease, SettingNext, SettingPrev, SettingsDismiss, ShowHistory,
    CertsConfirm, CertsDismiss, CertsNext, CertsPrev, CertsRemove,
    ClearCookies, CookiesDismiss, CookiesNext, CookiesPrev, CookiesRemove,
    NextResponseTab, PrevResponseTab, SwitchEnvironment, ToggleRow, ToggleTheme, UnfoldAll,
    BodyFindNext, BodyFindPrev, CloseBodyFind, CloseFind, CopyAsCode, CopyRowPath, CopyRowValue,
    FindInBody, FindInResponse, FindNext, FindPrev, ReplaceAll, ReplaceNext,
    MenuConfirm, MenuDismiss, MenuNext, MenuPrev, ResponseRowNext, ResponseRowPrev, ScrollLeft,
    ScrollRight, ScrollStart, ToggleFold,
    CollectionCollapse, CollectionConfirm, CollectionExpand, CollectionNext, CollectionPrev,
    CancelClose, CancelRename, CloseChoiceNext, CloseChoicePrev, CommitRename, ConfirmClose,
    WorkspaceConfirm, WorkspaceDismiss,
    AssertValue, CaptureValue, EditEnvironments, EnvConfirm, EnvDismiss, EnvNext, EnvPrev,
    FlowConfirm, FlowDismiss, FlowNext, FlowPrev, FlowStepDown, FlowStepNext, FlowStepPrev,
    FlowStepRemove, FlowStepUp, OpenDefaults,
    RunDismiss, RunFlow, RunFolder,
    DeleteRequest, FormatBody, ImportConfirm, ImportDismiss, ImportDocument, MinifyBody,
    NewFolder, NewRequest, RenameRequest, ToggleCollectionPanel,
};
use crate::input::{editor, text_input};
use crate::theme::Theme;
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

        // Before the theme: `app.json` is where the chosen appearance lives, and it also
        // resolves the active workspace into the collection-root and session-file globals that
        // everything downstream reads.
        app_state::install(cx);

        let mono = theme::pick_mono_font(cx);
        cx.set_global(Theme::new(app_state::theme(cx), mono));
        register_keymap(cx);
        boot.mark("theme + keymap");

        // A failure here is reported inline on the first send rather than blocking
        // startup — an API client that won't open because a thread failed to spawn is
        // worse than one that opens and explains itself.
        if let Err(error) = engine::install(cx) {
            eprintln!("[zuno] could not start the HTTP engine: {error}");
        }
        // The saved proxy has to be handed over *after* the engine exists: it lives in
        // `app.json` while the clients that honour it are built on the engine thread, and a
        // setting the engine never heard about is a status bar naming a proxy nothing uses.
        if let Some(engine) = crate::engine::ActiveEngine::engine(cx as &gpui::App) {
            engine.set_proxy(app_state::proxy(cx));
            engine.set_tls(app_state::tls(cx));
            // Same reasoning, and the same ordering problem: `collections::install_at` ran
            // before the engine existed, so the root it set never reached it.
            engine.set_collection(
                crate::collections::root(cx as &gpui::App).map(std::path::Path::to_path_buf),
            );
        }
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
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // On macOS the system's traffic lights sit over our own titlebar, which the
                // content view extends under; elsewhere both fields are ignored.
                titlebar: Some(TitlebarOptions {
                    title: Some("Zuno".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(crate::chrome::TRAFFIC_LIGHTS),
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

        // Started here rather than inside `Workspace::new`, and that placement is the whole
        // isolation: the test harness builds a `Workspace` directly, so a check wired into the
        // constructor would put a real HTTPS request to GitHub in front of every test in the
        // suite. `main` is the one path a person takes and no test does.
        let _ = window.update(cx, |workspace, _window, cx| workspace.check_for_update(cx));
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
    cx.bind_keys(bindings());
}

/// This platform's keymap.
fn bindings() -> Vec<KeyBinding> {
    bindings_for(cfg!(target_os = "macos"))
}

/// One binding, with its keystrokes translated for the platform — see `platform_keys`. A macro
/// rather than a function because each binding's action is a different type.
macro_rules! bind {
    ($mac:expr, $keys:expr, $($rest:tt)*) => {
        KeyBinding::new(&crate::platform_keys::for_platform($keys, $mac), $($rest)*)
    };
}

/// Every binding, as a list rather than passed straight to `bind_keys`.
///
/// **So a test can read it.** A context-less binding registered twice for one keystroke does not
/// fail to compile and does not fail loudly: `binding_enabled` scores both at maximum depth, the
/// tiebreak is registration order, and the later one silently wins. `ctrl-shift-r` was taken from
/// `FocusResponse` that way, and what noticed was two unrelated response tests going red.
///
/// **For a platform, not only this one**, so the Mac keymap can be checked for exactly that
/// collision from a Linux machine — the translation moves keys onto each other, and `⌘⇧M` landing
/// on two actions was the first thing it did.
fn bindings_for(mac: bool) -> Vec<KeyBinding> {
    vec![
        // --- Focus movement (global) ---
        bind!(mac, "ctrl-l", FocusUrl, None),
        bind!(mac, "ctrl-b", FocusBody, None),
        bind!(mac, "ctrl-shift-r", FocusResponse, None),
        bind!(mac, "tab", FocusNext, None),
        bind!(mac, "shift-tab", FocusPrev, None),
        // --- The collection panel ---
        //
        // `ctrl-shift-e` is VS Code's explorer binding, which is the closest existing
        // convention for "show me the tree of what I have". `ctrl-b` would be the other
        // candidate and is already `FocusBody`.
        bind!(mac, "ctrl-shift-e", ToggleCollectionPanel, None),
        // Scoped to the panel's own leaf context. `up`/`down` mean something different in
        // every pane, which is exactly what a context predicate is for.
        bind!(mac, "down", CollectionNext, Some("CollectionPanel")),
        bind!(mac, "up", CollectionPrev, Some("CollectionPanel")),
        bind!(mac, "enter", CollectionConfirm, Some("CollectionPanel")),
        // The file-tree convention: left closes a directory or steps out to its parent,
        // right opens one. Both are no-ops on a request row rather than errors.
        bind!(mac, "left", CollectionCollapse, Some("CollectionPanel")),
        bind!(mac, "right", CollectionExpand, Some("CollectionPanel")),
        // Keyboard-first is not keyboard-only, and the reverse holds too: right-click is the
        // discoverable path, and this is the one that keeps the panel usable without a mouse.
        // It only *asks* — `ConfirmDeleteRequest` is what removes anything, and it has no
        // binding at all, because a destructive verb one keystroke away is the thing the
        // confirmation exists to prevent.
        bind!(mac, "delete", DeleteRequest, Some("CollectionPanel")),
        // The desktop convention for rename, in every file manager and in VS Code.
        bind!(mac, "f2", RenameRequest, Some("CollectionPanel")),
        // The file-manager convention for a new folder, and free here: `ctrl-shift-n` is
        // otherwise unused, and `ctrl-n` is not bound at all.
        bind!(mac, "ctrl-shift-n", NewFolder, Some("CollectionPanel")),
        bind!(mac, "ctrl-n", NewRequest, Some("CollectionPanel")),
        // --- Buffers (global) ---
        //
        // `ctrl-tab` is a distinct keystroke from bare `tab` above, so tab-cycling focus
        // within a request and cycling between requests don't collide.
        bind!(mac, "ctrl-t", NewTab, None),
        bind!(mac, "ctrl-w", CloseTab, None),
        bind!(mac, "ctrl-tab", NextTab, None),
        bind!(mac, "ctrl-shift-tab", PrevTab, None),
        // --- Request editing (global) ---
        // Opens the method picker. Was cycling until M4; a dropdown replaces it, so
        // ctrl-shift-m is now free.
        bind!(mac, "ctrl-m", OpenMethod, None),
        bind!(mac, "ctrl-shift-h", AddHeader, None),
        bind!(mac, "ctrl-shift-y", AddQuery, None),
        bind!(mac, "alt-t", ToggleRow, None),
        bind!(mac, "ctrl-shift-k", RemoveRow, None),
        // Opens the body-type picker. Was cycling `RawKind` only, which could never reach a
        // form body.
        bind!(mac, "ctrl-shift-b", OpenBodyType, None),
        bind!(mac, "ctrl-shift-f", AddFormField, None),
        bind!(mac, "ctrl-shift-o", ChooseBodyFile, None),
        // Free since the method picker replaced CycleMethodBack.
        bind!(mac, "ctrl-shift-m", AddMultipartField, None),
        // Paste-special: import a curl command from the clipboard.
        bind!(mac, "ctrl-shift-v", ImportCurl, None),
        bind!(mac, "ctrl-shift-i", ImportDocument, None),
        // Headers ⇄ Params ⇄ Body, cycling forward and back like `ctrl-tab` does for buffers.
        // Not `alt-tab`, which the compositor's window switcher takes before we ever see it;
        // `alt-q` sits beside `alt-r` for the response pane's equivalent.
        bind!(mac, "alt-q", NextRequestTab, None),
        bind!(mac, "alt-shift-q", PrevRequestTab, None),
        // --- Response viewer ---
        // Body ⇄ headers. `alt-` rather than `ctrl-`, to sit with the other two viewer
        // bindings; `alt-r` is free where `ctrl-shift-r` already focuses this pane.
        // `alt-r`/`alt-shift-r` mirrors the request pane's `alt-q`/`alt-shift-q`. The five
        // `ShowResponse*` verbs are deliberately **unbound**, exactly as the request pane's
        // `Show*Tab` verbs are: they exist for the palette and for the tabs' own clicks, and
        // five more keystrokes would be five more chances at the clash §6e records.
        bind!(mac, "alt-r", NextResponseTab, None),
        bind!(mac, "alt-shift-r", PrevResponseTab, None),
        bind!(mac, "alt-f", FoldAll, None),
        bind!(mac, "alt-shift-f", FormatBody, None),
        bind!(mac, "alt-shift-m", MinifyBody, None),
        bind!(mac, "alt-e", UnfoldAll, None),
        // Moving a selection through the body. Scoped to the pane rather than global: `up` and
        // `down` are the editor's and the picker's too, and a context predicate matches only the
        // leaf, so the three cannot collide.
        bind!(mac, "down", ResponseRowNext, Some("ResponsePane")),
        bind!(mac, "up", ResponseRowPrev, Some("ResponsePane")),
        // `ctrl-c` finally means copy here. It is bound to `text_input::Copy` under
        // `TextInput`, and leaf-only matching keeps the two apart — which is why this can be
        // the obvious key while `CancelRequest` had to settle for `escape`.
        bind!(mac, "ctrl-c", CopyRowValue, Some("ResponsePane")),
        bind!(mac, "alt-c", CopyRowPath, Some("ResponsePane")),
        // Beside `alt-c`, because capturing *is* copying the path — into a rule rather than
        // onto the clipboard. Scoped to the pane the row lives in, like both of its neighbours.
        bind!(mac, "alt-shift-c", CaptureValue, Some("ResponsePane")),
        // Beside its capture twin. Same gesture, same source of the path — one puts the value
        // somewhere, the other checks it.
        bind!(mac, "alt-shift-a", AssertValue, Some("ResponsePane")),
        bind!(mac, "space", ToggleFold, Some("ResponsePane")),
        // Horizontal scrolling. `up`/`down` already move the row selection in this context, so
        // `left`/`right` moving the view across is the completion of that idiom rather than a
        // new one. Both are unbound here today; `home` is `text_input::Home` under a different
        // leaf context, so the two cannot collide.
        bind!(mac, "left", ScrollLeft, Some("ResponsePane")),
        bind!(mac, "right", ScrollRight, Some("ResponsePane")),
        bind!(mac, "home", ScrollStart, Some("ResponsePane")),
        // Getting the response back out. `ctrl-c` is taken by text-input copy, scoped to
        // `TextInput`; these are global because the response pane has no input to type in.
        bind!(mac, "ctrl-shift-c", CopyResponse, None),
        bind!(mac, "ctrl-shift-s", SaveResponse, None),
        // Export, mirroring `ctrl-shift-v`'s import. `ctrl-shift-c` is already the response body,
        // so the request gets its own key rather than overloading one. It opens the language
        // picker, curl first, so the old muscle memory is one `enter` longer.
        bind!(mac, "ctrl-shift-x", CopyAsCode, None),
        // --- Request lifecycle ---
        bind!(mac, "ctrl-s", SaveRequest, None),
        bind!(mac, "ctrl-enter", SendRequest, None),
        bind!(mac, "enter", SendRequest, Some("UrlBar")),
        bind!(mac, "escape", CancelRequest, None),
        // --- Application ---
        bind!(mac, "ctrl-shift-t", ToggleTheme, None),
        bind!(mac, "ctrl-q", Quit, None),
        // --- The picker ---
        //
        // ORDER MATTERS HERE, and not for the reason you'd guess. `Keymap::binding_enabled`
        // gives a context-less binding `depth = contexts.len()` — the *maximum* — so a
        // global binding does not lose to a leaf-context one, it **ties**. The tiebreak is
        // `ix_b.cmp(ix_a)`: later registration wins. So `escape` below only beats the
        // global `escape` -> CancelRequest because it is registered after it. Move this
        // block above the Application section and Esc stops closing the picker.
        bind!(mac, "ctrl-p", OpenRequest, None),
        bind!(mac, "ctrl-k", OpenPalette, None),
        // F10 is the desktop convention for "open this window's menu", so the menu has a
        // keystroke to advertise rather than being mouse-only.
        bind!(mac, "f10", OpenAppMenu, None),
        bind!(mac, "ctrl-e", SwitchEnvironment, None),
        // `ctrl-e` selects an environment, `ctrl-shift-e` is the collection panel, so the
        // editor takes the next free chord in the same family.
        bind!(mac, "ctrl-alt-e", EditEnvironments, None),
        bind!(mac, "ctrl-r", RunFolder, None),
        // `ctrl-r` is the folder in front of you; `ctrl-alt-r` is a flow you authored.
        //
        // **Not `ctrl-shift-r`**, which `FocusResponse` has held since M1 — and taking it did
        // not fail to compile or even fail loudly. A context-less binding registered later
        // simply wins the tie, so focusing the response pane silently started running a flow,
        // and two unrelated response tests were what noticed.
        bind!(mac, "ctrl-alt-r", RunFlow, None),
        bind!(mac, "ctrl-h", ShowHistory, None),
        bind!(mac, "down", PickerNext, Some("Picker")),
        bind!(mac, "up", PickerPrev, Some("Picker")),
        bind!(mac, "enter", PickerConfirm, Some("Picker")),
        bind!(mac, "escape", PickerDismiss, Some("Picker")),
        // --- Find in the response ---
        //
        // Below the globals for the third time and the same reason: `escape` here has to be
        // registered after `escape` -> CancelRequest or it merely ties and loses, and closing
        // the find bar would cancel an in-flight request instead. `ctrl-f` is global so the bar
        // opens from anywhere; `enter` is scoped because it already means send in the URL bar
        // and newline in the body editor.
        bind!(mac, "ctrl-f", FindInResponse, None),
        // --- Find and replace in the request body ---
        // `ctrl-f` means "find in what I am looking at", which in the body editor is the body.
        // The same shape as bare `enter` sending in the URL bar and inserting a newline here:
        // one key, disambiguated by leaf context. Registered after the global one, so the
        // ordering rule is satisfied whichever way the tie-break falls.
        bind!(mac, "ctrl-f", FindInBody, Some("BodyEditor")),
        bind!(mac, "enter", BodyFindNext, Some("BodySearch")),
        bind!(mac, "shift-enter", BodyFindPrev, Some("BodySearch")),
        bind!(mac, "escape", CloseBodyFind, Some("BodySearch")),
        bind!(mac, "ctrl-enter", ReplaceNext, Some("BodySearch")),
        bind!(mac, "ctrl-alt-enter", ReplaceAll, Some("BodySearch")),
        bind!(mac, "enter", FindNext, Some("ResponseSearch")),
        bind!(mac, "shift-enter", FindPrev, Some("ResponseSearch")),
        bind!(mac, "escape", CloseFind, Some("ResponseSearch")),
        // --- The settings panel ---
        //
        // Registered after the globals for the same reason as the picker block above: a
        // context-less binding ties on depth, and later registration breaks the tie.
        // --- Context menu ---
        // Registered after the global `escape` -> CancelRequest. A context-less binding does not
        // lose to a specific one, it *ties* at maximum depth and the later registration wins —
        // the same ordering the find bar depends on.
        bind!(mac, "down", MenuNext, Some("ContextMenu")),
        bind!(mac, "up", MenuPrev, Some("ContextMenu")),
        bind!(mac, "enter", MenuConfirm, Some("ContextMenu")),
        bind!(mac, "escape", MenuDismiss, Some("ContextMenu")),
        bind!(mac, "ctrl-,", OpenSettings, None),
        bind!(mac, "ctrl-shift-,", OpenDefaults, None),
        bind!(mac, "down", SettingNext, Some("SettingsPanel")),
        bind!(mac, "up", SettingPrev, Some("SettingsPanel")),
        bind!(mac, "right", SettingIncrease, Some("SettingsPanel")),
        bind!(mac, "left", SettingDecrease, Some("SettingsPanel")),
        bind!(mac, "enter", SettingConfirm, Some("SettingsPanel")),
        bind!(mac, "escape", SettingsDismiss, Some("SettingsPanel")),
        // **After the global `escape`, and that is not stylistic.** A leaf-matching predicate
        // scores the same depth as a context-less binding, so the tie falls through to "later
        // registration wins". Registered above `escape` -> CancelRequest, renaming could not be
        // cancelled and nothing would fail to compile. Sixth time this ordering has decided
        // behaviour. The input's own leaf context is `"TextInput CollectionRename"`, which is
        // why `CollectionRename` matches without any nesting.
        bind!(mac, "enter", CommitRename, Some("CollectionRename")),
        bind!(mac, "escape", CancelRename, Some("CollectionRename")),
        // After the global `escape` for the reason above. The input's leaf context is
        // `"TextInput ImportSource"`, so `ImportSource` is what matches — the panel's own
        // `"ImportPanel"` context never holds focus, since the field does.
        bind!(mac, "enter", ImportConfirm, Some("ImportSource")),
        bind!(mac, "escape", ImportDismiss, Some("ImportSource")),
        // The unsaved-changes prompt. After the global `escape` and `enter` for the reason
        // above — a leaf-matching predicate only *ties* with a context-less one, and the tie
        // goes to whichever was registered later. `left`/`right` move between the buttons and
        // `tab` does too, the dialog convention; `FocusNext` already refuses while a modal is
        // open, so `tab` would otherwise be dead here rather than merely unbound.
        // The new-workspace dialog. Its two fields carry leaf contexts `WorkspaceName` and
        // `WorkspaceLocation`, so both are bound — the panel's own `WorkspacePanel` context never
        // holds focus, since an input always does. After the global twins, for the usual reason.
        bind!(mac, "enter", WorkspaceConfirm, Some("WorkspaceName")),
        bind!(mac, "enter", WorkspaceConfirm, Some("WorkspaceLocation")),
        bind!(mac, "escape", WorkspaceDismiss, Some("WorkspaceName")),
        bind!(mac, "escape", WorkspaceDismiss, Some("WorkspaceLocation")),
        // The environment editor. Three leaf contexts hold focus inside it — the panel's own
        // handle when nothing is being typed, `EnvField` for a variable's two boxes, and
        // `EnvRename` for the name box — and a leaf predicate matches only the last context, so
        // each one is bound separately. All after the global `escape`, for the usual reason: a
        // leaf match merely *ties* with a context-less binding and the later registration wins.
        bind!(mac, "escape", EnvDismiss, Some("EnvPanel")),
        bind!(mac, "escape", EnvDismiss, Some("EnvField")),
        bind!(mac, "escape", EnvDismiss, Some("EnvRename")),
        bind!(mac, "enter", EnvConfirm, Some("EnvPanel")),
        bind!(mac, "enter", EnvConfirm, Some("EnvField")),
        bind!(mac, "enter", EnvConfirm, Some("EnvRename")),
        bind!(mac, "alt-down", EnvNext, Some("EnvPanel")),
        bind!(mac, "alt-up", EnvPrev, Some("EnvPanel")),
        bind!(mac, "alt-down", EnvNext, Some("EnvField")),
        bind!(mac, "alt-up", EnvPrev, Some("EnvField")),
        // After the global `escape`, for the reason every other modal's is.
        bind!(mac, "escape", RunDismiss, Some("RunPanel")),
        // The flow editor. Three leaf contexts hold focus in it — the panel, and the two name
        // boxes — and all after the global twins, for the reason every other modal's are.
        bind!(mac, "escape", FlowDismiss, Some("FlowPanel")),
        bind!(mac, "escape", FlowDismiss, Some("FlowName")),
        bind!(mac, "enter", FlowConfirm, Some("FlowPanel")),
        bind!(mac, "enter", FlowConfirm, Some("FlowName")),
        // `up`/`down` move the cursor; `alt-` moves the step it is on. Two verbs, one axis,
        // and the modifier is what separates "which step" from "where it goes".
        bind!(mac, "down", FlowStepNext, Some("FlowPanel")),
        bind!(mac, "up", FlowStepPrev, Some("FlowPanel")),
        bind!(mac, "ctrl-down", FlowNext, Some("FlowPanel")),
        bind!(mac, "ctrl-up", FlowPrev, Some("FlowPanel")),
        bind!(mac, "alt-up", FlowStepUp, Some("FlowPanel")),
        bind!(mac, "alt-down", FlowStepDown, Some("FlowPanel")),
        bind!(mac, "delete", FlowStepRemove, Some("FlowPanel")),
        bind!(mac, "enter", CertsConfirm, Some("CertPanel")),
        bind!(mac, "escape", CertsDismiss, Some("CertPanel")),
        bind!(mac, "down", CertsNext, Some("CertPanel")),
        bind!(mac, "up", CertsPrev, Some("CertPanel")),
        bind!(mac, "delete", CertsRemove, Some("CertPanel")),
        // After the global `escape`, for the tie-break the header-name dropdown's note below
        // explains. `shift-delete` is the whole jar, one modifier away from the one cookie.
        bind!(mac, "escape", CookiesDismiss, Some("CookiePanel")),
        bind!(mac, "down", CookiesNext, Some("CookiePanel")),
        bind!(mac, "up", CookiesPrev, Some("CookiePanel")),
        bind!(mac, "delete", CookiesRemove, Some("CookiePanel")),
        bind!(mac, "shift-delete", ClearCookies, Some("CookiePanel")),
        // The header-name dropdown. Scoped to `HeaderCell`, and **after** the global `escape`
        // above — a leaf-matching predicate ties with a context-less one and the tie goes to
        // later registration, which is what makes these win while a header name has focus.
        // That is also why `suggest_dismiss` forwards to cancel when no list is open: this
        // binding wins unconditionally, so without the fallback, putting the cursor in a header
        // cell would quietly disarm cancelling a request.
        bind!(mac, "down", SuggestNext, Some("HeaderCell")),
        bind!(mac, "up", SuggestPrev, Some("HeaderCell")),
        bind!(mac, "enter", SuggestConfirm, Some("HeaderCell")),
        bind!(mac, "escape", SuggestDismiss, Some("HeaderCell")),
        bind!(mac, "enter", ConfirmClose, Some("CloseConfirm")),
        bind!(mac, "escape", CancelClose, Some("CloseConfirm")),
        bind!(mac, "right", CloseChoiceNext, Some("CloseConfirm")),
        bind!(mac, "left", CloseChoicePrev, Some("CloseConfirm")),
        bind!(mac, "tab", CloseChoiceNext, Some("CloseConfirm")),
        bind!(mac, "shift-tab", CloseChoicePrev, Some("CloseConfirm")),
        // --- Text editing, scoped to any focused TextInput ---
        bind!(mac, "backspace", text_input::Backspace, Some("TextInput")),
        bind!(mac, "delete", text_input::Delete, Some("TextInput")),
        bind!(mac, "left", text_input::Left, Some("TextInput")),
        bind!(mac, "right", text_input::Right, Some("TextInput")),
        bind!(mac, "shift-left", text_input::SelectLeft, Some("TextInput")),
        bind!(mac, "shift-right", text_input::SelectRight, Some("TextInput")),
        // Word-level movement, missing until an audit of the hand-rolled editor. Scoped to
        // `TextInput`, whose identifier the body editor's leaf context also carries, so one
        // binding serves the URL bar, every table cell, the find bar, and the body editor.
        bind!(mac, "ctrl-left", text_input::WordLeft, Some("TextInput")),
        bind!(mac, "ctrl-right", text_input::WordRight, Some("TextInput")),
        bind!(mac, "ctrl-shift-left", text_input::SelectWordLeft, Some("TextInput")),
        bind!(mac, "ctrl-shift-right", text_input::SelectWordRight, Some("TextInput")),
        // Word deletion, reusing the same boundaries as the movement above.
        bind!(mac, "ctrl-backspace", text_input::DeleteWordLeft, Some("TextInput")),
        bind!(mac, "ctrl-delete", text_input::DeleteWordRight, Some("TextInput")),
        // Document ends. In a single-line input these are Home/End again; in the body editor
        // they are the difference between the line and the document.
        bind!(mac, "ctrl-home", text_input::DocStart, Some("TextInput")),
        bind!(mac, "ctrl-end", text_input::DocEnd, Some("TextInput")),
        bind!(mac, "ctrl-shift-home", text_input::SelectDocStart, Some("TextInput")),
        bind!(mac, "ctrl-shift-end", text_input::SelectDocEnd, Some("TextInput")),
        // Undo/redo, per text surface — each entity keeps its own history, so undoing in the
        // URL bar cannot reach into the body. Both redo spellings, since Linux ships both.
        bind!(mac, "ctrl-z", text_input::Undo, Some("TextInput")),
        bind!(mac, "ctrl-shift-z", text_input::Redo, Some("TextInput")),
        bind!(mac, "ctrl-y", text_input::Redo, Some("TextInput")),
        // Paging is the editor's alone: a single-line input has no page to move by.
        bind!(mac, "pageup", editor::PageUp, Some("BodyEditor")),
        bind!(mac, "pagedown", editor::PageDown, Some("BodyEditor")),
        bind!(mac, "shift-pageup", editor::SelectPageUp, Some("BodyEditor")),
        bind!(mac, "shift-pagedown", editor::SelectPageDown, Some("BodyEditor")),
        bind!(mac, "home", text_input::Home, Some("TextInput")),
        bind!(mac, "end", text_input::End, Some("TextInput")),
        bind!(mac, "shift-home", text_input::SelectHome, Some("TextInput")),
        bind!(mac, "shift-end", text_input::SelectEnd, Some("TextInput")),
        bind!(mac, "ctrl-a", text_input::SelectAll, Some("TextInput")),
        bind!(mac, "ctrl-c", text_input::Copy, Some("TextInput")),
        bind!(mac, "ctrl-v", text_input::Paste, Some("TextInput")),
        bind!(mac, "ctrl-x", text_input::Cut, Some("TextInput")),
        // --- Line-aware editing, only inside the multi-line body editor ---
        bind!(mac, "up", editor::Up, Some("BodyEditor")),
        bind!(mac, "down", editor::Down, Some("BodyEditor")),
        bind!(mac, "shift-up", editor::SelectUp, Some("BodyEditor")),
        bind!(mac, "shift-down", editor::SelectDown, Some("BodyEditor")),
        bind!(mac, "enter", editor::Newline, Some("BodyEditor")),
        // GraphQL completion. **Last in the list, deliberately**: `GraphQlQuery` sits in the same
        // leaf context as `BodyEditor`, so these tie with the editor's own `up`, `down` and
        // `enter` (and the global `tab` and `escape`) and win only by registering later. Each
        // handler forwards to what the key did before whenever no list is open.
        bind!(mac, "down", CompleteNext, Some("GraphQlQuery")),
        bind!(mac, "up", CompletePrev, Some("GraphQlQuery")),
        bind!(mac, "tab", CompleteAccept, Some("GraphQlQuery")),
        bind!(mac, "enter", CompleteConfirm, Some("GraphQlQuery")),
        bind!(mac, "escape", CompleteDismiss, Some("GraphQlQuery")),
        bind!(mac, "ctrl-space", TriggerCompletion, Some("GraphQlQuery")),
    ]
}
