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
    ParentElement, Render, SharedString, Styled, Window, div,
};
use zuno_core::RequestSpec;

use crate::actions::{
    AddHeader, AddQuery, CancelRequest, CycleMethod, CycleMethodBack, FocusBody, FocusNext,
    FocusPrev, FocusResponse, FocusUrl, RemoveRow, SendRequest, ToggleRow, ToggleTheme,
};
use crate::engine::ActiveEngine;
use crate::request_view::{RequestView, RowKind};
use crate::theme::{ActiveTheme, Theme};

pub struct Workspace {
    focus_handle: FocusHandle,
    /// One entry per open request. Only `active_ix` is rendered in M1.1 — the tab
    /// strip arrives in M2, but the ownership shape is already right.
    views: Vec<Entity<RequestView>>,
    active_ix: usize,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.new(|cx| RequestView::new(RequestSpec::sample(), cx));

        // Start focused on the URL bar — the loop begins with typing a URL, and it
        // puts the URL input on the focus path so its key context is live from the
        // first frame.
        let url_focus = view.read(cx).url_focus(cx);
        window.focus(&url_focus);

        Self {
            focus_handle: cx.focus_handle(),
            views: vec![view],
            active_ix: 0,
        }
    }

    pub fn active(&self) -> Option<Entity<RequestView>> {
        self.views.get(self.active_ix).cloned()
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
        self.focus_region(window, cx, |view, _| view.body_focus.clone());
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

        view.update(cx, |view, cx| view.send(&engine, cx));
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
        if view.body_focus.is_focused(window) {
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
            .map(|view| SharedString::from(view.read(cx).name.clone()))
            .unwrap_or_else(|| SharedString::from("No request"));

        div()
            .id("zuno")
            .key_context("Zuno")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_url))
            .on_action(cx.listener(Self::focus_body))
            .on_action(cx.listener(Self::focus_response))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_action(cx.listener(Self::cycle_method))
            .on_action(cx.listener(Self::cycle_method_back))
            .on_action(cx.listener(Self::add_header))
            .on_action(cx.listener(Self::add_query))
            .on_action(cx.listener(Self::toggle_row))
            .on_action(cx.listener(Self::remove_row))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::send_request))
            .on_action(cx.listener(Self::cancel_request))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .text_sm()
            .child(titlebar(title, &theme))
            .children(self.active())
            .child(status_bar(focused_region, status_message, &theme))
    }
}

fn titlebar(title: SharedString, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .px_3()
        .py_2()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().text_sm().text_color(theme.text).child(title))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("M1.2 · live".to_string()),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child(format!("{} · Ctrl+Shift+T", theme.appearance.label())),
        )
}

fn status_bar(
    focused_region: SharedString,
    message: Option<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    const HINTS: &str = "Ctrl+M method · Ctrl+Shift+H header · Alt+T mute · Ctrl+Enter send · Esc cancel";

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
