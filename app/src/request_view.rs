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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Task, Window, div, px,
};
use zuno_core::{
    Body, Engine, EngineError, Event, Header, JobId, Method, QueryParam, RawKind, RequestId,
    RequestSettings, RequestSpec, Resolver, ResponseData, ResponseDiff,
};

use crate::body_view::BodyView;
use crate::input::{Editor, TextInput};
use crate::theme::ActiveTheme;
use crate::{request_pane, response_pane};

/// How many previous responses to keep for comparison.
pub const HISTORY_LIMIT: usize = 10;

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
    /// The collection file this buffer is backed by, or `None` for a scratch buffer.
    ///
    /// Set when a buffer is opened from a collection or saved into one. It exists because a
    /// filename derived from the URL is *not* an identity: without remembering the file, a
    /// second Ctrl+S would derive the same name, find it taken, and write `posts-2.json`.
    pub path: Option<PathBuf>,
    pub method: Method,
    pub url: Entity<TextInput>,
    pub headers: Vec<KeyValueRow>,
    pub query: Vec<KeyValueRow>,
    /// The editor owns the body text, exactly as the inputs own theirs — `spec()` reads
    /// through to it rather than mirroring into a field.
    pub body_editor: Entity<Editor>,
    pub body_kind: RawKind,
    pub settings: RequestSettings,

    pub response: Option<ResponseData>,
    /// How the current response differs from the one before it. `None` on the first run.
    pub diff: Option<ResponseDiff>,
    /// Previous responses, newest first, capped at `HISTORY_LIMIT`.
    pub history: Vec<ResponseData>,
    /// Which run is on screen: `0` is the live response, `1` the run before it, and so on
    /// into `history`.
    ///
    /// An index rather than a cloned `ResponseData`, so there is exactly one copy of each
    /// run and `history` stays the only record of what happened. Reset to 0 whenever a new
    /// response lands — you want to see what you just sent, not stay parked in the past.
    viewing: usize,
    /// The indexed body. `None` while it's still being built off-thread.
    pub body_view: Option<BodyView>,
    body_task: Option<Task<()>>,
    pub inflight: Option<InFlight>,
    pub error: Option<EngineError>,
    pub status: Option<SharedString>,

    pub response_focus: FocusHandle,
}

