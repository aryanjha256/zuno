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
    Body, Engine, EngineError, Event, FormField, Header, JobId, Method, MultipartValue, QueryParam,
    RawKind, RequestId, RequestSettings, RequestSpec, Resolver, ResponseData, ResponseDiff,
};

/// Which body a request sends, among the kinds the UI can author.
///
/// Multipart is absent on purpose: it's held in `preserved_body` until its editor exists, and
/// offering a type nothing can edit would be worse than not offering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyType {
    Empty,
    Raw,
    Form,
    /// The contents of a file, sent as-is.
    Binary,
}

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
    /// A field of an `application/x-www-form-urlencoded` body.
    Form,
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
    /// Which body this request sends. `Raw` is further qualified by `body_kind`.
    ///
    /// Stored rather than inferred: a `Form` body with no fields yet and an `Empty` body are
    /// different intentions that look identical in the data, so nothing else records the
    /// choice. Ignored while `preserved_body` is set — see there.
    pub body_type: BodyType,
    pub body_kind: RawKind,
    /// Fields of a form body. Same widget as the header and query tables, since
    /// `FormField` has the same shape as `Header`.
    pub form: Vec<KeyValueRow>,
    /// The file a binary body sends.
    ///
    /// Only the path is held — the bytes are read at the send boundary by `build.rs`, so a
    /// file edited between sends goes out in its new state, and a 2GB upload never sits in
    /// this process's memory. A missing file surfaces as `BodyFileUnreadable` rather than
    /// being checked here, which would mean a filesystem call on every frame.
    pub binary_path: Option<PathBuf>,
    /// A body the UI cannot author yet — form, multipart, or binary — kept verbatim.
    ///
    /// **This exists to stop silent data loss, not as a feature.** `spec()` derives the body
    /// from the editor, and the editor can only represent raw text; so before this, loading
    /// a request with a form body produced an *empty* editor, and the next `Ctrl+S` wrote
    /// that emptiness over the real body. Reachable today, because curl import parses `-F`
    /// and `--data-binary @file` into exactly these variants.
    ///
    /// Deliberately **disjoint** from the editor rather than overlapping it: the editor
    /// stays the only source of truth for raw bodies, and this holds only what the editor
    /// cannot express. Two fields that could each describe the same body would be the
    /// mirroring this codebase avoids everywhere else. `Body::Empty` is *not* kept here —
    /// an empty body is editable, and typing into it should produce a raw one.
    pub preserved_body: Option<Body>,
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
            body_type: BodyType::Empty,
            body_kind: RawKind::Json,
            form: Vec::new(),
            binary_path: None,
            preserved_body: None,
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

        // Anything the editor and the form table can't represent is set aside rather than
        // dropped. Losing it here is invisible until a save writes the loss to disk.
        let (body_text, body_kind, body_type, preserved) = match &spec.body {
            Body::Raw { text, kind } => (text.clone(), *kind, BodyType::Raw, None),
            // Empty stays editable: typing into it is how you get a raw body.
            Body::Empty => (String::new(), RawKind::Json, BodyType::Empty, None),
            Body::Form(_) => (String::new(), RawKind::Json, BodyType::Form, None),
            Body::Binary(_) => (String::new(), RawKind::Json, BodyType::Binary, None),
            other => (
                String::new(),
                RawKind::Json,
                BodyType::Empty,
                Some(other.clone()),
            ),
        };

        let binary_path = match &spec.body {
            Body::Binary(path) => Some(path.clone()),
            _ => None,
        };

        let form = match &spec.body {
            Body::Form(fields) => fields
                .iter()
                .map(|field| {
                    KeyValueRow::new(field.enabled, &field.name, &field.value, "FormCell", cx)
                })
                .collect(),
            _ => Vec::new(),
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
        self.body_type = body_type;
        self.form = form;
        self.binary_path = binary_path;
        self.preserved_body = preserved;
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
        // A preserved body wins, because while one is held the editor is not shown — so
        // deriving from it would return `Empty` and destroy what was loaded.
        if let Some(body) = &self.preserved_body {
            return body.clone();
        }

        match self.body_type {
            BodyType::Form => Body::Form(
                self.form
                    .iter()
                    .map(|row| FormField {
                        enabled: row.enabled,
                        name: row.name.read(cx).text().to_string(),
                        value: row.value.read(cx).text().to_string(),
                    })
                    .collect(),
            ),
            // No file chosen yet is `Empty`, not a broken `Binary("")` — the request is
            // incomplete, not malformed, and sending nothing is the honest reading.
            BodyType::Binary => match &self.binary_path {
                Some(path) => Body::Binary(path.clone()),
                None => Body::Empty,
            },
            // Unconditional: "None" means no body even though the editor may still hold
            // text. Falling through to the editor here meant picking None sent the previous
            // body anyway — the setting looked applied and wasn't.
            BodyType::Empty => Body::Empty,
            BodyType::Raw => {
                let text = self.body_editor.read(cx).text();
                // An empty raw body is `Empty`, not `Raw("")`: it keeps a blank editor from
                // sending a Content-Type for content that isn't there.
                if text.trim().is_empty() {
                    Body::Empty
                } else {
                    Body::Raw {
                        text: text.to_string(),
                        kind: self.body_kind,
                    }
                }
            }
        }
    }

    /// Choose the body type.
    ///
    /// Only `preserved_body` is dropped — it can't be rendered or re-derived, so holding it
    /// alongside a chosen type would be invisible state. The editor's text and the form rows
    /// are *kept*, so switching JSON → Form → JSON is lossless and only changes what gets
    /// sent. That asymmetry is deliberate: what you can still see is safe to keep.
    pub fn set_body_type(&mut self, body_type: BodyType, cx: &mut Context<Self>) {
        self.body_type = body_type;
        self.preserved_body = None;
        cx.notify();
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
            RowKind::Form => {
                let row = KeyValueRow::new(true, "", "", "FormCell", cx);
                self.form.push(row);
                self.form.last()
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
        if let Some(ix) = self.query.iter().position(|row| row.is_focused(window, cx)) {
            return Some((RowKind::Query, ix));
        }
        self.form
            .iter()
            .position(|row| row.is_focused(window, cx))
            .map(|ix| (RowKind::Form, ix))
    }

    /// The rows a `RowKind` refers to. One place to add a variant rather than five.
    fn rows_mut(&mut self, kind: RowKind) -> &mut Vec<KeyValueRow> {
        match kind {
            RowKind::Header => &mut self.headers,
            RowKind::Query => &mut self.query,
            RowKind::Form => &mut self.form,
        }
    }

    pub fn toggle_focused_row(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some((kind, ix)) = self.focused_row(window, cx) else {
            return false;
        };
        let rows = self.rows_mut(kind);
        rows[ix].enabled = !rows[ix].enabled;
        cx.notify();
        true
    }

    pub fn remove_focused_row(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some((kind, ix)) = self.focused_row(window, cx) else {
            return false;
        };
        self.rows_mut(kind).remove(ix);
        cx.notify();
        true
    }

    pub fn toggle_row_at(&mut self, kind: RowKind, ix: usize, cx: &mut Context<Self>) {
        let rows = self.rows_mut(kind);
        if let Some(row) = rows.get_mut(ix) {
            row.enabled = !row.enabled;
            cx.notify();
        }
    }

    pub fn remove_row_at(&mut self, kind: RowKind, ix: usize, cx: &mut Context<Self>) {
        let rows = self.rows_mut(kind);
        if ix < rows.len() {
            rows.remove(ix);
            cx.notify();
        }
    }

    pub fn body_label(&self) -> SharedString {
        if let Some(body) = &self.preserved_body {
            return SharedString::from(body.label());
        }
        match self.body_type {
            BodyType::Form => SharedString::from("Form"),
            BodyType::Binary => SharedString::from("Binary"),
            BodyType::Empty | BodyType::Raw => SharedString::from(self.body_kind.label()),
        }
    }

    /// Point a binary body at a file, switching the body type to match.
    pub fn set_binary_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.binary_path = Some(path);
        self.set_body_type(BodyType::Binary, cx);
    }

    /// An explicit `Content-Type` header that disagrees with the body being sent, if there
    /// is one.
    ///
    /// `build.rs` only fills in a derived Content-Type when no explicit header is set, so a
    /// stale header silently wins: switch a request from JSON to Form and the body is
    /// urlencoded while declaring itself JSON. That's a request lying about itself, and the
    /// server rejects or misparses it. Reported rather than rewritten — editing someone's
    /// headers behind their back is worse than telling them.
    pub fn conflicting_content_type(&self, cx: &App) -> Option<(String, &'static str)> {
        let expected = match self.body(cx) {
            Body::Raw { kind, .. } => kind.content_type(),
            Body::Form(_) => "application/x-www-form-urlencoded",
            // Nothing to disagree with. `build.rs` deliberately sends no Content-Type for
            // a binary body — the user is expected to set one — so there is nothing for a
            // header to contradict.
            Body::Empty | Body::Multipart(_) | Body::Binary(_) => return None,
        };

        let declared = self
            .headers
            .iter()
            .filter(|row| row.enabled)
            .find(|row| row.name.read(cx).text().trim().eq_ignore_ascii_case("content-type"))?
            .value
            .read(cx)
            .text()
            .to_string();

        // Compare the essence only: `application/json; charset=utf-8` agrees with JSON.
        let essence = declared.split(';').next().unwrap_or("").trim();
        if essence.eq_ignore_ascii_case(expected) {
            return None;
        }
        Some((declared, expected))
    }

    /// A one-line description of a body the UI can't edit yet, for the pane to show in
    /// place of the editor.
    ///
    /// Counts rather than contents: a multipart field can be a file path or a secret, and
    /// the point here is to prove nothing was lost, not to display it.
    pub fn preserved_body_summary(&self) -> Option<String> {
        let body = self.preserved_body.as_ref()?;
        let detail = match body {
            Body::Form(fields) => format!("{} fields", fields.len()),
            Body::Multipart(fields) => {
                let files = fields
                    .iter()
                    .filter(|field| matches!(field.value, MultipartValue::File(_)))
                    .count();
                match files {
                    0 => format!("{} fields", fields.len()),
                    n => format!("{} fields, {n} from files", fields.len()),
                }
            }
            // Binary is authorable now, so `load` never puts one here. Described anyway
            // rather than reported as nothing: an accessor that says "nothing is held" while
            // something *is* held can't be used to check the invariant, and returning `None`
            // here silently defeated a test asserting exactly that.
            Body::Binary(path) => path.display().to_string(),
            // Genuinely never preserved: both are authorable and representable.
            Body::Raw { .. } | Body::Empty => return None,
        };
        Some(format!("{} · {detail}", body.label()))
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
