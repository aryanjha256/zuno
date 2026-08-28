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
    ScrollStrategy, SharedString, Styled, Subscription, Task, UniformListScrollHandle, Window, div,
    px,
};
use zuno_core::{
    Body, Engine, EngineError, Event, FormField, Header, Hits, JobId, Method, MultipartField,
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
use crate::input::text_input::Changed;
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

/// Which section of the request the pane shows.
///
/// Headers, query and body used to stack, so the two you weren't editing still cost a header
/// row and an empty-state row each — about 130px to say "nothing here". Tabbed, they cost one
/// strip, and the body editor gets the pane's full height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestTab {
    Headers,
    /// Labelled "Params" but named Query throughout the code, matching `RowKind::Query` and
    /// `RequestSpec::query` — that serde field is in every saved collection file, so renaming
    /// it would break them with `missing field query`.
    Query,
    /// Default: authoring a body is where the time goes.
    #[default]
    Body,
}

impl RequestTab {
    /// Visual order, which is also cycle order — deliberately not most-recently-used. With
    /// three tabs in a fixed strip, MRU sends the same keystroke somewhere different each time
    /// and throws away the muscle memory the strip gives for free.
    pub const ALL: [RequestTab; 3] = [RequestTab::Headers, RequestTab::Query, RequestTab::Body];

    fn step(self, delta: isize) -> Self {
        let at = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0) as isize;
        let len = Self::ALL.len() as isize;
        Self::ALL[(at + delta).rem_euclid(len) as usize]
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

/// The find bar's state. Present only while the bar is open.
///
/// Shared by the response find bar and the request body's.
///
/// **`rows` means "which display line" in whichever surface owns this** — outline rows for the
/// response, editor lines for the body. Same purpose either way: what to scroll to and what to
/// highlight. Renamed from `ResponseSearch` when the body got a bar of its own; leaving the old
/// name would have made every reader think the request side had its own copy of this logic.
///
/// An `Option<TextSearch>` rather than a `bool` plus fields on `RequestView`, so "closed"
/// cannot carry stale matches — and so the query input and its subscription are created and
/// dropped together.
pub struct TextSearch {
    pub query: Entity<TextInput>,
    /// Byte offsets of every match, ascending. Empty means the query matched nothing, which is
    /// different from the bar being closed.
    pub offsets: Vec<u32>,
    /// The row each match falls in, parallel to `offsets`.
    pub rows: Vec<u32>,
    /// Which match is current, as an index into `offsets`. Meaningless while it's empty.
    pub current: usize,
    /// The scan stopped at `search::MAX_MATCHES` with body left unscanned.
    pub truncated: bool,
    /// The current match sits past the raw view's per-line display cut, so its row is on
    /// screen but the match itself isn't. Says so rather than looking broken.
    pub current_clipped: bool,
    /// The replacement text, for the body's bar. `None` on the response, which is read-only —
    /// an `Option` rather than an unused input, so the read-only surface cannot grow a
    /// replace box by accident.
    pub replace: Option<Entity<TextInput>>,
    /// Held, not detached: dropping a `Subscription` unsubscribes.
    _query_changed: Subscription,
}

impl TextSearch {
    /// `1 of 47`, or nothing when there is no match to number.
    pub fn position(&self) -> Option<(usize, usize)> {
        (!self.offsets.is_empty()).then(|| (self.current + 1, self.offsets.len()))
    }
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

/// How far one `left`/`right` press moves the response body.
///
/// Roughly ten monospace characters at the viewer's text size: small enough to land where you
/// meant, large enough that crossing a long token isn't a drum solo.
const H_SCROLL_STEP: f32 = 70.;

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
    /// Private, and set only through `set_body_kind`. The editor's colouring is derived from it,
    /// and a public field is how those two drift: assigning it directly compiles and silently
    /// leaves JSON text painted as plain, or XML painted as JSON.
    body_kind: RawKind,
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
    /// Sticky per buffer, like `response_view`: two requests are open for different reasons.
    pub request_tab: RequestTab,
    /// The indexed body. `None` while it's still being built off-thread.
    pub body_view: Option<BodyView>,
    body_task: Option<Task<()>>,
    /// The find bar, when open.
    pub search: Option<TextSearch>,
    /// The request body's own find bar. Separate from `search` because both can be open at
    /// once — you can be hunting for a field in what you are sending *and* in what came back.
    pub body_search: Option<TextSearch>,
    /// Holding it keeps it alive; replacing it cancels a superseded scan, the same contract as
    /// `diff_task`. Typing fast enough to outrun a 10MB scan is exactly when that matters.
    search_task: Option<Task<()>>,
    /// Shared by the JSON and raw lists — they are never on screen together — so that a jump
    /// to a match can scroll the one that is. `uniform_list` needs the handle at render time,
    /// which is why it lives here rather than in `BodyView`.
    pub body_scroll: UniformListScrollHandle,
    /// The headers tab's scroll state. Tracked so the tab can scroll *sideways* — header values
    /// routinely exceed the pane, and until this existed the cell was told to shrink and clip.
    pub headers_scroll: gpui::ScrollHandle,
    /// Where the last right-click in the body landed, in window coordinates.
    ///
    /// The menu itself is owned by `Workspace`, beside the picker and the settings panel — it
    /// has to be, or `modal_open` cannot see it and the response pane's `overflow_hidden`
    /// clips it. But an action carries no payload without pulling `schemars` in for a derived
    /// `Action`, so the position is parked here and `take`n by the handler. Consumed on read,
    /// so a stale anchor can never place a later menu.
    menu_anchor: Option<gpui::Point<gpui::Pixels>>,
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
            request_tab: RequestTab::default(),
            body_view: None,
            body_task: None,
            search: None,
            body_search: None,
            search_task: None,
            body_scroll: UniformListScrollHandle::new(),
            headers_scroll: gpui::ScrollHandle::new(),
            menu_anchor: None,
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
        self.set_body_kind(body_kind, cx);
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
        // A find bar left open over a request that has been replaced would show a count for a
        // body that no longer exists. Its input entity goes with it, and `load` is not a focus
        // move — curl import lands in a new buffer — so there is nothing to restore focus to.
        self.search = None;
        self.body_search = None;
        self.search_task = None;

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
    pub fn cycle_request_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.request_tab = self.request_tab.step(delta);
        cx.notify();
    }

