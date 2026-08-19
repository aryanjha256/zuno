//! The root view. Owns the buffers, hosts every application action handler, and
//! draws the chrome around the panes.
//!
//! Action handlers live here rather than on `RequestView` on purpose: dispatch
//! travels up the focus tree, and `Workspace` is the one element guaranteed to be on
//! that path no matter which region holds focus — including when focus is inside a
//! `TextInput` nested two levels down. Handlers that need buffer state reach into the
//! active `RequestView` through its entity.

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, div, px,
};
use zuno_core::{RequestId, RequestSpec};

use crate::actions::{
    AddHeader, AddQuery, CancelRequest, CloseTab, CycleBodyKind, CycleMethod, CycleMethodBack,
    FocusBody, FocusNext, FocusPrev, FocusResponse, FocusUrl, FoldAll, ImportCurl, NewTab, NextTab,
    PrevTab, Quit, RemoveRow, SendRequest, ToggleRow, ToggleTheme, UnfoldAll,
};
use crate::engine::ActiveEngine;
use crate::request_view::{RequestView, RowKind};
use crate::theme::{ActiveTheme, Theme};

pub struct Workspace {
    focus_handle: FocusHandle,
    /// Last title handed to the OS, so `set_window_title` isn't called every frame.
    window_title: String,
    /// Holding the quit subscription is what keeps it alive.
    _quit_subscription: Subscription,
    /// One entry per open request; only `active_ix` is rendered as a pane. Every mutation
    /// goes through `activate`, which is what keeps focus and `active_ix` from disagreeing.
    views: Vec<Entity<RequestView>>,
    active_ix: usize,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Reopen where you left off. A missing or unreadable session falls back to one
        // sample request rather than an empty window. `session::load` guarantees a
        // non-empty `tabs` and an in-range `active`, so neither is re-checked here.
        let session = crate::session::load(cx)
            .unwrap_or_else(|| crate::session::Session::single(RequestSpec::sample()));
        let active_ix = session.active;
        let views: Vec<_> = session
            .tabs
            .into_iter()
            .map(|spec| cx.new(|cx| RequestView::new(spec, cx)))
            .collect();

        // Start focused on the URL bar of the buffer that was in front — the loop begins
        // with typing a URL, and it puts that input on the focus path so its key context
        // is live from the first frame.
        let url_focus = views[active_ix].read(cx).url_focus(cx);
        window.focus(&url_focus);

        // Save on *every* quit path, not just Ctrl+Q. Before this, closing the window
        // with the window manager's button lost every edit made since the last send —
        // `session::save` was only reachable from the Send and Quit actions.
        let quit_subscription = cx.on_app_quit(|workspace, cx| {
            crate::session::save(&workspace.session(cx), cx);
            // The hook wants a future; there is nothing to await, since the write is
            // synchronous and must finish before the process goes away.
            async {}
        });

