//! One request buffer: the editable request, its latest response, and focus.
//!
//! **Single source of truth.** The `TextInput` entities own their text; there is no
//! parallel `RequestSpec` field kept in sync beside them. `spec(cx)` assembles a
//! `RequestSpec` on demand by reading the inputs. The alternative — storing a spec
//! and mirroring every keystroke into it via subscriptions — has two copies of every
//! string and a desync bug waiting in each one. Deriving instead means the spec that
//! goes on the wire in M1.2 is, by construction, exactly what's on screen.
//!
//! Fields that aren't text (`method`, `body`, `settings`, row `enabled` flags) are
//! plain state here, since nothing else owns them.

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div, px,
};
use zuno_core::{
    Body, Header, Method, QueryParam, RequestId, RequestSettings, RequestSpec, ResponseData,
};

use crate::input::TextInput;
use crate::theme::ActiveTheme;
use crate::{request_pane, response_pane};

/// One editable row of the headers or query tables. `enabled` lives here rather
/// than in the inputs because muting a row must not disturb what you typed.
pub struct KeyValueRow {
    pub enabled: bool,
    pub name: Entity<TextInput>,
    pub value: Entity<TextInput>,
}

impl KeyValueRow {
    fn new(
        enabled: bool,
        name: &str,
        value: &str,
        context: &'static str,
        cx: &mut Context<RequestView>,
    ) -> Self {
        Self {
            enabled,
            name: cx.new(|cx| TextInput::new(name.to_string(), "name", context, cx)),
            value: cx.new(|cx| TextInput::new(value.to_string(), "value", context, cx)),
        }
    }

    fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.name.read(cx).focus_handle(cx).is_focused(window)
            || self.value.read(cx).focus_handle(cx).is_focused(window)
    }
}

/// Which table a row belongs to. Used by the row actions to find their target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Header,
    Query,
}

pub struct RequestView {
    pub id: RequestId,
    pub name: String,
    pub method: Method,
    pub url: Entity<TextInput>,
    pub headers: Vec<KeyValueRow>,
    pub query: Vec<KeyValueRow>,
    /// Still read-only in M1.1 — the multi-line editor is M1.4.
    pub body: Body,
    pub settings: RequestSettings,

    pub response: Option<ResponseData>,
    pub status: Option<SharedString>,

    pub body_focus: FocusHandle,
    pub response_focus: FocusHandle,
}

impl RequestView {
    pub fn new(spec: RequestSpec, cx: &mut Context<Self>) -> Self {
        let url = cx.new(|cx| {
            TextInput::new(spec.url.clone(), "https://api.example.com/…", "UrlBar", cx)
        });

        let headers = spec
            .headers
            .iter()
            .map(|header| {
                KeyValueRow::new(
                    header.enabled,
                    &header.name,
                    &header.value,
                    "HeaderCell",
                    cx,
                )
            })
            .collect();

        let query = spec
            .query
            .iter()
            .map(|param| {
                KeyValueRow::new(param.enabled, &param.name, &param.value, "QueryCell", cx)
            })
            .collect();

        Self {
            id: spec.id,
            name: spec.name,
            method: spec.method,
            url,
            headers,
            query,
            body: spec.body,
            settings: spec.settings,
            response: Some(ResponseData::sample()),
            status: None,
            // Higher than the inputs' default 0, so Tab reaches every text field
            // first and only then leaves for the body and response panes.
            body_focus: cx.focus_handle().tab_index(1).tab_stop(true),
            response_focus: cx.focus_handle().tab_index(2).tab_stop(true),
        }
    }

    /// Assemble the request exactly as it currently appears on screen.
    ///
    /// This is what M1.2's engine will send and what M2 will persist. Both get the
    /// same guarantee: no staleness, because nothing is cached.
    pub fn spec(&self, cx: &App) -> RequestSpec {
        RequestSpec {
            id: self.id,
            name: self.name.clone(),
            method: self.method.clone(),
            url: self.url.read(cx).text().to_string(),
            query: self
                .query
                .iter()
                .map(|row| QueryParam {
                    enabled: row.enabled,
                    name: row.name.read(cx).text().to_string(),
                    value: row.value.read(cx).text().to_string(),
                })
                .collect(),
            headers: self
                .headers
                .iter()
                .map(|row| Header {
                    enabled: row.enabled,
                    name: row.name.read(cx).text().to_string(),
                    value: row.value.read(cx).text().to_string(),
                })
                .collect(),
            body: self.body.clone(),
            settings: self.settings.clone(),
        }
    }

