//! The root view. Owns the buffers, hosts every action handler, and draws the
//! chrome around the panes.
//!
//! Action handlers live here rather than on `RequestView` on purpose: dispatch
//! travels up the focus tree, and `Workspace` is the one element guaranteed to be
//! on that path no matter which region holds focus. Handlers that need buffer
//! state reach into the active `RequestView` through its entity.

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Window, div,
};
use zuno_core::RequestSpec;

use crate::actions::{
    FocusBody, FocusNext, FocusPrev, FocusResponse, FocusUrl, SendRequest, ToggleTheme,
};
use crate::request_view::RequestView;
use crate::theme::{ActiveTheme, Theme};

pub struct Workspace {
    focus_handle: FocusHandle,
    /// One entry per open request. Only `active_ix` is rendered in M1.0 — the tab
    /// strip arrives in M2, but the ownership shape is already right.
    views: Vec<Entity<RequestView>>,
    active_ix: usize,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.new(|cx| RequestView::new(RequestSpec::sample(), cx));

        // Start focused on the URL bar — the loop begins with typing a URL, and
        // it puts a RequestView on the focus path so its key contexts are live
        // from the first frame.
        let url_focus = view.read(cx).url_focus.clone();
        window.focus(&url_focus);

        Self {
            focus_handle: cx.focus_handle(),
            views: vec![view],
            active_ix: 0,
        }
    }

    fn active(&self) -> Option<Entity<RequestView>> {
        self.views.get(self.active_ix).cloned()
    }

    /// `Window::focus` refreshes the whole window internally, so there's no
    /// `cx.notify()` here — the child `RequestView` repaints (and updates its
    /// focus ring) on its own. It also no-ops when the handle is already focused,
    /// which means a redundant notify would cost a frame for nothing.
    fn focus_region(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        pick: impl Fn(&RequestView) -> FocusHandle,
    ) {
        let Some(view) = self.active() else { return };
        let handle = pick(view.read(cx));
        window.focus(&handle);
    }

    fn focus_url(&mut self, _: &FocusUrl, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view| view.url_focus.clone());
    }

    fn focus_body(&mut self, _: &FocusBody, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view| view.body_focus.clone());
    }

    fn focus_response(&mut self, _: &FocusResponse, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view| view.response_focus.clone());
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, _: &mut Context<Self>) {
        window.focus_next();
    }

    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, _: &mut Context<Self>) {
        window.focus_prev();
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        cx.global_mut::<Theme>().toggle();
        // A theme change repaints every window, not just this view.
        cx.refresh_windows();
    }

    /// Placeholder until the engine lands in M1.2. It exists so that action
    /// dispatch — including the context-scoped `enter` binding in the URL bar —
    /// is observably wired rather than assumed.
    fn send_request(&mut self, _: &SendRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        view.update(cx, |view, cx| {
            view.status = Some(SharedString::from(
                "Send dispatched — the HTTP engine lands in M1.2, so nothing left the machine.",
            ));
            cx.notify();
        });
    }

    fn focused_region(&self, window: &Window, cx: &App) -> &'static str {
        let Some(view) = self.views.get(self.active_ix) else {
            return "—";
        };
        let view = view.read(cx);

        if view.url_focus.is_focused(window) {
            "URL"
        } else if view.body_focus.is_focused(window) {
            "Body"
        } else if view.response_focus.is_focused(window) {
            "Response"
        } else {
            "Window"
        }
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
            .map(|view| SharedString::from(view.read(cx).spec.name.clone()))
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
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::send_request))
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
                        .child("M1.0 · shell".to_string()),
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
    focused_region: &'static str,
    message: Option<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    const HINTS: &str =
        "Ctrl+L url · Ctrl+B body · Ctrl+Shift+R response · Tab cycle · Ctrl+Enter send · Ctrl+Q quit";

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
                .child(
                    div()
                        .text_color(theme.accent)
                        .child(format!("focus: {focused_region}")),
                )
                .children(message.map(|message| {
                    div().text_color(theme.text_muted).child(message)
                })),
        )
        .child(div().text_color(theme.text_muted).child(HINTS.to_string()))
}