        Self {
            focus_handle: cx.focus_handle(),
            window_title: String::new(),
            _quit_subscription: quit_subscription,
            views,
            active_ix,
        }
    }

    pub fn active(&self) -> Option<Entity<RequestView>> {
        self.views.get(self.active_ix).cloned()
    }

    /// How many buffers are open. The strip renders from `render`'s own collected list, so
    /// this stays test-only until something in the UI needs the bare count.
    #[cfg(test)]
    pub fn tab_count(&self) -> usize {
        self.views.len()
    }

    /// Move focus into a buffer's URL bar and repaint.
    ///
    /// Every switch has to end here. A `FocusHandle` belongs to the entity that made it,
    /// so after the active buffer changes, focus is still sitting inside the *old* view —
    /// and after a close it's inside a dropped one, where no key context matches and the
    /// keymap goes dead with nothing on screen explaining why.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.views.get(ix) else { return };
        self.active_ix = ix;

        let url_focus = view.read(cx).url_focus(cx);
        window.focus(&url_focus);
        // Unlike `focus_region`, this needs the notify: the strip and the title change
        // even when `window.focus` finds the handle already focused.
        cx.notify();
    }

    /// Ids have to be distinct across buffers, and nothing hands them out — `sample()`
    /// hardcodes 1 and `default()` 0. Highest-plus-one is enough for a session's lifetime
    /// and needs no counter to persist and keep in sync.
    fn next_id(&self, cx: &App) -> RequestId {
        let highest = self
            .views
            .iter()
            .map(|view| view.read(cx).id.0)
            .max()
            .unwrap_or(0);
        RequestId(highest + 1)
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let spec = RequestSpec {
            id: self.next_id(cx),
            ..RequestSpec::default()
        };
        self.open(spec, window, cx);
    }

    /// Add a buffer and switch to it. Shared by `NewTab` and curl import.
    fn open(&mut self, spec: RequestSpec, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| RequestView::new(spec, cx));
        self.views.push(view);
        self.activate(self.views.len() - 1, window, cx);
    }

    /// Closing the last buffer leaves a fresh one rather than an empty window.
    ///
    /// An empty `views` would make `active()` return `None`, which every action handler
    /// treats as "do nothing" — the window would still be there, silently inert. Ctrl+W
    /// also shouldn't quit the app; that's Ctrl+Q's job.
    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() {
            return;
        }

        self.views.remove(self.active_ix);

        if self.views.is_empty() {
            let spec = RequestSpec::default();
            self.open(spec, window, cx);
            return;
        }

        // Closing the last tab in the strip moves left; anything else keeps the index,
        // which now points at what was to the right — the behaviour every editor has.
        let next = self.active_ix.min(self.views.len() - 1);
        self.activate(next, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() {
            return;
        }
        // Wraps, so cycling never dead-ends at either edge.
        let next = (self.active_ix + 1) % self.views.len();
        self.activate(next, window, cx);
    }

    fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() {
            return;
        }
        let prev = (self.active_ix + self.views.len() - 1) % self.views.len();
        self.activate(prev, window, cx);
    }

    /// The persistable state of every open buffer.
    ///
    /// Walks *all* views, not just the active one. Saving only `active()` was correct
    /// while one buffer was the only buffer; with a tab strip coming it would quietly
    /// discard every other open request on quit.
    fn session(&self, cx: &App) -> crate::session::Session {
        let tabs = self
            .views
            .iter()
            .map(|view| view.read(cx).spec(cx))
            .collect();
        crate::session::Session::new(tabs, self.active_ix)
    }

    /// `Window::focus` refreshes the whole window internally, so there's no
    /// `cx.notify()` here — the child `RequestView` repaints (and updates its focus
    /// ring) on its own. It also no-ops when the handle is already focused, so a
    /// redundant notify would cost a frame for nothing.
    fn focus_region(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        pick: impl Fn(&RequestView, &App) -> FocusHandle,
    ) {
        let Some(view) = self.active() else { return };
        let handle = pick(view.read(cx), cx);
        window.focus(&handle);
    }

    fn focus_url(&mut self, _: &FocusUrl, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view, cx| view.url_focus(cx));
    }

    fn focus_body(&mut self, _: &FocusBody, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view, cx| view.body_focus(cx));
    }

    fn focus_response(&mut self, _: &FocusResponse, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view, _| view.response_focus.clone());
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, _: &mut Context<Self>) {
        window.focus_next();
    }

    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, _: &mut Context<Self>) {
        window.focus_prev();
    }

    fn cycle_method(&mut self, _: &CycleMethod, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.cycle_method(true, cx));
        }
    }

    fn cycle_method_back(&mut self, _: &CycleMethodBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.cycle_method(false, cx));
        }
    }

    fn add_header(&mut self, _: &AddHeader, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.add_row(RowKind::Header, window, cx));
        }
    }

    fn add_query(&mut self, _: &AddQuery, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.add_row(RowKind::Query, window, cx));
        }
    }

    /// Row actions target whichever row currently holds focus, so there's no
    /// "selected row" index to keep valid across insertions and deletions.
    fn toggle_row(&mut self, _: &ToggleRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let handled = view.update(cx, |view, cx| view.toggle_focused_row(window, cx));
        if !handled {
            self.set_status("Focus a header or query row first", cx);
        }
    }

    fn remove_row(&mut self, _: &RemoveRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let handled = view.update(cx, |view, cx| view.remove_focused_row(window, cx));
        if !handled {
            self.set_status("Focus a header or query row first", cx);
        }
    }

    fn cycle_body_kind(&mut self, _: &CycleBodyKind, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.cycle_body_kind(cx));
        }
    }

    /// Open a request parsed from a curl command on the clipboard in a **new** buffer.
    ///
    /// Reading the clipboard rather than opening a paste dialog is deliberate: the whole
    /// value of this feature is that "Copy as cURL" in devtools is one keystroke away
    /// from a request you can edit.
    ///
    /// It replaced the active buffer until tabs existed, which was only ever defensible
    /// because there was nowhere else to put the result — an import over unsaved work
    /// destroyed it with no undo. `RequestView::load` still exists for genuine in-place
    /// replacement; it just isn't what an import is.
    fn import_curl(&mut self, _: &ImportCurl, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            self.set_status("Nothing on the clipboard — copy a curl command first", cx);
            return;
        };

        let import = match zuno_core::curl::parse(&text) {
            Ok(import) => import,
            Err(error) => {
                self.set_status(&format!("Could not import: {error}"), cx);
                return;
            }
        };

        // An import never silently drops part of the command — anything skipped is named.
        let message = if import.ignored.is_empty() {
            format!("Imported {}", import.spec.method.as_str())
        } else {
            format!("Imported — ignored {}", import.ignored.join(", "))
        };

        // The parsed spec carries `RequestId::default()`, which would collide with the
        // buffer already holding id 0.
        let spec = RequestSpec {
            id: self.next_id(cx),
            ..import.spec
        };
        self.open(spec, window, cx);
        // After `open`, so the status lands on the new buffer rather than the one that
        // happened to be in front when the import ran.
        self.set_status(&message, cx);
    }

    fn fold_all(&mut self, _: &FoldAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.set_all_folded(true, cx));
        }
    }

    fn unfold_all(&mut self, _: &UnfoldAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.set_all_folded(false, cx));
        }
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        cx.global_mut::<Theme>().toggle();
        // A theme change repaints every window, not just this view.
        cx.refresh_windows();
    }

    fn send_request(&mut self, _: &SendRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        let Some(engine) = cx.engine() else {
            self.set_status("The HTTP engine failed to start — restart Zuno", cx);
            return;
        };

        // A send is a natural checkpoint: persist here so a crash costs at most the
        // edits made since the last one. Every buffer is written, not just the one being
        // sent — the checkpoint is the window's state, not this request's.
        crate::session::save(&self.session(cx), cx);
        view.update(cx, |view, cx| view.send(&engine, cx));
    }

    /// `on_app_quit` does the saving, so this only has to ask the app to quit — one save
    /// path instead of one per exit route.
    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn cancel_request(&mut self, _: &CancelRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let Some(engine) = cx.engine() else { return };

        let cancelled = view.update(cx, |view, cx| view.cancel(&engine, cx));
        if cancelled {
            self.set_status("Cancelled", cx);
        }
    }

    fn set_status(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            let message = SharedString::from(message.to_string());
            view.update(cx, |view, cx| {
                view.status = Some(message);
                cx.notify();
            });
        }
    }


    fn focused_region(&self, window: &Window, cx: &App) -> SharedString {
        let Some(view) = self.views.get(self.active_ix) else {
            return SharedString::from("—");
        };
        let view = view.read(cx);

        if view.url_focus(cx).is_focused(window) {
            return SharedString::from("URL");
        }
        if view.body_focus(cx).is_focused(window) {
            return SharedString::from("Body");
        }
        if view.response_focus.is_focused(window) {
            return SharedString::from("Response");
        }
        if let Some((kind, ix)) = view.focused_row(window, cx) {
            let label = match kind {
                RowKind::Header => "header",
                RowKind::Query => "query",
            };
            return SharedString::from(format!("{label} row {}", ix + 1));
        }
        SharedString::from("Window")
    }

    fn status_message(&self, cx: &App) -> Option<SharedString> {
        self.views.get(self.active_ix)?.read(cx).status.clone()
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let focused_region = self.focused_region(window, cx);
        let status_message = self.status_message(cx);
        let title = self
            .views
            .get(self.active_ix)
            .map(|view| view.read(cx).label(cx))
            .unwrap_or_else(|| SharedString::from("No request"));

        // Collected before building elements: the closures below borrow `cx` mutably, so
        // the labels can't be read from the views while they're alive.
        let tabs: Vec<(usize, SharedString, bool)> = self
            .views
            .iter()
            .enumerate()
            .map(|(ix, view)| (ix, view.read(cx).label(cx), ix == self.active_ix))
            .collect();

        // The window title tracks the request, so the taskbar entry is useful even
        // though we draw our own titlebar. Only written when it changes — this runs every
        // frame.
        let window_title = format!("{title} — Zuno");
        if self.window_title != window_title {
            window.set_window_title(&window_title);
            self.window_title = window_title;
        }

        div()
            .id("zuno")
            .key_context("Zuno")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_url))
            .on_action(cx.listener(Self::focus_body))
            .on_action(cx.listener(Self::focus_response))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::cycle_method))
            .on_action(cx.listener(Self::cycle_method_back))
            .on_action(cx.listener(Self::add_header))
            .on_action(cx.listener(Self::add_query))
            .on_action(cx.listener(Self::toggle_row))
            .on_action(cx.listener(Self::remove_row))
            .on_action(cx.listener(Self::cycle_body_kind))
            .on_action(cx.listener(Self::import_curl))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::fold_all))
            .on_action(cx.listener(Self::unfold_all))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::send_request))
            .on_action(cx.listener(Self::cancel_request))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .text_sm()
            .relative()
            .child(crate::chrome::titlebar(title, &theme, window))
            .children(tab_strip(tabs, &theme, cx))
            .children(self.active())
            .child(status_bar(focused_region, status_message, &theme))
            // Last, so the edge strips sit above the panes for hit-testing.
            .children(crate::chrome::resize_handles(window))
    }

}