    pub fn url_focus(&self, cx: &App) -> FocusHandle {
        self.url.read(cx).focus_handle(cx)
    }

    // ---- structural edits ---------------------------------------------------

    pub fn cycle_method(&mut self, forward: bool, cx: &mut Context<Self>) {
        let methods = Method::common();
        let current = methods.iter().position(|m| *m == self.method).unwrap_or(0);
        let next = if forward {
            (current + 1) % methods.len()
        } else {
            (current + methods.len() - 1) % methods.len()
        };
        self.method = methods[next].clone();
        cx.notify();
    }

    /// Append an empty row and move focus into its name cell — adding a row you
    /// then have to click into would defeat the point.
    pub fn add_row(&mut self, kind: RowKind, window: &mut Window, cx: &mut Context<Self>) {
        let row = match kind {
            RowKind::Header => {
                let row = KeyValueRow::new(true, "", "", "HeaderCell", cx);
                self.headers.push(row);
                self.headers.last()
            }
            RowKind::Query => {
                let row = KeyValueRow::new(true, "", "", "QueryCell", cx);
                self.query.push(row);
                self.query.last()
            }
        };

        if let Some(row) = row {
            let handle = row.name.read(cx).focus_handle(cx);
            window.focus(&handle);
        }
        cx.notify();
    }

    /// The row containing focus, if any. Row actions operate on this rather than a
    /// stored "selected row", so there's no index to keep valid across edits.
    pub fn focused_row(&self, window: &Window, cx: &App) -> Option<(RowKind, usize)> {
        if let Some(ix) = self
            .headers
            .iter()
            .position(|row| row.is_focused(window, cx))
        {
            return Some((RowKind::Header, ix));
        }
        self.query
            .iter()
            .position(|row| row.is_focused(window, cx))
            .map(|ix| (RowKind::Query, ix))
    }

    pub fn toggle_focused_row(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some((kind, ix)) = self.focused_row(window, cx) else {
            return false;
        };
        let rows = match kind {
            RowKind::Header => &mut self.headers,
            RowKind::Query => &mut self.query,
        };
        rows[ix].enabled = !rows[ix].enabled;
        cx.notify();
        true
    }

    pub fn remove_focused_row(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some((kind, ix)) = self.focused_row(window, cx) else {
            return false;
        };
        match kind {
            RowKind::Header => {
                self.headers.remove(ix);
            }
            RowKind::Query => {
                self.query.remove(ix);
            }
        }
        cx.notify();
        true
    }

    pub fn toggle_row_at(&mut self, kind: RowKind, ix: usize, cx: &mut Context<Self>) {
        let rows = match kind {
            RowKind::Header => &mut self.headers,
            RowKind::Query => &mut self.query,
        };
        if let Some(row) = rows.get_mut(ix) {
            row.enabled = !row.enabled;
            cx.notify();
        }
    }

    pub fn remove_row_at(&mut self, kind: RowKind, ix: usize, cx: &mut Context<Self>) {
        let rows = match kind {
            RowKind::Header => &mut self.headers,
            RowKind::Query => &mut self.query,
        };
        if ix < rows.len() {
            rows.remove(ix);
            cx.notify();
        }
    }

    pub fn body_label(&self) -> SharedString {
        SharedString::from(match &self.body {
            Body::Raw { kind, .. } => kind.label(),
            other => other.label(),
        })
    }

}

impl Focusable for RequestView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.url_focus(cx)
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
            .child(request_pane::render(self, &theme, window, cx))
            .child(div().w(px(1.)).flex_none().bg(theme.border))
            .child(response_pane::render(self, &theme, window))
    }
}
