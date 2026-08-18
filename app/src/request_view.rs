//! One request buffer: a spec, its latest response, and the focus handles for
//! the three regions you move between.
//!
//! This is the "buffer" level from architecture.md §11 — `Workspace` owns a
//! `Vec<Entity<RequestView>>` with an `active_ix` from the start, so adding a tab
//! strip in M2 is a rendering change rather than an ownership refactor.
//!
//! `request_pane` and `response_pane` are its two render halves. They stay plain
//! functions until M1.1, when the request side grows state of its own and becomes
//! an entity.

use gpui::{
    App, Context, FocusHandle, Focusable, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, px,
};
use zuno_core::{RequestSpec, ResponseData};

use crate::theme::ActiveTheme;
use crate::{request_pane, response_pane};

pub struct RequestView {
    pub spec: RequestSpec,
    pub response: Option<ResponseData>,
    /// Transient one-line message. The engine replaces this with real in-flight
    /// state in M1.2.
    pub status: Option<SharedString>,

    pub url_focus: FocusHandle,
    pub body_focus: FocusHandle,
    pub response_focus: FocusHandle,
}

impl RequestView {
    pub fn new(spec: RequestSpec, cx: &mut Context<Self>) -> Self {
        Self {
            spec,
            // Hardcoded until the engine lands in M1.2.
            response: Some(ResponseData::sample()),
            status: None,
            // tab_index drives Tab / Shift-Tab order.
            url_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            body_focus: cx.focus_handle().tab_index(1).tab_stop(true),
            response_focus: cx.focus_handle().tab_index(2).tab_stop(true),
        }
    }
}

impl Focusable for RequestView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.url_focus.clone()
    }
}

impl Render for RequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .flex_1()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(request_pane::render(self, &theme, window))
            .child(div().w(px(1.)).flex_none().bg(theme.border))
            .child(response_pane::render(self, &theme, window))
    }
}