/// The strip of open buffers.
///
/// Hidden entirely at one buffer: a single tab is a row of chrome that says nothing, and
/// the window title already names the request. It appears the moment there's a choice to
/// make, which is also the moment it starts carrying information.
fn tab_strip(
    tabs: Vec<(usize, SharedString, bool)>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> Option<impl IntoElement> {
    if tabs.len() < 2 {
        return None;
    }

    Some(
        div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .w_full()
            // Many tabs must scroll rather than squeeze every label into illegibility.
            // `overflow_x_scroll` lives on `StatefulInteractiveElement`, hence the `.id()`.
            .overflow_x_scroll()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .children(tabs.into_iter().map(|(ix, label, active)| {
                div()
                    .id(("tab", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_none()
                    .max_w(px(180.))
                    // A long label must clip, not push its neighbours off the strip.
                    .overflow_hidden()
                    .px_3()
                    .py_1()
                    .border_r_1()
                    .border_color(theme.border)
                    // The active tab is marked by a top rule in the accent colour rather
                    // than by text weight: reflowing on switch would shift every label.
                    .border_t_2()
                    .border_color(if active { theme.accent } else { theme.bg_panel })
                    .bg(if active { theme.bg } else { theme.bg_panel })
                    .text_xs()
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.bg_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                            workspace.activate(ix, window, cx);
                        }),
                    )
                    // Middle-click closes, the convention every browser and editor shares.
                    // It closes *that* tab, not the active one, so it activates first.
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                            workspace.activate(ix, window, cx);
                            workspace.close_tab(&CloseTab, window, cx);
                        }),
                    )
                    .child(label)
            })),
    )
}

fn status_bar(
    focused_region: SharedString,
    message: Option<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    const HINTS: &str = "Ctrl+T tab · Ctrl+Tab switch · Ctrl+Shift+V import curl · Ctrl+M method · Ctrl+Shift+H header · Ctrl+Enter send · Esc cancel";

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .flex_none()
        .px_3()
        .py_1()
        .bg(theme.bg_panel)
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w(gpui::px(0.))
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.accent)
                        .child(format!("focus: {focused_region}")),
                )
                .children(message.map(|message| {
                    div()
                        .flex_1()
                        .min_w(gpui::px(0.))
                        .truncate()
                        .text_color(theme.text_muted)
                        .child(message)
                })),
        )
        .child(
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(HINTS.to_string()),
        )
}
