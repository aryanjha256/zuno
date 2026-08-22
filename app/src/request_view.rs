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
    Body, Engine, EngineError, Event, FormField, Header, JobId, Method, MultipartField,
    MultipartValue, QueryParam, RawKind, RequestId, RequestSettings, RequestSpec, Resolver,
    ResponseData, ResponseDiff,
};

/// Flip `enabled` on a row, reporting whether the index existed.
fn flip_enabled(rows: &mut [KeyValueRow], ix: usize) -> bool {
    match rows.get_mut(ix) {
        Some(row) => {
            row.enabled = !row.enabled;
            true
        }
        None => false,
    }
}

/// Which body a request sends.
///
/// Covers every `Body` variant, which is what let `RequestView::preserved_body` go: there is
/// no longer a body the UI can hold but not author, so `load`'s match is exhaustive and
/// adding a `Body` variant is a compile error until someone decides how to edit it. That's
/// stronger than the catch-all it replaced, which silently preserved the unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyType {
    Empty,
    Raw,
    Form,
    /// The contents of a file, sent as-is.
    Binary,
    /// Mixed text and file parts.
    Multipart,
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
    ///
    /// **It drops itself, and that only works because the terminal event also ends the loop.**
    /// `apply`'s `Done` and `Failed` arms set `inflight = None`, which drops this field — the task
    /// currently executing that very closure. The remaining statements in the arm still run,
    /// because a `Task` dropped while running is cancelled only after the current poll returns, and
    /// by then the consuming future has already `break`ed and completed. Make a terminal event
    /// return `true` instead, or add work after the `while` loop in `send`, and that work silently
    /// never happens: the future would go back to awaiting a channel nothing will poll again.
    _task: Task<()>,
}

/// One part of a multipart body: a key-value row plus whether its value is a file path.
///
/// A wrapper rather than a third `TextInput`, so the name and value cells behave exactly
/// like every other table's — the only difference is how the value is *interpreted*.
pub struct MultipartRow {
    pub row: KeyValueRow,
    pub is_file: bool,
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

/// Which half of a response the pane shows.
///
/// Split into tabs because the headers table is unbounded and the pane clips: a response
/// with two dozen headers pushed the body region off the bottom edge, and with no scroll
/// anywhere in the pane the body was not merely small but *unreachable*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponseView {
    /// The default, because the body is the answer you sent the request to get.
    #[default]
    Body,
    Headers,
}