impl RequestView {
    pub fn new(spec: RequestSpec, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            id: spec.id,
            name: String::new(),
            path: None,
            method: Method::Get,
            url: cx.new(|cx| TextInput::new("", "", "UrlBar", cx)),
            headers: Vec::new(),
            query: Vec::new(),
            body_editor: cx.new(|cx| Editor::new("", "Request body…", cx)),
            body_kind: RawKind::Json,
            settings: RequestSettings::default(),
            response: None,
            diff: None,
            history: Vec::new(),
            viewing: 0,
            body_view: None,
            body_task: None,
            inflight: None,
            error: None,
            status: None,
            // Higher than the inputs' default 0, so Tab reaches every text field first
            // and only then leaves for the response pane. The body editor sets its own
            // handle to tab_stop, at the default index, so it lands with the inputs.
            response_focus: cx.focus_handle().tab_index(2).tab_stop(true),
        };
        view.load(spec, cx);
        view
    }

    /// Replace this buffer's contents with a different request.
    ///
    /// Used by curl import. Deliberately in-place rather than swapping in a fresh
    /// entity: replacing the entity invalidates every handle to it and resets focus, and
    /// the buffer's *identity* hasn't changed — only what's in it.
    ///
    /// `response_focus` is intentionally not rebuilt, so focus survives the swap.
    pub fn load(&mut self, spec: RequestSpec, cx: &mut Context<Self>) {
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

        // The model still supports form, multipart, and binary bodies; the UI only
        // authors raw ones so far, so anything else opens as an empty raw body.
        let (body_text, body_kind) = match &spec.body {
            Body::Raw { text, kind } => (text.clone(), *kind),
            _ => (String::new(), RawKind::Json),
        };
        let body_editor = cx.new(|cx| Editor::new(body_text, "Request body…", cx));

        self.id = spec.id;
        self.name = spec.name;
        self.method = spec.method;
        self.url = url;
        self.headers = headers;
        self.query = query;
        self.body_editor = body_editor;
        self.body_kind = body_kind;
        self.settings = spec.settings;

        // A different request has no relationship to the last one's response.
        self.response = None;
        self.diff = None;
        self.history.clear();
        self.viewing = 0;
        self.body_view = None;
        self.body_task = None;
        self.inflight = None;
        self.error = None;
        self.status = None;

        cx.notify();
    }

    /// The response on screen: the live one, or a retained earlier run.
    ///
    /// Everything that renders or indexes a response goes through this rather than reading
    /// `response` directly, which is what makes browsing history a change of one number.
    pub fn displayed(&self) -> Option<&ResponseData> {
        if self.viewing == 0 {
            self.response.as_ref()
        } else {
            self.history.get(self.viewing - 1)
        }
    }

    /// How many runs back the display is. `0` is live.
    pub fn viewing(&self) -> usize {
        self.viewing
    }

    /// Every run that can be shown, newest first, as `(offset, response)`.
    ///
    /// Offset 0 is the live response, so the list is "what happened", not "what happened
    /// before now" — the current run belongs in it or the picker can't take you back.
    pub fn runs(&self) -> Vec<(usize, &ResponseData)> {
        self.response
            .iter()
            .map(|response| (0, response))
            .chain(
                self.history
                    .iter()
                    .enumerate()
                    .map(|(ix, response)| (ix + 1, response)),
            )
            .collect()
    }

    /// Show a retained run. Re-indexes the body, since the outline belongs to one response.
    pub fn view_run(&mut self, offset: usize, cx: &mut Context<Self>) {
        // Out of range would blank the pane with no way back; ignoring is the safer failure.
        if offset != 0 && self.history.get(offset - 1).is_none() {
            return;
        }
        if self.viewing == offset {
            return;
        }
        self.viewing = offset;
        self.index_body(false, cx);
        cx.notify();
    }

    /// This buffer's tab label, derived live from the URL as it's typed.
    ///
    /// Deliberately *not* `spec(cx).label()`: the strip asks every buffer for this on every
    /// frame, and `spec` clones the URL, every header, every query param, and the body.
    pub fn label(&self, cx: &App) -> SharedString {
        SharedString::from(zuno_core::label_for(self.url.read(cx).text(), &self.name).to_string())
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
            body: self.body(cx),
            settings: self.settings.clone(),
        }
    }

    /// A blank editor means no body at all, not an empty raw one — sending
    /// `Content-Type: application/json` with zero bytes confuses servers.
    fn body(&self, cx: &App) -> Body {
        let text = self.body_editor.read(cx).text();
        if text.trim().is_empty() {
            Body::Empty
        } else {
            Body::Raw {
                text: text.to_string(),
                kind: self.body_kind,
            }
        }
    }

    pub fn url_focus(&self, cx: &App) -> FocusHandle {
        self.url.read(cx).focus_handle(cx)
    }

    pub fn body_focus(&self, cx: &App) -> FocusHandle {
        self.body_editor.read(cx).focus_handle(cx)
    }

    pub fn cycle_body_kind(&mut self, cx: &mut Context<Self>) {
        const KINDS: [RawKind; 4] = [RawKind::Json, RawKind::Text, RawKind::Xml, RawKind::Html];
        let current = KINDS.iter().position(|k| *k == self.body_kind).unwrap_or(0);
        self.body_kind = KINDS[(current + 1) % KINDS.len()];
        cx.notify();
    }

    // ---- the send loop ------------------------------------------------------

    /// Submit the request as it currently appears on screen.
    /// Send, substituting variables first.
    ///
    /// The resolver is applied to a *copy*: the buffer keeps its `{{placeholders}}`, which
    /// is the entire point of having them. Anything the resolver doesn't know is left
    /// verbatim, so `build.rs`'s existing check is what reports it — by name, before DNS.
    pub fn send(&mut self, engine: &Arc<Engine>, resolver: &Resolver, cx: &mut Context<Self>) {
        // Hitting Send again must abandon the previous attempt immediately. Without
        // this, a rapid resend leaves the old socket draining and the new response can
        // land behind a stale one.
        self.cancel(engine, cx);

        let spec = resolver.apply(&self.spec(cx));
        let (job, events) = engine.send(spec);

        self.error = None;
        self.body_view = None;
        self.body_task = None;
        self.status = None;
        // `response` and `diff` are deliberately left in place until the new response
        // lands, so `apply` can diff against them.

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

                // Diff against the run this one replaces, then retire it to history.
                let previous = self.response.take();
                self.diff = previous
                    .as_ref()
                    .map(|previous| ResponseDiff::between(previous, &response));
                if let Some(previous) = previous {
                    self.history.insert(0, previous);
                    self.history.truncate(HISTORY_LIMIT);
                }

                self.response = Some(*response);
                // Back to live: a fresh response arriving while you're reading an old one
                // must not leave you staring at the old one with no sign anything happened.
                self.viewing = 0;
                self.index_body(false, cx);
                cx.notify();
                false
            }
            Event::Failed { error, .. } => {
                self.inflight = None;
                // The last successful response is deliberately kept: the pane shows the
                // error instead (a failure outranks a stale success), but keeping it
                // preserves the baseline so the *next* successful send still has
                // something to diff against. The diff itself has to go — it described a
                // comparison that no longer holds.
                self.diff = None;
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
        let Some(response) = self.displayed() else {
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
        SharedString::from(self.body_kind.label())
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