    /// Reveal a section. Called by the verbs that act on one — adding a header while the
    /// Headers tab is hidden would otherwise put a row somewhere you can't see, which reads
    /// as the keystroke having done nothing.
    pub fn show_request_tab(&mut self, tab: RequestTab, cx: &mut Context<Self>) {
        if self.request_tab != tab {
            self.request_tab = tab;
            cx.notify();
        }
    }

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

    /// The handle `FocusBody` should move focus to, or `None` when this body has nothing to
    /// type into.
    ///
    /// **It has to be a handle that is actually painted, and that is the whole point of this
    /// existing.** `body_focus` hands back the editor's, and the editor is only rendered for a
    /// raw body — a `FocusHandle` belongs to the entity that made it whether or not it is on
    /// screen, so `Ctrl+B` on a form body focused a handle with no element. Action dispatch
    /// travels *up the focus tree*, so with no element there is no path to `Workspace` and every
    /// binding stops resolving: `Ctrl+L` did nothing, and typing went nowhere. The keyboard was
    /// dead until you clicked something, with nothing on screen to explain it.
    ///
    /// Exhaustive with no catch-all: a new `Body` variant has to say where focus goes rather than
    /// inheriting a handle that might not be rendered.
    pub fn body_focus_target(&self, cx: &App) -> Option<FocusHandle> {
        match self.body_type {
            BodyType::Raw => Some(self.body_focus(cx)),
            BodyType::Form => self
                .form
                .first()
                .map(|row| row.name.read(cx).focus_handle(cx)),
            BodyType::Multipart => self
                .multipart
                .first()
                .map(|part| part.row.name.read(cx).focus_handle(cx)),
            // A path you click and a sentence: there is no input to land on.
            BodyType::Binary | BodyType::Empty => None,
        }
    }