/// Which table a row belongs to. Used by the row actions to find their target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Header,
    Query,
    /// A field of an `application/x-www-form-urlencoded` body.
    Form,
    /// A part of a `multipart/form-data` body.
    Multipart,
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
    /// Parts of a multipart body.
    pub multipart: Vec<MultipartRow>,
    /// The file a binary body sends.
    ///
    /// Only the path is held — the bytes are read at the send boundary by `build.rs`, so a
    /// file edited between sends goes out in its new state, and a 2GB upload never sits in
    /// this process's memory. A missing file surfaces as `BodyFileUnreadable` rather than
    /// being checked here, which would mean a filesystem call on every frame.
    pub binary_path: Option<PathBuf>,
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
    /// Body or headers. A *view* preference rather than response state, which is why
    /// nothing resets it — not a new response, and not `load`. Switching to Headers to
    /// watch a `set-cookie` across sends is the reason to be there, and snapping back to
    /// Body on arrival would undo the thing you were doing.
    pub response_view: ResponseView,
    /// The indexed body. `None` while it's still being built off-thread.
    pub body_view: Option<BodyView>,
    body_task: Option<Task<()>>,
    /// Holding the diff task is what keeps it alive, and replacing it is what makes a
    /// superseded diff harmless — see `diff_against`.
    diff_task: Option<Task<()>>,
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
            multipart: Vec::new(),
            binary_path: None,
            settings: RequestSettings::default(),
            response: None,
            diff: None,
            history: Vec::new(),
            viewing: 0,
            response_view: ResponseView::default(),
            body_view: None,
            body_task: None,
            diff_task: None,
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

        // Exhaustive on purpose — no catch-all. Every `Body` variant has an editor now, so
        // adding one has to fail the build rather than be silently set aside. The catch-all
        // this replaced was itself a fix for `load` *dropping* non-raw bodies, which a save
        // then wrote to disk.
        let (body_text, body_kind, body_type) = match &spec.body {
            Body::Raw { text, kind } => (text.clone(), *kind, BodyType::Raw),
            // Empty stays editable: typing into it is how you get a raw body.
            Body::Empty => (String::new(), RawKind::Json, BodyType::Empty),
            Body::Form(_) => (String::new(), RawKind::Json, BodyType::Form),
            Body::Binary(_) => (String::new(), RawKind::Json, BodyType::Binary),
            Body::Multipart(_) => (String::new(), RawKind::Json, BodyType::Multipart),
        };

        let binary_path = match &spec.body {
            Body::Binary(path) => Some(path.clone()),
            _ => None,
        };

        let multipart = match &spec.body {
            Body::Multipart(fields) => fields
                .iter()
                .map(|field| {
                    let (text, is_file) = match &field.value {
                        MultipartValue::Text(text) => (text.clone(), false),
                        MultipartValue::File(path) => (path.display().to_string(), true),
                    };
                    MultipartRow {
                        row: KeyValueRow::new(field.enabled, &field.name, &text, "PartCell", cx),
                        is_file,
                    }
                })
                .collect(),
            _ => Vec::new(),
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
        self.multipart = multipart;
        self.settings = spec.settings;

        // A different request has no relationship to the last one's response.
        self.response = None;
        self.diff = None;
        self.history.clear();
        self.viewing = 0;
        self.body_view = None;
        self.body_task = None;
        self.diff_task = None;
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

    /// Swap between the body and the headers.
    ///
    /// Per-buffer, so switching tabs doesn't carry the choice with it — the pane belongs to
    /// the buffer, and two requests being read for different reasons is the normal case.
    pub fn toggle_response_view(&mut self, cx: &mut Context<Self>) {
        self.response_view = match self.response_view {
            ResponseView::Body => ResponseView::Headers,
            ResponseView::Headers => ResponseView::Body,
        };
        cx.notify();
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
            BodyType::Multipart => Body::Multipart(
                self.multipart
                    .iter()
                    .map(|part| {
                        let text = part.row.value.read(cx).text().to_string();
                        MultipartField {
                            enabled: part.row.enabled,
                            name: part.row.name.read(cx).text().to_string(),
                            value: if part.is_file {
                                MultipartValue::File(PathBuf::from(text))
                            } else {
                                MultipartValue::Text(text)
                            },
                        }
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
    /// Nothing is discarded: the editor's text, the form rows, the multipart parts, and the
    /// binary path all stay put, so switching JSON → Form → JSON round-trips and only what
    /// gets *sent* changes. A mistaken type change is therefore never destructive.
    pub fn set_body_type(&mut self, body_type: BodyType, cx: &mut Context<Self>) {
        self.body_type = body_type;
        cx.notify();
    }

    /// Point a multipart part at a file, marking it a file part.
    pub fn set_multipart_file(&mut self, ix: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(part) = self.multipart.get_mut(ix) else {
            return;
        };
        part.is_file = true;
        // `TextInput` has no setter — it owns its text — so the cell is rebuilt, exactly as
        // `load` rebuilds every input. Focus moves off the cell, which is fine: the dialog
        // already took it.
        let text = path.display().to_string();
        part.row.value = cx.new(|cx| TextInput::new(text, "path", "PartCell", cx));
        cx.notify();
    }

    /// The multipart part containing focus, if the body is multipart.
    ///
    /// Lets one "choose a file" verb serve both bodies: with a part focused it fills that
    /// part, otherwise it sets the whole binary body.
    pub fn focused_multipart_row(&self, window: &Window, cx: &App) -> Option<usize> {
        if self.body_type != BodyType::Multipart {
            return None;
        }
        self.multipart
            .iter()
            .position(|part| part.row.is_focused(window, cx))
    }

    pub fn url_focus(&self, cx: &App) -> FocusHandle {
        self.url.read(cx).focus_handle(cx)
    }

    pub fn body_focus(&self, cx: &App) -> FocusHandle {
        self.body_editor.read(cx).focus_handle(cx)
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

                // The run this one replaces is the diff baseline, and then becomes history.
                let previous = self.response.take();
                // Cleared rather than left stale: it described a comparison that no longer holds,
                // and the replacement arrives from a background task a frame or two later.
                self.diff = None;
                // Cloned before the move, and cheap for it: `Bytes` is refcounted, so this copies
                // a status line and a header list, not a body.
                let current = (*response).clone();

                self.response = Some(*response);
                // Back to live: a fresh response arriving while you're reading an old one
                // must not leave you staring at the old one with no sign anything happened.
                self.viewing = 0;

                if let Some(previous) = previous {
                    self.diff_against(previous.clone(), current, cx);
                    self.history.insert(0, previous);
                    self.history.truncate(HISTORY_LIMIT);
                }

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

    /// Compare this response with the one it replaced, **on a background thread**.
    ///
    /// `ResponseDiff::between` compares both bodies byte-for-byte and counts the newlines in each,
    /// so on two 10MB responses it is tens of megabytes of scanning — and it was doing it in the
    /// same frame that then has to lay the pane out and repaint it, three lines above an
    /// `index_body` call that goes off-thread for exactly this reason. Invariant 3 applies to a
    /// diff as much as to an index; this was the last piece of response handling still inline.
    ///
    /// The consequence is that `diff` is `None` for a frame or two after a response lands, which
    /// is the same deal the body index has always had, and `response_pane` already renders a
    /// missing diff as simply no diff bar.
    ///
    /// Holding the task in a field is what makes a superseded diff harmless: assigning a new one
    /// drops the old, and dropping a `Task` cancels it, so a late result can never land on top of
    /// a newer response.
    fn diff_against(
        &mut self,
        previous: ResponseData,
        current: ResponseData,
        cx: &mut Context<Self>,
    ) {
        let compute = cx
            .background_executor()
            .spawn(async move { ResponseDiff::between(&previous, &current) });

        self.diff_task = Some(cx.spawn(async move |this, cx| {
            let diff = compute.await;
            let _ = this.update(cx, |this, cx| {
                this.diff = Some(diff);
                cx.notify();
            });
        }));
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
            RowKind::Multipart => {
                self.multipart.push(MultipartRow {
                    row: KeyValueRow::new(true, "", "", "PartCell", cx),
                    is_file: false,
                });
                self.multipart.last().map(|part| &part.row)
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
        if let Some(ix) = self.form.iter().position(|row| row.is_focused(window, cx)) {
            return Some((RowKind::Form, ix));
        }
        self.multipart
            .iter()
            .position(|part| part.row.is_focused(window, cx))
            .map(|ix| (RowKind::Multipart, ix))
    }

    /// Flip a row's `enabled` flag. Multipart parts wrap their row, so this can't hand back
    /// a single `&mut Vec<KeyValueRow>` for every kind.
    fn toggle(&mut self, kind: RowKind, ix: usize) -> bool {
        match kind {
            RowKind::Header => flip_enabled(&mut self.headers, ix),
            RowKind::Query => flip_enabled(&mut self.query, ix),
            RowKind::Form => flip_enabled(&mut self.form, ix),
            RowKind::Multipart => match self.multipart.get_mut(ix) {
                Some(part) => {
                    part.row.enabled = !part.row.enabled;
                    true
                }
                None => false,
            },
        }
    }

    fn remove(&mut self, kind: RowKind, ix: usize) -> bool {
        let len = match kind {
            RowKind::Header => self.headers.len(),
            RowKind::Query => self.query.len(),
            RowKind::Form => self.form.len(),
            RowKind::Multipart => self.multipart.len(),
        };
        if ix >= len {
            return false;
        }
        match kind {
            RowKind::Header => drop(self.headers.remove(ix)),
            RowKind::Query => drop(self.query.remove(ix)),
            RowKind::Form => drop(self.form.remove(ix)),
            RowKind::Multipart => drop(self.multipart.remove(ix)),
        }
        true
    }

    pub fn toggle_focused_row(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some((kind, ix)) = self.focused_row(window, cx) else {
            return false;
        };
        if self.toggle(kind, ix) {
            cx.notify();
        }
        true
    }

    pub fn remove_focused_row(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some((kind, ix)) = self.focused_row(window, cx) else {
            return false;
        };
        if self.remove(kind, ix) {
            cx.notify();
        }
        true
    }

    pub fn toggle_row_at(&mut self, kind: RowKind, ix: usize, cx: &mut Context<Self>) {
        if self.toggle(kind, ix) {
            cx.notify();
        }
    }

    pub fn remove_row_at(&mut self, kind: RowKind, ix: usize, cx: &mut Context<Self>) {
        if self.remove(kind, ix) {
            cx.notify();
        }
    }

    /// How the body type reads on screen, and what `open_body_type` compares against to mark
    /// the current row.
    ///
    /// **`Empty` reports "None", not the retained raw sub-kind.** Folding the two together
    /// meant a body-less request advertised "JSON" on the pane's chip while the pane beside it
    /// read "No body" — and since the picker marks its current row by comparing this string
    /// against the row labels, it marked *JSON* as current on every fresh buffer and could
    /// never mark None. The string has to stay equal to the picker's own "None" label.
    pub fn body_label(&self) -> SharedString {
        match self.body_type {
            BodyType::Empty => SharedString::from("None"),
            BodyType::Form => SharedString::from("Form"),
            BodyType::Binary => SharedString::from("Binary"),
            BodyType::Multipart => SharedString::from("Multipart"),
            BodyType::Raw => SharedString::from(self.body_kind.label()),
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
