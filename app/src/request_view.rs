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

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Task, Window, div, px,
};
use zuno_core::{
    Body, Engine, EngineError, Event, Header, JobId, Method, QueryParam, RequestId, RequestSettings,
    RequestSpec, ResponseData,
};

use crate::body_view::BodyView;
use crate::input::TextInput;
use crate::theme::ActiveTheme;
use crate::{request_pane, response_pane};

/// What's known about a request that hasn't finished yet.
///
/// Populated incrementally from the engine's event stream, which is the point of the
/// stream existing: status and headers land at TTFB, byte counts while the body
/// downloads.
pub struct InFlight {
    pub job: JobId,
    pub status: Option<(u16, String)>,
    pub headers: Vec<Header>,
    pub ttfb: Option<Duration>,
    pub received: usize,
    pub total: Option<usize>,
    /// Holding the task is what keeps the event loop alive; dropping it stops
    /// consumption. Never read — that's the whole contract.
    _task: Task<()>,
}

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
    /// The indexed body. `None` while it's still being built off-thread.
    pub body_view: Option<BodyView>,
    body_task: Option<Task<()>>,
    pub inflight: Option<InFlight>,
    pub error: Option<EngineError>,
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
            // No canned response any more — the pane starts empty and fills from a
            // real request.
            response: None,
            body_view: None,
            body_task: None,
            inflight: None,
            error: None,
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

    // ---- the send loop ------------------------------------------------------

    /// Submit the request as it currently appears on screen.
    pub fn send(&mut self, engine: &Arc<Engine>, cx: &mut Context<Self>) {
        // Hitting Send again must abandon the previous attempt immediately. Without
        // this, a rapid resend leaves the old socket draining and the new response can
        // land behind a stale one.
        self.cancel(engine, cx);

        let spec = self.spec(cx);
        let (job, events) = engine.send(spec);

        self.error = None;
        self.response = None;
        self.body_view = None;
        self.body_task = None;
        self.status = None;

        // Consume the event stream on the foreground executor. `update` fails once the
        // view is gone, which ends the loop.
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                match this.update(cx, |this, cx| this.apply(event, cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        });

        self.inflight = Some(InFlight {
            job,
            status: None,
            headers: Vec::new(),
            ttfb: None,
            received: 0,
            total: None,
            _task: task,
        });
        cx.notify();
    }

    /// Returns false when the event should end the loop.
    fn apply(&mut self, event: Event, cx: &mut Context<Self>) -> bool {
        // A cancelled or superseded job can still have events queued. Ignore anything
        // that isn't the request we're currently waiting on.
        let Some(current) = self.inflight.as_ref().map(|inflight| inflight.job) else {
            return false;
        };
        if event.job() != current {
            return true;
        }

        match event {
            Event::Done { response, .. } => {
                timing!(
                    "request  ttfb {:>9.2?}  total {:>9.2?}  {} bytes",
                    response.timing.ttfb,
                    response.timing.total,
                    response.size.decoded
                );
                self.inflight = None;
                self.response = Some(*response);
                self.index_body(false, cx);
                cx.notify();
                false
            }
            Event::Failed { error, .. } => {
                self.inflight = None;
                self.error = Some(error);
                cx.notify();
                false
            }
            Event::Started { .. } => true,
            Event::Head {
                status,
                status_text,
                headers,
                ttfb,
                ..
            } => {
                if let Some(inflight) = self.inflight.as_mut() {
                    inflight.status = Some((status, status_text));
                    inflight.headers = headers;
                    inflight.ttfb = Some(ttfb);
                }
                cx.notify();
                true
            }
            Event::Progress {
                received, total, ..
            } => {
                if let Some(inflight) = self.inflight.as_mut() {
                    inflight.received = received;
                    inflight.total = total;
                }
                cx.notify();
                true
            }
        }
    }

    /// Abandon an in-flight request. Returns whether there was one.
    ///
    /// Cancellation has two halves and needs both: dropping the task stops the UI
    /// consuming events, and `Engine::cancel` is what actually stops the socket.
    pub fn cancel(&mut self, engine: &Arc<Engine>, cx: &mut Context<Self>) -> bool {
        let Some(inflight) = self.inflight.take() else {
            return false;
        };
        engine.cancel(inflight.job);
        drop(inflight);
        cx.notify();
        true
    }

    pub fn is_sending(&self) -> bool {
        self.inflight.is_some()
    }

    // ---- body indexing ------------------------------------------------------

    /// Classify and index the response body on a background thread.
    ///
    /// Parsing 10MB of JSON is ~48ms and allocates 1.3M rows; doing that inline would
    /// drop three frames and defeat the entire point of the viewer. Only the finished
    /// index crosses back (architecture.md §1, rule 2).
    ///
    /// Note `background_executor().spawn` rather than `cx.background_spawn` — the latter
    /// doesn't exist in gpui 0.2.2.
    pub fn index_body(&mut self, force_parse: bool, cx: &mut Context<Self>) {
        let Some(response) = &self.response else {
            self.body_view = None;
            self.body_task = None;
            return;
        };

        let body = response.body.clone(); // Bytes: refcount bump, not a copy
        let content_type = response.content_type().map(str::to_string);
        let len = body.len();

        self.body_view = None;
        let build = cx
            .background_executor()
            .spawn(async move { BodyView::build(body, content_type, force_parse) });

        self.body_task = Some(cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let view = build.await;
            let elapsed = started.elapsed();

            let _ = this.update(cx, |this, cx| {
                timing!(
                    "body     index {:>9.2?}  {len} bytes  {} rows",
                    elapsed,
                    view.row_count()
                );
                this.body_view = Some(view);
                cx.notify();
            });
        }));
    }

    pub fn toggle_fold(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        if let Some(body) = self.body_view.as_mut() {
            body.toggle_fold(row_ix);
            cx.notify();
        }
    }

    pub fn set_all_folded(&mut self, folded: bool, cx: &mut Context<Self>) {
        if let Some(body) = self.body_view.as_mut() {
            body.set_all_folded(folded);
            cx.notify();
        }
    }

    /// Parse an over-the-cap body anyway, at the user's explicit request.
    pub fn force_parse_body(&mut self, cx: &mut Context<Self>) {
        self.index_body(true, cx);
        cx.notify();
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
            .child(response_pane::render(self, &theme, window, cx))
    }
}