    /// Whether anything inside the body region holds focus.
    ///
    /// **Not the same question as `body_focus`**, which hands back the *editor's* handle. The
    /// editor is only painted for `BodyType::Raw`, and a handle belongs to the entity that made
    /// it whether or not it is on screen — so a focus ring keyed on `body_focus` stayed grey
    /// while you were plainly editing a form field. Form and multipart rows own their own inputs,
    /// so there is no single handle to ask and this has to poll them.
    ///
    /// Matched exhaustively with no catch-all, like `load`: a new `Body` variant should not
    /// silently inherit "never focused".
    pub fn body_region_focused(&self, window: &Window, cx: &App) -> bool {
        match self.body_type {
            BodyType::Raw => self.body_focus(cx).is_focused(window),
            BodyType::Form => self.form.iter().any(|row| row.is_focused(window, cx)),
            BodyType::Multipart => {
                self.multipart.iter().any(|part| part.row.is_focused(window, cx))
            }
            // Neither has anything focusable: a binary body is a path you click, and an empty
            // one is a sentence.
            BodyType::Binary | BodyType::Empty => false,
        }
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
                // Matches belong to the bytes they were found in. A resend, or picking a run
                // out of the history browser, replaces those bytes — so offsets from the old
                // body would point into the new one at random, and the count would describe a
                // response that is no longer on screen. Re-scan instead of clearing, because
                // the query is still what the user wants to know about.
                if this.is_searching() {
                    this.run_search(cx);
                }
                cx.notify();
            });
        }));
    }

    // ---- request body search and replace -------------------------------------

    /// Open the body's find bar, or refocus it if it is already open.
    ///
    /// Reveals the Body tab first, for the same reason the response bar switches to the Body
    /// view: a find bar that appears over a section you cannot see reads as doing nothing.
    pub fn open_body_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_tab = RequestTab::Body;

        if self.body_search.is_none() {
            let query = cx.new(|cx| {
                TextInput::new(String::new(), "Find in body…", "BodySearch", cx)
            });
            let replace = cx.new(|cx| {
                TextInput::new(String::new(), "Replace with…", "BodySearch", cx)
            });
            let query_changed = cx.subscribe(&query, |this: &mut Self, _, _: &Changed, cx| {
                this.run_body_search(cx);
            });

            self.body_search = Some(TextSearch {
                query,
                replace: Some(replace),
                offsets: Vec::new(),
                rows: Vec::new(),
                current: 0,
                truncated: false,
                current_clipped: false,
                _query_changed: query_changed,
            });
        }

        if let Some(search) = &self.body_search {
            let handle = search.query.read(cx).focus_handle(cx);
            window.focus(&handle);
            search.query.update(cx, |input, cx| input.select_all_text(cx));
        }
        self.run_body_search(cx);
        cx.notify();
    }

    /// Close it, putting focus back in the editor rather than leaving it on a dropped input.
    pub fn close_body_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.body_search.take().is_some() {
            let handle = self.body_editor.read(cx).focus_handle(cx);
            window.focus(&handle);
            cx.notify();
        }
    }

    /// Test-only: the render path reads `body_search` itself, so nothing in the UI asks this.
    /// `is_searching` has a real caller and so is not gated the same way.
    #[cfg(test)]
    pub fn is_searching_body(&self) -> bool {
        self.body_search.is_some()
    }

    /// Re-scan the body for the current query.
    ///
    /// **On the UI thread, unlike the response scan**, and deliberately: the response can be
    /// 100MB (invariant 3 is not conditional on today's body being small), while a request body
    /// is hand-authored — the same argument §7 makes for dropping the rope. Spawning a task per
    /// keystroke to search a few kilobytes would cost more than the search.
    pub fn run_body_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.body_search else { return };
        let query = search.query.read(cx).text().to_string();
        let content = self.body_editor.read(cx).text().to_string();

        let hits = if query.is_empty() {
            zuno_core::search::Hits::default()
        } else {
            zuno_core::search::find(content.as_bytes(), &query)
        };

        // Which line each match falls in, so the bar can scroll to it and the editor can paint
        // it. `rows` means display lines here — see `TextSearch`.
        let rows = self.body_editor.read(cx).lines_for_offsets(&hits.offsets);

        let Some(search) = self.body_search.as_mut() else { return };
        search.offsets = hits.offsets;
        search.rows = rows;
        search.truncated = hits.truncated;
        search.current = 0;

        // **Reveal immediately, the way the response's `apply_search` does.** Without this the
        // first match is found but not shown: nothing is selected and nothing scrolls until you
        // press Enter, which for a single match means pressing Enter to go to the match you are
        // already on.
        self.reveal_body_match(cx);
        cx.notify();
    }

    /// Move to another match, wrapping, and put the caret on it.
    ///
    /// Moving the caret is what makes this an *editor* find rather than a viewer's: it is where
    /// typing resumes, it is what `ReplaceNext` acts on, and it drags the horizontal scroll to
    /// the match for free through the caret-following clamp.
    pub fn step_body_search(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(search) = self.body_search.as_mut() else { return };
        if search.offsets.is_empty() {
            return;
        }

        let count = search.offsets.len() as isize;
        search.current = (search.current as isize + delta).rem_euclid(count) as usize;
        self.reveal_body_match(cx);
    }

    /// Select the current match in the editor.
    fn reveal_body_match(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.body_search.as_ref() else { return };
        let Some(&start) = search.offsets.get(search.current) else { return };
        let len = search.query.read(cx).text().len();

        self.body_editor.update(cx, |editor, cx| {
            editor.select_range(start as usize, start as usize + len, cx);
        });
        cx.notify();
    }

    /// Replace the current match and move to the next.
    ///
    /// Returns how many were replaced, so the caller can say so — silence after a replace is
    /// indistinguishable from a replace that found nothing.
    pub fn replace_current(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let Some(search) = self.body_search.as_ref() else { return 0 };
        let Some(&start) = search.offsets.get(search.current) else { return 0 };
        let Some(replace) = search.replace.as_ref() else { return 0 };

        let with = replace.read(cx).text().to_string();
        let len = search.query.read(cx).text().len();

        self.body_editor.update(cx, |editor, cx| {
            editor.replace_range(start as usize..start as usize + len, &with, window, cx);
        });
        // The offsets after this one have all shifted, so re-scan rather than patch them. A few
        // kilobytes is cheaper than the bookkeeping to keep them correct.
        self.run_body_search(cx);
        self.reveal_body_match(cx);
        1
    }

    /// Replace every match, last one first.
    ///
    /// **Backwards on purpose:** replacing from the front invalidates every offset after the one
    /// just written the moment the replacement is a different length. Going from the end means
    /// each splice only moves text the loop has already passed.
    pub fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let Some(search) = self.body_search.as_ref() else { return 0 };
        let Some(replace) = search.replace.as_ref() else { return 0 };

        let with = replace.read(cx).text().to_string();
        let len = search.query.read(cx).text().len();
        if len == 0 {
            return 0;
        }
        let offsets = search.offsets.clone();

        let ranges: Vec<_> = offsets
            .iter()
            .rev()
            .map(|start| *start as usize..*start as usize + len)
            .collect();
        self.body_editor.update(cx, |editor, cx| {
            editor.replace_ranges(&ranges, &with, window, cx);
        });

        self.run_body_search(cx);
        offsets.len()
    }

    // ---- response search ----------------------------------------------------

    /// Open the find bar, or refocus it if it's already open.
    ///
    /// Refocus rather than close, because `Ctrl+F` while the bar is open but focus has moved
    /// elsewhere means "put me back in the search box", not "throw away my query". Selecting
    /// the existing text is what makes retyping over it the default.
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Searching applies to the body, so being on the Headers tab and pressing Ctrl+F means
        // you want the body. Switching is less surprising than a find bar that appears to do
        // nothing.
        self.response_view = ResponseView::Body;

        if self.search.is_none() {
            let query = cx.new(|cx| TextInput::new(String::new(), "Find in response…", "ResponseSearch", cx));
            let query_changed = cx.subscribe(&query, |this: &mut Self, _, _: &Changed, cx| {
                this.run_search(cx);
            });

            self.search = Some(TextSearch {
                query,
                replace: None,
                offsets: Vec::new(),
                rows: Vec::new(),
                current: 0,
                truncated: false,
                current_clipped: false,
                _query_changed: query_changed,
            });
        }

        if let Some(search) = &self.search {
            let handle = search.query.read(cx).focus_handle(cx);
            window.focus(&handle);
            search.query.update(cx, |input, cx| input.select_all_text(cx));
        }
        cx.notify();
    }

    /// Close the bar and put focus back where the response pane can use it.
    ///
    /// Focus has to move: the handle belongs to the input entity being dropped, and leaving it
    /// there means no key context matches and the whole keymap goes quiet — the same failure as
    /// switching buffers without moving focus.
    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            self.search_task = None;
            window.focus(&self.response_focus);
            cx.notify();
        }
    }

    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    /// Re-scan the body for the current query, off-thread.
    ///
    /// Called from the input's `Changed` subscription, so it runs once per edit rather than
    /// once per frame. 10MB takes ~7ms for a query that matches nothing — comfortably inside a
    /// frame, and still off the UI thread, because the transfer cap is 100MB and invariant 3
    /// isn't conditional on the body being small today.
    pub fn run_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.search else { return };
        let query = search.query.read(cx).text().to_string();

        // Bytes and the index are behind `Arc`/`Bytes`, so this is a refcount bump each.
        let source = self
            .body_view
            .as_ref()
            .and_then(|body| body.searchable_source().cloned());

        let Some(source) = source else {
            self.apply_search(Hits::default(), cx);
            return;
        };

        let scan = cx
            .background_executor()
            .spawn(async move { zuno_core::search::find(&source, &query) });

        self.search_task = Some(cx.spawn(async move |this, cx| {
            let hits = scan.await;
            let _ = this.update(cx, |this, cx| this.apply_search(hits, cx));
        }));
    }

    /// Store a finished scan and jump to its first match.
    ///
    /// The offset-to-row mapping happens here, on the UI thread, and deliberately: it needs the
    /// live `BodyView`, which the background task cannot borrow, and it is a merge over at most
    /// `MAX_MATCHES` offsets — 148µs against 1.31M rows, measured. The *scan* is the O(bytes)
    /// half and that's what went to the executor.
    fn apply_search(&mut self, hits: Hits, cx: &mut Context<Self>) {
        let rows = self
            .body_view
            .as_ref()
            .map(|body| body.rows_for_offsets(&hits.offsets))
            .unwrap_or_default();

        let Some(search) = self.search.as_mut() else { return };
        search.offsets = hits.offsets;
        search.rows = rows;
        search.truncated = hits.truncated;
        search.current = 0;
        search.current_clipped = false;

        self.reveal_current_match(cx);
        cx.notify();
    }

    /// Move to another match, wrapping at both ends.
    pub fn step_search(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else { return };
        if search.offsets.is_empty() {
            return;
        }

        let count = search.offsets.len() as isize;
        // `rem_euclid` so stepping back from the first match wraps to the last rather than
        // underflowing a usize — the same reason `Picker::select` uses it.
        search.current = (search.current as isize + delta).rem_euclid(count) as usize;

        self.reveal_current_match(cx);
        cx.notify();
    }

    /// Unfold and scroll so the current match is on screen, and record whether it's readable.
    fn reveal_current_match(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_ref() else { return };
        let Some(&row) = search.rows.get(search.current) else { return };
        let Some(&offset) = search.offsets.get(search.current) else { return };

        let Some(body) = self.body_view.as_mut() else { return };
        let visible = body.reveal(row as usize);
        let clipped = !body.offset_is_displayed(offset);

        if let Some(visible_ix) = visible {
            // Centred rather than Top: a match at the very top of the viewport with no
            // surrounding context is hard to place in a large document.
            self.body_scroll
                .scroll_to_item(visible_ix, ScrollStrategy::Center);
        }

        if let Some(search) = self.search.as_mut() {
            search.current_clipped = clipped;
        }
        cx.notify();
    }

    /// The current match's byte range in the response *source*.
    ///
    /// The row alone was enough while a match tinted its whole row; highlighting the matched
    /// characters needs to know which ones, and each surface maps this range into its own
    /// rendered text.
    pub fn current_match_bytes(&self, cx: &App) -> Option<std::ops::Range<u32>> {
        let search = self.search.as_ref()?;
        let start = *search.offsets.get(search.current)?;
        let len = search.query.read(cx).text().len() as u32;
        (len > 0).then(|| start..start + len)
    }

    /// The row currently highlighted as the active match, if the bar is open.
    pub fn current_match_row(&self) -> Option<u32> {
        let search = self.search.as_ref()?;
        search.rows.get(search.current).copied()
    }

    /// The selected body row, for the highlight.
    pub fn selected_body_row(&self) -> Option<u32> {
        self.body_view.as_ref()?.selected()
    }

    /// Step the selection through the body, scrolling to keep it on screen.
    ///
    /// Only moves focus's *content*, never focus itself — the pane already has focus when
    /// these keys resolve, since the binding is scoped to its context.
    pub fn move_body_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(body) = self.body_view.as_mut() else { return };
        let Some(visible_ix) = body.move_selection(delta) else {
            return;
        };

        // The strategy follows the direction of travel, and there is no `Nearest` to reach
        // for: `scroll_to_item` skips scrolling entirely while the row is on screen, but once
        // it isn't, it *does* apply the strategy — so a fixed `Top` would fling the viewport a
        // whole page whenever you stepped off the bottom edge. Matching the strategy to the
        // direction makes both edges scroll by exactly the row that went out of view.
        let strategy = if delta > 0 {
            ScrollStrategy::Bottom
        } else {
            ScrollStrategy::Top
        };
        self.body_scroll.scroll_to_item(visible_ix, strategy);
        cx.notify();
    }

    /// Select the row drawn at `visible_ix`.
    ///
    /// **Focus is not moved here, and that is checked rather than assumed.** Clicking a row
    /// while the URL bar has focus has to leave the keyboard able to continue, or the
    /// selection the click just made refuses to move — but the pane's own `track_focus`
    /// already does it: `Interactivity::paint` registers a Bubble-phase mouse listener that
    /// focuses the tracked handle on any hit inside the element. An explicit `window.focus`
    /// here was the first version and it was dead code, which
    /// `clicking_a_row_takes_focus_so_the_keyboard_can_carry_on` proved by passing without it.
    /// The corollary is in `json_row`: anything nested that stops propagation suppresses that
    /// listener too.
    pub fn select_body_row_at(&mut self, visible_ix: usize, cx: &mut Context<Self>) {
        let Some(body) = self.body_view.as_mut() else { return };
        if body.select_visible(visible_ix).is_none() {
            return;
        }

        cx.notify();
    }

    /// Test-only, like `Workspace::tab_count`: nothing in the UI reads the kind directly, it
    /// reads the label derived from it.
    #[cfg(test)]
    pub fn body_kind(&self) -> RawKind {
        self.body_kind
    }

    /// Set the raw body's flavour, and the editor's colouring with it.
    ///
    /// One funnel, for the reason `Workspace::activate` is one: the two live in different
    /// entities, so keeping them in step at each call site is a rule to remember rather than a
    /// thing that cannot be got wrong.
    pub fn set_body_kind(&mut self, kind: RawKind, cx: &mut Context<Self>) {
        self.body_kind = kind;
        let json = matches!(kind, RawKind::Json);
        self.body_editor
            .update(cx, |editor, cx| editor.set_highlight_json(json, cx));
        cx.notify();
    }

    pub fn set_menu_anchor(&mut self, at: gpui::Point<gpui::Pixels>) {
        self.menu_anchor = Some(at);
    }

    pub fn take_menu_anchor(&mut self) -> Option<gpui::Point<gpui::Pixels>> {
        self.menu_anchor.take()
    }

    pub fn selected_is_container(&self) -> bool {
        self.body_view
            .as_ref()
            .is_some_and(|body| body.selected_is_container())
    }

    pub fn selected_is_folded(&self) -> bool {
        self.body_view
            .as_ref()
            .is_some_and(|body| body.selected_is_folded())
    }

    pub fn toggle_selected_fold(&mut self, cx: &mut Context<Self>) {
        if let Some(body) = self.body_view.as_mut() {
            body.toggle_selected_fold();
            cx.notify();
        }
    }

    /// Scroll the body sideways by `steps` of `H_SCROLL_STEP`.
    ///
    /// **Offsets run negative as you scroll right** — gpui's convention, stated on
    /// `ScrollHandle::set_offset`.
    ///
    /// **Deliberately does not clamp**, which is the opposite of the obvious code. `set_offset`
    /// writes without checking, so an explicit clamp here looks required — but gpui re-clamps
    /// `scroll_offset.x` to `[-scroll_max.width, 0]` on every `interactivity.prepaint`, using a
    /// maximum computed from the content as it is *now*. A clamp here would duplicate that
    /// against a `max_offset` recorded by the previous frame, which is the staler of the two.
    /// Tried, and no test or eye could tell the difference.
    pub fn scroll_body_horizontally(&mut self, steps: f32, cx: &mut Context<Self>) {
        let handle = self.body_scroll.0.borrow().base_handle.clone();
        let mut offset = handle.offset();

        offset.x -= px(steps * H_SCROLL_STEP);
        handle.set_offset(offset);
        cx.notify();
    }

    /// Back to column zero. `Home` rather than a long press on `left`, for the same reason
    /// every editor has one.
    pub fn scroll_body_to_start(&mut self, cx: &mut Context<Self>) {
        let handle = self.body_scroll.0.borrow().base_handle.clone();
        let mut offset = handle.offset();
        offset.x = px(0.);
        handle.set_offset(offset);
        cx.notify();
    }

    /// The selected row's value and path, for the copy verbs.
    pub fn selected_body_value(&self) -> Option<String> {
        self.body_view.as_ref()?.selected_value()
    }

    pub fn selected_body_path(&self) -> Option<String> {
        self.body_view.as_ref()?.selected_path()
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
