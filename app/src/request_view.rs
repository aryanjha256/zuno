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
    RequestKind,
    Body, BodyDiff, Direction, Engine, EngineError, Event, Frame, Header, Hits, JobId, RawKind,
    RequestId, RequestSettings, RequestSpec, Resolver, ResponseData, ResponseDiff,
    assertion::{Assertion, Op},
    capture::Capture,
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
use crate::kinds::{HttpEditor, KindEditor};
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

/// One line of a conversation.
pub struct TranscriptRow {
    /// Since the connect, so the gaps are readable without a wall clock on every row — which is
    /// what you actually want to see in a heartbeat or a subscription.
    pub at: Duration,
    pub kind: TranscriptKind,
}

/// What a transcript row is.
///
/// **Not everything in a conversation is a message.** A stream dropping and being picked up
/// again is the most important thing that can happen to it, and it belongs *in the timeline*
/// where the gap is — a counter in the status strip would say it happened without saying when,
/// which is the one thing you need in order to read what is missing.
pub enum TranscriptKind {
    Frame { direction: Direction, frame: Frame },
    /// A break: a drop, a reconnect, a give-up.
    Notice(SharedString),
}

impl TranscriptKind {
    fn weight(&self) -> usize {
        match self {
            TranscriptKind::Frame { frame, .. } => frame.len(),
            TranscriptKind::Notice(text) => text.len(),
        }
    }

    pub fn frame(&self) -> Option<&Frame> {
        match self {
            TranscriptKind::Frame { frame, .. } => Some(frame),
            TranscriptKind::Notice(_) => None,
        }
    }
}

/// A conversation, live or finished.
///
/// **Kept after the socket closes, deliberately.** A transcript is the *result* of a session
/// the way a response is the result of a request: closing the connection is the end of the run,
/// not the end of wanting to read it. Clearing it on close would make the most common ending —
/// the server hanging up — also erase what it said on the way out.
///
/// Runtime state, like `ResponseData`: it survives switching tabs, because it lives on the
/// view, and does not survive a restart, because nothing serializes it.
pub struct Transcript {
    pub job: JobId,
    /// What is carrying this — a socket, or an event stream. Reported by the engine rather than
    /// guessed from the kind, because an ordinary HTTP request becomes a session whenever the
    /// server answers `text/event-stream`.
    pub transport: zuno_core::Transport,
    /// Whichever subprotocol the server picked from the offers. The one thing about an open
    /// socket you cannot learn by watching it.
    pub protocol: Option<String>,
    /// The handshake's status line and headers.
    ///
    /// **Held here rather than read off `inflight`**, which is cleared at the close — so a
    /// finished transcript kept the frames and silently lost the `101 Switching Protocols` and
    /// every response header that came with it. A stream's headers are where `content-type`,
    /// `cache-control` and the CORS and auth answers live, and they are worth exactly as much
    /// after it ends.
    pub status: Option<(u16, String)>,
    pub headers: Vec<Header>,
    /// **A ring, not a log.** Bounded by `MAX_TRANSCRIPT_BYTES` and `MAX_TRANSCRIPT_FRAMES`,
    /// whichever binds first, dropping from the front. A subscription is the likeliest thing in
    /// the app to run for hours, and nothing else here has a ceiling: `history` has one, this
    /// had none, and the process would eventually die with no explanation.
    ///
    /// Oldest-first because the recent frames are the ones being read — dropping the newest
    /// would make the cap worse than no cap.
    ///
    /// A `VecDeque` rather than a `Vec`: evicting from the front of a `Vec` is a memmove of
    /// everything behind it, which at ten thousand frames and a few hundred a second is the
    /// kind of quiet O(n²) this codebase has already paid for once in the render.
    pub rows: std::collections::VecDeque<TranscriptRow>,
    /// How many have been evicted, so the transcript can say so rather than silently losing
    /// the beginning of a conversation someone is debugging.
    pub dropped: usize,
    /// Bytes of payload currently held in `rows`.
    retained: usize,
    /// How many of `rows` are messages rather than notices.
    ///
    /// Maintained rather than counted, because the strip asks for it and the strip redraws on
    /// every arriving frame — a scan there is the O(n)-per-repaint shape this feature already
    /// paid for once in the row previews.
    frame_count: usize,
    /// `Some` once the socket has closed, carrying the close code and reason. A `None` code
    /// inside means the peer vanished without a Close frame, which is an ordinary ending.
    pub closed: Option<(Option<u16>, String)>,
}

impl Transcript {
    pub fn is_open(&self) -> bool {
        self.closed.is_none()
    }

    /// Record a row, evicting from the front until both budgets hold.
    ///
    /// Returns how many were evicted, because every index into `rows` — the selected one, most
    /// of all — shifts by exactly that much. Returning it is what stops the detail pane quietly
    /// switching to a different frame under the reader; see `reindex_selection`.
    fn push(&mut self, row: TranscriptRow) -> usize {
        self.retained += row.kind.weight();
        if matches!(row.kind, TranscriptKind::Frame { .. }) {
            self.frame_count += 1;
        }
        self.rows.push_back(row);

        let mut dropped = 0;
        while self.rows.len() > MAX_TRANSCRIPT_FRAMES
            || (self.retained > MAX_TRANSCRIPT_BYTES && self.rows.len() > 1)
        {
            match self.rows.pop_front() {
                Some(gone) => {
                    self.retained = self.retained.saturating_sub(gone.kind.weight());
                    if matches!(gone.kind, TranscriptKind::Frame { .. }) {
                        self.frame_count = self.frame_count.saturating_sub(1);
                    }
                    dropped += 1;
                }
                None => break,
            }
        }

        self.dropped += dropped;
        dropped
    }

    /// How many rows are messages rather than notices, for the count on the strip.
    pub fn frames(&self) -> usize {
        self.frame_count
    }
}

/// The transcript's byte budget. Generous on purpose: now that only visible rows are formatted,
/// a long transcript costs nothing to *draw*, so this is about memory alone.
const MAX_TRANSCRIPT_BYTES: usize = 16 * 1024 * 1024;
/// And a count, for the streams whose frames are twenty bytes each — sixteen megabytes of those
/// is millions of rows, which the list would survive and a person would not.
#[cfg(not(test))]
const MAX_TRANSCRIPT_FRAMES: usize = 10_000;
/// **Small under test**, so a real socket reaches the ring in a handful of frames. What is worth
/// testing is the eviction arithmetic and the index shift behind it, and ten thousand frames
/// would exercise the same three lines more slowly. The production value is a number, not
/// behaviour.
#[cfg(test)]
const MAX_TRANSCRIPT_FRAMES: usize = 6;

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
    pub(crate) fn new(
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

    pub(crate) fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.name.read(cx).focus_handle(cx).is_focused(window)
            || self.value.read(cx).focus_handle(cx).is_focused(window)
    }
}

/// One editable assertion. `enabled` and `op` live here rather than in the inputs for
/// `CaptureRow`'s reason: changing either must not disturb what you typed.
pub struct AssertionRow {
    pub enabled: bool,
    pub path: Entity<TextInput>,
    pub op: Op,
    pub value: Entity<TextInput>,
}

impl AssertionRow {
    fn new(assertion: &Assertion, cx: &mut Context<RequestView>) -> Self {
        Self {
            enabled: assertion.enabled,
            path: cx.new(|cx| {
                TextInput::new(assertion.path.clone(), "$.status", "AssertCell", cx)
            }),
            op: assertion.op,
            value: cx.new(|cx| TextInput::new(assertion.value.clone(), "ok", "AssertCell", cx)),
        }
    }

    fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.path.read(cx).focus_handle(cx).is_focused(window)
            || self.value.read(cx).focus_handle(cx).is_focused(window)
    }
}

/// Why captures are running.
///
/// The only thing it decides is whether a refusal is reported. After a send, silence is right —
/// a request with no rules has nothing to say, and a failed one already shows its status. When
/// someone chose "Capture as variable", every refusal has to be named: they just asked for a
/// thing, and this path shipped doing nothing at all without a word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTrigger {
    Send,
    Requested,
}

/// One editable capture rule. `enabled` and `secret` live here rather than in the inputs for
/// the reason `KeyValueRow::enabled` does: toggling either must not disturb what you typed.
pub struct CaptureRow {
    pub enabled: bool,
    pub path: Entity<TextInput>,
    pub name: Entity<TextInput>,
    pub secret: bool,
}

impl CaptureRow {
    fn new(capture: &Capture, cx: &mut Context<RequestView>) -> Self {
        Self {
            enabled: capture.enabled,
            path: cx.new(|cx| {
                TextInput::new(capture.path.clone(), "$.access_token", "CaptureCell", cx)
            }),
            name: cx.new(|cx| TextInput::new(capture.name.clone(), "token", "CaptureCell", cx)),
            secret: capture.secret,
        }
    }

    fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.path.read(cx).focus_handle(cx).is_focused(window)
            || self.name.read(cx).focus_handle(cx).is_focused(window)
    }
}

/// Which section of the request the pane shows.
///
/// Headers, query and body used to stack, so the two you weren't editing still cost a header
/// row and an empty-state row each — about 130px to say "nothing here". Tabbed, they cost one
/// strip, and the body editor gets the pane's full height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestTab {
    Headers,
    /// One of the **active kind's own** tabs, by index into `KindEditor::tabs()`.
    ///
    /// **The strip is composed, not fixed.** HTTP contributes Params and Body; GraphQL
    /// contributes Query and Variables; a future gRPC contributes Message. Before this, the
    /// strip was a `const [RequestTab; 5]`, so a GraphQL request rendered **Params** and
    /// **Body** tabs that meant nothing to it — and a new kind would have meant a new variant
    /// here plus a new arm at every site that matched one.
    Kind(u8),
    /// What this request publishes into the environment after a successful send.
    Capture,
    /// What a run checks in the response.
    Assert,
}

impl Default for RequestTab {
    fn default() -> Self {
        // Slot 1, which is the body for HTTP — where the time goes. A buffer that knows its
        // kind uses `KindEditor::default_tab` instead; this is only the pre-load value.
        RequestTab::Kind(1)
    }
}

impl RequestTab {
    /// Visual order, which is also cycle order — deliberately not most-recently-used. With
    /// three tabs in a fixed strip, MRU sends the same keystroke somewhere different each time
    /// and throws away the muscle memory the strip gives for free.
    /// The strip, in visual order, for the kind a buffer is currently authoring.
    ///
    /// Spine tabs bracket the kind's own: **who you are → what you send → what you check.**
    /// That order happens to reproduce HTTP's original `Headers, Params, Body, Capture, Assert`
    /// exactly, so the composed strip costs existing muscle memory nothing.
    pub fn for_kind(kind: &KindEditor) -> Vec<RequestTab> {
        let mut tabs = vec![RequestTab::Headers];
        tabs.extend((0..kind.tabs().len() as u8).map(RequestTab::Kind));
        // Not unconditional: see `KindEditor::checks_a_response`. A socket has no single
        // response to capture from or assert on, and drawing the tabs anyway was two controls
        // that did nothing.
        if kind.checks_a_response() {
            tabs.push(RequestTab::Capture);
            tabs.push(RequestTab::Assert);
        }
        tabs
    }

    fn step(self, delta: isize, kind: &KindEditor) -> Self {
        let all = Self::for_kind(kind);
        let at = all.iter().position(|tab| *tab == self).unwrap_or(0) as isize;
        let len = all.len() as isize;
        all[(at + delta).rem_euclid(len) as usize]
    }
}

/// Which section of a response the pane shows.
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
    /// Where the time went, on one time axis. Third, so the two answers you came for keep
    /// their positions — this is the tab you visit when one of them was slow.
    Timing,
    /// What changed since the run before. Last for the same reason Timing is third: it is a
    /// question you ask *about* an answer, not one of the answers.
    Diff,
}

impl ResponseView {
    /// Visual order, which is also cycle order — not most-recently-used, for the reason
    /// `RequestTab::ALL` states: MRU on a fixed strip sends one keystroke somewhere
    /// different each time and throws away the muscle memory the strip gives for free.
    pub const ALL: [ResponseView; 4] = [
        ResponseView::Body,
        ResponseView::Headers,
        ResponseView::Timing,
        ResponseView::Diff,
    ];

    fn step(self, delta: isize) -> Self {
        let at = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0) as isize;
        let len = Self::ALL.len() as isize;
        Self::ALL[(at + delta).rem_euclid(len) as usize]
    }
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
    /// A capture rule. Shares the enum so `ToggleRow` and `RemoveRow` reach it without a second
    /// pair of actions, and so the compiler names every site that has to decide about it.
    Capture,
    /// An assertion, for the same reasons.
    Assert,
}

/// How far one `left`/`right` press moves the response body.
///
/// Roughly ten monospace characters at the viewer's text size: small enough to land where you
/// meant, large enough that crossing a long token isn't a drum solo.
const H_SCROLL_STEP: f32 = 70.;

/// Compare a table of live rows against the baseline's, without building either.
pub(crate) fn rows_match<T>(
    rows: &[KeyValueRow],
    base: &[T],
    field: impl Fn(&T) -> (bool, &str, &str),
    cx: &App,
) -> bool {
    rows.len() == base.len()
        && rows.iter().zip(base).all(|(row, base)| {
            let (enabled, name, value) = field(base);
            row.enabled == enabled
                && row.name.read(cx).text() == name
                && row.value.read(cx).text() == value
        })
}

pub struct RequestView {
    pub id: RequestId,
    pub name: String,
    /// The request as its file has it — the other half of `is_dirty`.
    ///
    /// Set in `load`, which every path that fills a buffer goes through, and again on save. A
    /// buffer with no file keeps whatever it was created with, so a fresh tab reads clean while
    /// one you typed into does not.
    pub baseline: RequestSpec,
    /// The collection file this buffer is backed by, or `None` for a scratch buffer.
    ///
    /// Set when a buffer is opened from a collection or saved into one. It exists because a
    /// filename derived from the URL is *not* an identity: without remembering the file, a
    /// second Ctrl+S would derive the same name, find it taken, and write `posts-2.json`.
    pub path: Option<PathBuf>,
    pub url: Entity<TextInput>,
    pub headers: Vec<KeyValueRow>,
    /// **Everything that differs by protocol**, in one field instead of a dozen.
    ///
    /// The app-side mirror of `RequestKind`: `KindEditor::Http` owns the method, query rows and
    /// the five body editors; `KindEditor::GraphQl` owns the document, its variables and the
    /// operation name. Adding gRPC or MQTT is a new variant and a new module, with the compiler
    /// naming every site that has to respond — and nothing added here.
    pub kind: KindEditor,
    /// What this request publishes. Held as rows for the same reason every other table is:
    /// `spec` derives from the inputs, so anything not represented here is destroyed on save.
    pub captures: Vec<CaptureRow>,
    /// What a run checks in the response. Rows for the same reason every other table is: `spec`
    /// derives from the inputs, so anything not represented here is destroyed on save.
    pub assertions: Vec<AssertionRow>,
    /// The status a run expects, as typed. `spec` parses it rather than mirroring a parsed copy
    /// alongside it, so what runs cannot disagree with what is on screen — and an empty or
    /// unparseable box simply means "this request states no expectation".
    pub expect_status: Entity<TextInput>,
    pub settings: RequestSettings,

    pub response: Option<ResponseData>,
    /// How the current response differs from the one before it. `None` on the first run.
    pub diff: Option<ResponseDiff>,
    /// The line-by-line body comparison behind the Diff tab. `None` on the first run, and
    /// while the background comparison is still running.
    ///
    /// Computed eagerly beside `diff` rather than when the tab is opened, which is a real
    /// cost traded for a real thing: the tab's label carries `+n −n`, and a count that only
    /// appears once you visit the tab cannot tell you whether visiting is worth it. The cost
    /// is bounded on both sides that matter — identical bodies settle on a byte compare, and
    /// anything past `MAX_DIFF_BYTES` returns without diffing.
    pub body_diff: Option<BodyDiff>,
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
    /// Which half of an HTML body to show, for as long as this buffer is open.
    ///
    /// Here rather than on `BodyView` because `BodyView` is rebuilt on every response, so a
    /// preference living there would silently reset on each send — which is the one moment the
    /// reader is least likely to notice it and most likely to be re-reading the same endpoint.
    /// Threaded into `BodyView::build` the same way `force_parse` is.
    pub html_view: crate::body_view::HtmlView,
    /// The indexed body. `None` while it's still being built off-thread.
    pub body_view: Option<BodyView>,
    body_task: Option<Task<()>>,
    /// Which environment a capture writes into, taken at send time.
    ///
    /// Passed in rather than read here because only `Workspace` knows what is selected. `None`
    /// means nothing was selected, which captures refuse rather than falling back to globals —
    /// see `run_captures`.
    capture_target: Option<String>,
    capture_task: Option<Task<()>>,
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
    /// The Diff tab's scroll state.
    ///
    /// Its own handle rather than sharing `body_scroll`: the two lists are never on screen
    /// together, which is what lets the JSON and raw views share one, but they are views of
    /// *different documents* — a diff is a few dozen rows where the body is thousands, so a
    /// shared offset would land one of them somewhere its content does not reach.
    pub diff_scroll: UniformListScrollHandle,
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
    /// Which multipart row's type menu is being opened, and where its chip is.
    ///
    /// Parked here for the reason `menu_anchor` is: an `Action` carries no payload without
    /// pulling `schemars` in for a derived one, and the menu's two verbs need to know *which*
    /// row they act on. Kept rather than consumed when the menu opens, because the row is still
    /// needed when a item is finally chosen — a dismissed menu just leaves it to be overwritten
    /// by the next chip click, and nothing else can dispatch those two actions.
    part_kind_menu: Option<(usize, gpui::Point<gpui::Pixels>)>,
    /// Holding the diff task is what keeps it alive, and replacing it is what makes a
    /// superseded diff harmless — see `diff_against`.
    diff_task: Option<Task<()>>,
    pub inflight: Option<InFlight>,
    /// The conversation, when this buffer is a session. Outlives `inflight`, which ends at the
    /// close — see `Transcript`.
    pub session: Option<Transcript>,
    /// The transcript's scroll, held so a newly arrived frame can be revealed.
    ///
    /// A handle is the only way in: `scroll_to_item` writes a deferred request that nothing
    /// consumes unless the list was built with `track_scroll`, and omitting it fails silently.
    pub session_scroll: gpui::UniformListScrollHandle,
    /// Which frame the detail pane is showing, as an index into `Transcript::frames`.
    pub session_selected: Option<usize>,
    pub error: Option<EngineError>,
    pub status: Option<SharedString>,

    pub response_focus: FocusHandle,
}

impl RequestView {
    pub fn new(spec: RequestSpec, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            id: spec.id,
            name: String::new(),
            baseline: RequestSpec::default(),
            path: None,
            url: cx.new(|cx| TextInput::new("", "", "UrlBar", cx)),
            headers: Vec::new(),
            kind: KindEditor::Http(HttpEditor::new(cx)),
            captures: Vec::new(),
            assertions: Vec::new(),
            expect_status: cx.new(|cx| TextInput::new("", "200", "ExpectStatus", cx)),
            capture_target: None,
            capture_task: None,
            settings: RequestSettings::default(),
            response: None,
            diff: None,
            body_diff: None,
            history: Vec::new(),
            viewing: 0,
            response_view: ResponseView::default(),
            request_tab: RequestTab::default(),
            html_view: crate::body_view::HtmlView::default(),
            body_view: None,
            body_task: None,
            search: None,
            body_search: None,
            search_task: None,
            body_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            headers_scroll: gpui::ScrollHandle::new(),
            menu_anchor: None,
            part_kind_menu: None,
            diff_task: None,
            inflight: None,
            session: None,
            session_scroll: gpui::UniformListScrollHandle::new(),
            session_selected: None,
            error: None,
            status: None,
            // Higher than the inputs' default 0, so Tab reaches every text field first
            // and only then leaves for the response pane. The body editor sets its own
            // handle to tab_stop, at the default index, so it lands with the inputs.
            response_focus: cx.focus_handle().tab_index(2).tab_stop(true),
        };
        view.load(spec, cx);
        // A fresh buffer opens on its kind's home tab. `load` cannot decide this — it now
        // preserves whatever tab the buffer was on, and a new one has no meaningful "was".
        view.request_tab = RequestTab::Kind(view.kind.default_tab());
        view
    }

    /// Replace this buffer's contents with a different request.
    ///
    /// Used by curl import. Deliberately in-place rather than swapping in a fresh
    /// entity: replacing the entity invalidates every handle to it and resets focus, and
    /// the buffer's *identity* hasn't changed — only what's in it.
    ///
    /// `response_focus` is intentionally not rebuilt, so focus survives the swap.
    /// The HTTP editors, when this buffer is authoring an HTTP request.
    ///
    /// `None` for any other kind, and callers are expected to *mean* it — a GraphQL request has
    /// no body table, no form rows and no query rows, so a control for one would be a control
    /// that does nothing. Same shape as `RequestSpec::http`, deliberately.
    pub fn http(&self) -> Option<&HttpEditor> {
        self.kind.as_http()
    }

    /// Replace the authoring state with another kind's.
    ///
    /// **Destructive by nature** — the outgoing `KindEditor` owns its editors, so its text goes
    /// with it. `Workspace::switch_request_kind` asks first when there is anything to lose; see
    /// `KindEditor::has_content`.
    pub fn set_kind(&mut self, kind: KindEditor, cx: &mut Context<Self>) {
        // A tab slot valid for the old kind may not exist in the new one.
        self.request_tab = RequestTab::Kind(kind.default_tab());
        self.kind = kind;
        cx.notify();
    }

    /// The HTTP verb, for the kinds that have one.
    pub fn method(&self) -> Option<&zuno_core::Method> {
        self.kind.method()
    }

    pub fn set_method(&mut self, method: zuno_core::Method, cx: &mut Context<Self>) {
        self.kind.set_method(method);
        cx.notify();
    }

    /// The text surface this kind's find bar and formatter act on — the body editor for a raw
    /// HTTP body, the document for GraphQL.
    pub fn primary_editor(&self) -> Option<&Entity<Editor>> {
        self.kind.primary_editor()
    }

    pub fn load(&mut self, spec: RequestSpec, cx: &mut Context<Self>) {
        self.baseline = spec.clone();

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

        // **One call, and it is exhaustive on the kind.** Everything that differs by protocol
        // is built inside `KindEditor::from_spec`, so a new kind is a new arm there rather than
        // another twenty lines here — and `load` silently dropping a kind's fields is exactly
        // how non-raw bodies were once emptied and then written over on save.
        //
        // Built before the fields below are replaced, so a failure part-way cannot leave the
        // view half-loaded.
        let kind = KindEditor::from_spec(&spec.kind, cx);

        // **Kept if it still exists, not reset.** `request_tab` is documented as sticky per
        // buffer — `response_view`'s note is explicit that not even `load` resets it — and
        // clearing it here threw away which section you were editing every time a request was
        // opened. Only a slot the incoming kind does not have needs replacing.
        if !RequestTab::for_kind(&kind).contains(&self.request_tab) {
            self.request_tab = RequestTab::Kind(kind.default_tab());
        }
        self.id = spec.id;
        self.name = spec.name;
        self.kind = kind;
        self.url = url;
        self.headers = headers;
        self.captures = spec
            .captures
            .iter()
            .map(|capture| CaptureRow::new(capture, cx))
            .collect();
        self.assertions = spec
            .assertions
            .iter()
            .map(|assertion| AssertionRow::new(assertion, cx))
            .collect();
        self.expect_status = cx.new(|cx| {
            let text = spec.expect_status.map(|code| code.to_string()).unwrap_or_default();
            TextInput::new(text, "200", "ExpectStatus", cx)
        });
        self.settings = spec.settings;

        // A different request has no relationship to the last one's response.
        self.response = None;
        self.diff = None;
        self.body_diff = None;
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
        self.request_tab = self.request_tab.step(delta, &self.kind);
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

    /// Swap an HTML body between the extracted text and the markup.
    ///
    /// No re-index: `BodyView` holds both halves, so this is an `Arc` swap rather than a second
    /// hundred-millisecond pass through html5ever. The preference is written to the buffer as
    /// well as to the view, so the next response to this request opens the way this one is left.
    ///
    /// A no-op on anything that is not an extractable HTML body, which is what lets the palette
    /// row exist unconditionally without doing something surprising on a JSON response.
    pub fn toggle_html_view(&mut self, cx: &mut Context<Self>) {
        let Some(showing) = self.body_view.as_ref().and_then(BodyView::html_view) else {
            return;
        };
        let next = showing.other();
        self.html_view = next;
        if let Some(body) = self.body_view.as_mut() {
            body.set_html_view(next);
        }
        cx.notify();
    }

    pub fn cycle_response_view(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.response_view = self.response_view.step(delta);
        cx.notify();
    }

    /// Show one section by name, which is what a tab click and a palette row both mean.
    ///
    /// **Three tabs need this and two did not.** With two, one cycling action served both
    /// because the single inactive tab was always one step away; with three, clicking
    /// Timing while on Body is two steps and a cycling handler lands on Headers instead —
    /// a control that does something other than what its label says. Same correction the
    /// request pane's strip already made.
    pub fn show_response_view(&mut self, view: ResponseView, cx: &mut Context<Self>) {
        if self.response_view != view {
            self.response_view = view;
            cx.notify();
        }
    }

    /// The body diff, but only where it describes what is on screen.
    ///
    /// `body_diff` compares the live response with the one before it, so beside an older run it
    /// is not merely uninteresting but **wrong** — it would label lines as added that the run
    /// you are looking at never contained. The summary diff bar solves this by hiding itself; a
    /// tab cannot hide without shifting the three tabs beside it, so the decision is made here
    /// and the tab renders a note instead.
    ///
    /// A method rather than a check inside the renderer so a test can ask. Nothing in the
    /// headless platform can observe that a region was not painted — `debug_bounds` reports the
    /// last frame drawn and `is_none()` proves nothing — so the alternative is an assertion that
    /// passes whether or not the guard is there.
    pub fn diff_to_show(&self) -> Option<&BodyDiff> {
        (self.viewing == 0).then_some(self.body_diff.as_ref()).flatten()
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
            expect_status: self.expect_status_value(cx),
            assertions: self
                .assertions
                .iter()
                .map(|row| Assertion {
                    enabled: row.enabled,
                    path: row.path.read(cx).text().to_string(),
                    op: row.op,
                    value: row.value.read(cx).text().to_string(),
                })
                .collect(),
            captures: self
                .captures
                .iter()
                .map(|row| Capture {
                    enabled: row.enabled,
                    path: row.path.read(cx).text().to_string(),
                    name: row.name.read(cx).text().to_string(),
                    secret: row.secret,
                })
                .collect(),
            id: self.id,
            name: self.name.clone(),
            url: self.url.read(cx).text().to_string(),
            kind: self.request_kind(cx),
            headers: self
                .headers
                .iter()
                .map(|row| Header {
                    enabled: row.enabled,
                    name: row.name.read(cx).text().to_string(),
                    value: row.value.read(cx).text().to_string(),
                })
                .collect(),
            settings: self.settings.clone(),
        }
    }

    /// Whether this buffer differs from the file it came from.
    ///
    /// Compares against `baseline` field by field rather than building `spec()` and comparing
    /// that: the tab strip asks every buffer every frame, and `spec()` clones every row and the
    /// whole body. Ordered cheapest first and short-circuiting, so nothing here allocates.
    ///
    /// `id` is excluded. Collection files write 0 and it is reassigned on open (invariant 9), so
    /// it is never a difference the reader made.
    pub fn is_dirty(&self, cx: &App) -> bool {
        // **Destructured with no `..`, and that is the whole guard.** This is a hand-written
        // mirror of `spec`, so a new `RequestSpec` field is simply forgotten here — `captures`
        // was, and editing one left the tab clean, so `Ctrl+W` closed without asking and the
        // rule was gone. Now adding a field fails to compile until someone decides whether
        // changing it makes a buffer dirty.
        let RequestSpec {
            // Neither is edited here: a session-local handle, and a name derived from the URL.
            id: _,
            name: _,
            url,
            headers,
            settings,
            kind,
            captures,
            expect_status,
            assertions,
        } = &self.baseline;

        // Destructured with no `..` for the same reason the spec is: a field added to the
        // kind must fail to compile here until someone decides whether editing it makes a
        // buffer dirty. The kind itself is matched exhaustively so that *adding a kind*
        // does too.
        let spine_changed = self.settings != *settings
            || self.url.read(cx).text() != url
            || !rows_match(&self.headers, headers, |h| (h.enabled, &h.name, &h.value), cx)
            || !self.captures_match(captures, cx)
            || self.expect_status_value(cx) != *expect_status
            || !self.assertions_match(assertions, cx);

        if spine_changed {
            return true;
        }

        self.kind.is_dirty(kind, cx)
    }

    /// The kind half of `spec`, read back out of whichever editors are live.
    ///
    /// **Only one side is ever read**, which is what keeps the two from fighting: an HTTP
    /// request never carries a stray GraphQL document, and a GraphQL request never carries a
    /// body the user cannot see. Switching kind is lossless in the same way switching body type
    /// is — both sets of editors keep their text, and only what gets *sent* changes.
    fn request_kind(&self, cx: &App) -> RequestKind {
        self.kind.to_spec(cx)
    }

    /// Choose the body type.
    ///
    /// Nothing is discarded: the editor's text, the form rows, the multipart parts, and the
    /// binary path all stay put, so switching JSON → Form → JSON round-trips and only what
    /// gets *sent* changes. A mistaken type change is therefore never destructive.
    pub fn set_body_type(&mut self, body_type: BodyType, cx: &mut Context<Self>) {
        let Some(http) = self.kind.as_http_mut() else { return };
        http.set_body_type(body_type);
        cx.notify();
    }

    /// Switch one multipart part between sending text and sending a file.
    ///
    /// **Lossless, and that is why it is a toggle rather than two row types.** `is_file` only
    /// decides how the cell's text is *read* when the spec is derived — `MultipartValue::File`
    /// of that string, or `Text` of it — so flipping it back and forth destroys nothing and a
    /// path typed by hand survives being switched to text and back.
    ///
    /// It exists because the state was previously **invisible and one-way**: a part became a
    /// file only by choosing one, nothing on the row said which it was, and there was no way
    /// back. A form-data body routinely mixes the two.
    pub fn set_multipart_kind(&mut self, ix: usize, is_file: bool, cx: &mut Context<Self>) {
        let Some(http) = self.kind.as_http_mut() else { return };
        let Some(part) = http.multipart.get_mut(ix) else { return };
        if part.is_file == is_file {
            return;
        }
        part.is_file = is_file;
        cx.notify();
    }

    /// Point a multipart part at a file, marking it a file part.
    pub fn set_multipart_file(&mut self, ix: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(http) = self.kind.as_http_mut() else { return };
        let Some(part) = http.multipart.get_mut(ix) else {
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

    /// Put focus on one multipart part's value cell.
    ///
    /// Exists so a **click** on that row's browse icon can target that row. `ChooseBodyFile`
    /// resolves which part it fills from focus — one verb for "pick a file", per
    /// `choose_body_file` — and an icon button is not inside the cell, so `track_focus` does not
    /// move focus there on its own. `Window::focus` writes `window.focus` synchronously and
    /// `Window::dispatch_action` reads it before deferring (`window.rs:1386` and `:1477`), so
    /// focusing here and dispatching on the next line reaches the row that was clicked.
    pub fn focus_multipart_value(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(part) = self.http().and_then(|http| http.multipart.get(ix)) else { return };
        window.focus(&part.row.value.read(cx).focus_handle(cx));
    }

    /// The multipart part containing focus, if the body is multipart.
    ///
    /// Lets one "choose a file" verb serve both bodies: with a part focused it fills that
    /// part, otherwise it sets the whole binary body.
    pub fn focused_multipart_row(&self, window: &Window, cx: &App) -> Option<usize> {
        let http = self.http()?;
        if http.body_type != BodyType::Multipart {
            return None;
        }
        http.multipart
            .iter()
            .position(|part| part.row.is_focused(window, cx))
    }

    /// The header-*name* cell that currently has focus.
    ///
    /// By entity, not by key context: a row's name and value inputs share `"HeaderCell"`, so a
    /// context predicate cannot tell them apart — only the row knows which of its two is which.
    ///
    /// Deliberately **not** returning bounds. What the suggestion list contains must not depend
    /// on whether the cell has been painted yet: `last_bounds` is written during paint, so a
    /// brand-new row has none on its first frame, and folding the two together made the list's
    /// *contents* unobservable for a frame rather than just its position.
    pub fn focused_header_name(&self, window: &Window, cx: &App) -> Option<usize> {
        self.headers
            .iter()
            .position(|row| row.name.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Where a header name cell last painted, in window coordinates. `None` until it has been
    /// drawn once.
    pub fn header_name_bounds(
        &self,
        row: usize,
        cx: &App,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.headers.get(row)?.name.read(cx).last_bounds()
    }

    pub fn url_focus(&self, cx: &App) -> FocusHandle {
        self.url.read(cx).focus_handle(cx)
    }

    /// The main text surface's focus handle, when this kind has one on screen.
    ///
    /// Kind-aware now: for GraphQL it is the document editor, not a body editor that is never
    /// painted. Focusing an unpainted handle is what once killed the whole keymap — see
    /// `body_focus_target`.
    pub fn body_focus(&self, cx: &App) -> Option<FocusHandle> {
        self.primary_editor()
            .map(|editor| editor.read(cx).focus_handle(cx))
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
        let Some(http) = self.http() else {
            // Every other kind's main surface is an editor that is always painted.
            return self.body_focus(cx);
        };
        match http.body_type {
            BodyType::Raw => self.body_focus(cx),
            BodyType::Form => http
                .form
                .first()
                .map(|row| row.name.read(cx).focus_handle(cx)),
            BodyType::Multipart => http
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
        let Some(http) = self.http() else {
            return self.kind.is_focused(window, cx);
        };
        match http.body_type {
            BodyType::Raw => self
                .body_focus(cx)
                .is_some_and(|handle| handle.is_focused(window)),
            BodyType::Form => http.form.iter().any(|row| row.is_focused(window, cx)),
            BodyType::Multipart => {
                http.multipart.iter().any(|part| part.row.is_focused(window, cx))
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
    pub fn send(
        &mut self,
        engine: &Arc<Engine>,
        resolver: &Resolver,
        environment: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.capture_target = environment;

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
        // A reconnect is a new conversation. The old transcript is kept until *here* rather
        // than cleared at the close, so it stays readable for as long as nobody starts again.
        self.session = None;
        self.session_selected = None;
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
            Event::Opened {
                transport,
                status,
                status_text,
                headers,
                protocol,
                elapsed,
                ..
            } => {
                // The handshake response is a response, so it fills the same fields the status
                // line already reads — the pane shows `101 Switching Protocols` and the server's
                // headers without a second code path for them.
                if let Some(inflight) = self.inflight.as_mut() {
                    inflight.status = Some((status, status_text.clone()));
                    inflight.headers = headers.clone();
                    inflight.ttfb = Some(elapsed);
                }
                self.session = Some(Transcript {
                    job: current,
                    transport,
                    protocol,
                    status: Some((status, status_text)),
                    headers,
                    rows: std::collections::VecDeque::new(),
                    dropped: 0,
                    retained: 0,
                    frame_count: 0,
                    closed: None,
                });
                cx.notify();
                true
            }
            Event::Frame {
                at,
                direction,
                frame,
                ..
            } => {
                // Asked **before** the push, so "were you at the end" is not made false by the
                // row that is arriving.
                let following = self
                    .session
                    .as_ref()
                    .is_some_and(|session| self.at_transcript_end(session.rows.len()));

                let mut evicted = 0;
                if let Some(session) = self.session.as_mut() {
                    evicted = session.push(TranscriptRow {
                        at,
                        kind: TranscriptKind::Frame { direction, frame },
                    });
                    let last = session.rows.len().saturating_sub(1);
                    // Follow only while the person is already at the end. Scroll up and the
                    // stream carries on filling underneath; scroll back down and it resumes
                    // following, with no control to find and nothing to remember.
                    if following {
                        self.session_scroll
                            .scroll_to_item(last, gpui::ScrollStrategy::Top);
                    }
                }

                self.reindex_selection(evicted);
                cx.notify();
                true
            }
            // **In the timeline, not in a counter.** A stream that dropped at 12.4s and came
            // back at 15.9s is missing whatever happened in between, and the only way to read
            // that is to see the gap where it is.
            Event::Reconnecting { attempt, delay, .. } => {
                let notice = if attempt == 1 {
                    format!("disconnected · reconnecting in {:.0}s", delay.as_secs_f32())
                } else {
                    format!(
                        "still disconnected · retrying in {:.0}s (attempt {attempt})",
                        delay.as_secs_f32()
                    )
                };
                let mut evicted = 0;
                if let Some(session) = self.session.as_mut() {
                    evicted = session.push(TranscriptRow {
                        at: session
                            .rows
                            .back()
                            .map(|row| row.at)
                            .unwrap_or_default(),
                        kind: TranscriptKind::Notice(SharedString::from(notice)),
                    });
                }
                // **A notice evicts like anything else.** Discarding the count here left the
                // detail pane pointing at whatever slid into the selected index — the same bug
                // the frame arm has a test for, in the arm that did not.
                self.reindex_selection(evicted);
                cx.notify();
                true
            }
            Event::Closed { code, reason, .. } => {
                if let Some(session) = self.session.as_mut() {
                    session.closed = Some((code, reason));
                }
                // Dropping `inflight` drops the task running this very closure, which is why
                // this arm returns `false` — see `InFlight::_task`.
                self.inflight = None;
                cx.notify();
                false
            }
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
                // Cleared rather than left stale: they described a comparison that no longer
                // holds, and the replacements arrive from a background task a frame or two later.
                self.diff = None;
                self.body_diff = None;
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
                // **A failed session is a closed one.** Without this the transcript kept
                // `closed: None` while `inflight` went away, so a socket killed by a network
                // drop or a protocol error drew a strip reading "open" with a live Disconnect
                // button — and the error never appeared, because `render` returns the
                // transcript before it reaches the error arm. The reason travels in the close
                // reason so it lands where the person is already looking.
                if let Some(session) = self.session.as_mut()
                    && session.closed.is_none()
                {
                    session.closed = Some((None, error.to_string()));
                }
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

    /// Slide the selected row down by however many fell off the front.
    ///
    /// **Every index into `rows` moves by exactly the eviction count**, and the selection is the
    /// one that matters: without this the detail pane keeps its highlight and silently shows a
    /// different frame, which is a wrong answer rather than a missing one. When the selected row
    /// is the one that went, there is nothing honest left to show and the pane closes.
    ///
    /// One method rather than the arithmetic written out per arm, because it *was* written out
    /// per arm and the second arm forgot it.
    fn reindex_selection(&mut self, evicted: usize) {
        if evicted == 0 {
            return;
        }
        match self.session_selected.map(|ix| ix.checked_sub(evicted)) {
            Some(Some(moved)) => self.session_selected = Some(moved),
            Some(None) => {
                self.session_selected = None;
                self.body_view = None;
                self.body_task = None;
            }
            None => {}
        }
    }

    /// Whether the transcript is parked at its newest row.
    ///
    /// **This is what makes a fast stream readable.** The reveal used to be unconditional, and
    /// against something like Wikimedia's recent-changes feed — several events a second — it
    /// dragged the view back to the bottom before a person could finish reading a row, which
    /// presents as "scrolling is broken" rather than as a following list.
    ///
    /// Reading the handle here is deliberately a frame behind, and that is the right answer
    /// rather than a limitation: the question is whether the person was looking at the end *as
    /// it was last painted*, which is exactly what the last prepaint recorded.
    fn at_transcript_end(&self, total: usize) -> bool {
        if total == 0 {
            return true;
        }
        // **Read off the state rather than through `logical_scroll_top_index`**, which is
        // `#[cfg(any(test, feature = "test-support"))]` — it compiles under `cargo test` and
        // vanishes from the shipping binary. `ScrollHandle::logical_scroll_top` is not gated
        // and is what that helper calls; the deferred check in front of it matters because a
        // reveal requested this frame has not been applied yet.
        let state = self.session_scroll.0.borrow();
        let Some(size) = state.last_item_size else {
            // Never painted, so there is no scrollback to protect yet.
            return true;
        };
        let row = f32::from(size.item.height);
        if row <= 0. {
            return true;
        }
        let visible = (f32::from(size.contents.height) / row).ceil() as usize;
        let top = state
            .deferred_scroll_to_item
            .as_ref()
            .map(|deferred| deferred.item_index)
            .unwrap_or_else(|| state.base_handle.logical_scroll_top().0);

        top + visible >= total
    }

    /// Show one frame in full, through the response body's own viewer.
    ///
    /// **The frame is indexed into `body_view`, the same field a response uses**, and that
    /// reuse is the design rather than a shortcut: a socket has no response body competing for
    /// it, so for a session "the body" simply *is* the selected frame. Everything built on that
    /// field then works with no second implementation — the JSON outline, folding, `Ctrl+F`,
    /// copy, the too-large notice. Writing a second viewer would have meant a second set of
    /// fold state, scroll state and search state that could drift from the first.
    ///
    /// Indexed on the background executor like any other body (invariant 3): a subscription can
    /// push a megabyte down a socket, and `JsonOutline::parse` on the UI thread would drop
    /// frames on a click.
    pub fn select_frame(&mut self, ix: usize, cx: &mut Context<Self>) {
        // A notice is a break in the conversation, not a message — there is nothing to open.
        let Some(frame) = self
            .session
            .as_ref()
            .and_then(|session| session.rows.get(ix))
            .and_then(|row| row.kind.frame())
        else {
            return;
        };

        let body: bytes::Bytes = match frame {
            zuno_core::Frame::Text(text) => bytes::Bytes::from(text.clone().into_bytes()),
            // The payload alone. The name and id are on the row and in the detail header,
            // where they are metadata about the event rather than part of it — putting them
            // in the viewer would break the JSON that is usually inside.
            zuno_core::Frame::Event { data, .. } => bytes::Bytes::from(data.clone().into_bytes()),
            zuno_core::Frame::Binary(bytes)
            | zuno_core::Frame::Ping(bytes)
            | zuno_core::Frame::Pong(bytes) => bytes.clone(),
        };

        self.session_selected = Some(ix);
        self.body_view = None;

        // `None` content type, honestly: a WebSocket frame carries no such header. `BodyView`
        // sniffs the bytes, which is the only evidence there is.
        //
        // `HtmlView::Raw` rather than the `Text` default: the readable-text extraction exists
        // because an HTML *response* is usually a framework's error page, and a frame that
        // happens to contain markup is payload — showing it as prose would hide what was sent.
        let build = cx
            .background_executor()
            .spawn(async move { BodyView::build(body, None, false, crate::body_view::HtmlView::Raw) });

        self.body_task = Some(cx.spawn(async move |this, cx| {
            let view = build.await;
            let _ = this.update(cx, |this, cx| {
                this.body_view = Some(view);
                // No `run_captures` here, unlike `index_body`: a session has no captures — see
                // `KindEditor::checks_a_response`, which is why it has no Capture tab either.
                if this.is_searching() {
                    this.run_search(cx);
                }
                cx.notify();
            });
        }));
    }

    /// Keep the composed message with the request, so it survives the session and the restart.
    ///
    /// Does **not** clear the composer: saving and sending are different intentions, and the
    /// common order is to write something, save it, then send it.
    pub fn save_message(&mut self, cx: &mut Context<Self>) {
        let Some(socket) = self.kind.as_websocket() else {
            return;
        };
        let body = socket.compose.read(cx).text().to_string();
        if body.trim().is_empty() {
            return;
        }
        if let Some(socket) = self.kind.as_websocket_mut() {
            socket.save(body);
        }
        cx.notify();
    }

    /// Put a saved message back in the composer, ready to edit or send.
    ///
    /// Loaded rather than sent directly: the saved text is usually a template with one field to
    /// change, and a click that fired it at the server would make that a mistake you cannot
    /// take back.
    pub fn load_message(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(socket) = self.kind.as_websocket() else {
            return;
        };
        let Some(body) = socket.messages.get(ix).map(|message| message.body.clone()) else {
            return;
        };
        let composer = socket.compose.clone();
        composer.update(cx, |editor, cx| {
            let end = editor.text().len();
            editor.replace_range(0..end, &body, window, cx);
        });
        cx.notify();
    }

    pub fn forget_message(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(socket) = self.kind.as_websocket_mut() {
            socket.forget(ix);
            cx.notify();
        }
    }

    /// The selected frame's payload as text, when there is one and it is text.
    ///
    /// `None` for a binary frame rather than lossy bytes: the clipboard is for things you can
    /// paste, and `SaveResponse` is the counterpart that handles the rest — the same split the
    /// response body already makes.
    pub fn selected_frame_text(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        let frame = session.rows.get(self.session_selected?)?.kind.frame()?;
        match frame {
            zuno_core::Frame::Text(text) => Some(text.clone()),
            // Same as the viewer: what you paste into a fixture is the payload, not the
            // envelope that delivered it.
            zuno_core::Frame::Event { data, .. } => Some(data.clone()),
            zuno_core::Frame::Binary(_) | zuno_core::Frame::Ping(_) | zuno_core::Frame::Pong(_) => {
                None
            }
        }
    }

    /// Whether a socket is open right now, as opposed to merely having been.
    pub fn is_connected(&self) -> bool {
        self.session.as_ref().is_some_and(Transcript::is_open) && self.inflight.is_some()
    }

    /// Send whatever is in the composer down the open socket.
    ///
    /// **Clears the composer on success**, which is what makes it a composer rather than a
    /// buffer: the next message starts empty, the way every chat box works. What was sent is
    /// not lost — it is the last row of the transcript, which is a better record than a box
    /// still holding it.
    ///
    /// `{{vars}}` are resolved here, so a saved message can carry `{{token}}` and mean it.
    pub fn send_frame(
        &mut self,
        engine: &Arc<Engine>,
        resolver: &Resolver,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self
            .session
            .as_ref()
            .filter(|session| session.is_open())
            .map(|session| session.job)
        else {
            return;
        };
        let Some(socket) = self.kind.as_websocket() else {
            return;
        };

        let composer = socket.compose.clone();
        let text = composer.read(cx).text().to_string();
        if text.trim().is_empty() {
            return;
        }

        engine.send_frame(job, Frame::Text(resolver.resolve(&text).into_owned()));
        composer.update(cx, |editor, cx| {
            let end = editor.text().len();
            editor.replace_range(0..end, "", window, cx);
        });
        cx.notify();
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
        // Both comparisons in one task. They read the same two responses and land in the same
        // frame, so splitting them would buy two background hops and a window in which the
        // summary says the body changed while the Diff tab still shows nothing.
        let compute = cx.background_executor().spawn(async move {
            (
                ResponseDiff::between(&previous, &current),
                BodyDiff::between(&previous, &current),
            )
        });

        self.diff_task = Some(cx.spawn(async move |this, cx| {
            let (diff, body_diff) = compute.await;
            let _ = this.update(cx, |this, cx| {
                this.diff = Some(diff);
                this.body_diff = Some(body_diff);
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
        let html_view = self.html_view;

        self.body_view = None;
        let build = cx
            .background_executor()
            .spawn(async move { BodyView::build(body, content_type, force_parse, html_view) });

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
                this.run_captures(CaptureTrigger::Send, cx);
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

    /// Publish this request's captures into the selected environment.
    ///
    /// Three refusals, each of which would otherwise be a silent wrong answer:
    ///
    /// - **Only on a success.** A 401 body has fields too, and pulling one into `{{token}}`
    ///   produces a chain that fails on the *next* request — the hardest kind to read back.
    /// - **Only on the live run.** `index_body` also runs when you browse the history, and
    ///   re-publishing a token from three sends ago because you looked at it is not a thing
    ///   anyone asked for.
    /// - **Only into a selected environment**, never globals. A captured token is
    ///   environment-specific by nature — dev's and prod's are different values — so putting one
    ///   in the always-active layer means switching environment does not switch the token.
    fn run_captures(&mut self, trigger: CaptureTrigger, cx: &mut Context<Self>) {
        let requested = trigger == CaptureTrigger::Requested;

        if self.captures.is_empty() {
            return;
        }
        if self.viewing != 0 {
            self.refuse(requested, "Captures publish from the live response, not a retained run", cx);
            return;
        }
        let Some(response) = self.response.as_ref() else {
            self.refuse(requested, "Send the request first", cx);
            return;
        };
        if !(200..300).contains(&response.status) {
            self.refuse(requested, "Captures only publish from a successful response", cx);
            return;
        }

        let rules: Vec<Capture> = self
            .captures
            .iter()
            .map(|row| Capture {
                enabled: row.enabled,
                path: row.path.read(cx).text().to_string(),
                name: row.name.read(cx).text().to_string(),
                secret: row.secret,
            })
            .filter(|rule| {
                rule.enabled && !rule.path.trim().is_empty() && !rule.name.trim().is_empty()
            })
            .collect();
        if rules.is_empty() {
            self.refuse(requested, "Give the capture a path and a variable name", cx);
            return;
        }

        let Some(target) = self.capture_target.clone() else {
            self.status = Some("Select an environment to capture into".into());
            return;
        };
        let Some(root) = crate::collections::root(cx).map(std::path::Path::to_path_buf) else {
            self.refuse(requested, "No collection directory to hold environments", cx);
            return;
        };
        let Some(outline) = self
            .body_view
            .as_ref()
            .and_then(|body| body.outline())
            .cloned()
        else {
            self.status = Some("Captures need a JSON response".into());
            return;
        };

        // Extraction is a descent rather than a scan, so it is cheap even on a large body — but
        // reading and writing the environment is file IO, and invariant 3 keeps that off this
        // thread whatever its size.
        let work = cx.background_executor().spawn(async move {
            let mut file = match zuno_core::environment::read(&root, &target) {
                Ok(file) => file,
                Err(error) => return Err(format!("{error}")),
            };

            // Through `capture::publish` rather than written out here: the collection runner
            // needs the same rule, and invariant 10 written twice can be right in one place and
            // quietly wrong in the other. It was — this site removed the committed entry for
            // *every* secret name, which deletes the placeholder half of the very pattern
            // `EnvironmentFile` exists to keep.
            let published = zuno_core::capture::publish(&mut file, &outline, &rules);

            if let Err(error) = zuno_core::environment::save(&root, &file) {
                return Err(format!("{error}"));
            }

            // A capture writing the first secret into an environment has to arm the ignore rule
            // the same way the editor does. It did not: `protect_secrets` only fires on a
            // *switch*, so a token captured into a freshly-made environment sat in a file git
            // was still watching until you happened to switch away and back.
            let ignored = published.new_secret
                && matches!(zuno_core::environment::ensure_gitignored(&root), Ok(true));

            Ok((target, published.written, published.missed, ignored))
        });

        self.capture_task = Some(cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |this, cx| {
                this.status = Some(SharedString::from(match outcome {
                    Err(error) => error,
                    Ok((target, written, missed, ignored)) if missed.is_empty() => {
                        let mut message = format!("Captured {} into {target}", written.join(", "));
                        if ignored {
                            message.push_str(" — added *.local.json to .gitignore");
                        }
                        message
                    }
                    // Named, not counted: "1 path did not match" makes you go and find which.
                    Ok((_, _, missed, _)) => {
                        format!("No match for {}", missed.join(", "))
                    }
                }));
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
        self.request_tab = RequestTab::Kind(1);

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
            // Whichever surface this kind searches — the body editor for a raw HTTP body, the
            // document for GraphQL. Focusing an unpainted handle is what kills the keymap.
            if let Some(handle) = self.body_focus(cx) {
                window.focus(&handle);
            }
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
        let Some(editor) = self.primary_editor().cloned() else { return };
        let content = editor.read(cx).text().to_string();

        let hits = if query.is_empty() {
            zuno_core::search::Hits::default()
        } else {
            zuno_core::search::find(content.as_bytes(), &query)
        };

        // Which line each match falls in, so the bar can scroll to it and the editor can paint
        // it. `rows` means display lines here — see `TextSearch`.
        let rows = editor.read(cx).lines_for_offsets(&hits.offsets);

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

        let Some(editor) = self.primary_editor().cloned() else { return };
        editor.update(cx, |editor, cx| {
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

        let Some(editor) = self.primary_editor().cloned() else { return 0 };
        editor.update(cx, |editor, cx| {
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
        let Some(editor) = self.primary_editor().cloned() else { return 0 };
        editor.update(cx, |editor, cx| {
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
    pub fn body_kind(&self) -> RawKind {
        self.http().map(HttpEditor::body_kind).unwrap_or_default()
    }

    /// Set the raw body's flavour, and the editor's colouring with it.
    ///
    /// One funnel, for the reason `Workspace::activate` is one: the two live in different
    /// entities, so keeping them in step at each call site is a rule to remember rather than a
    /// thing that cannot be got wrong.
    pub fn set_body_kind(&mut self, kind: RawKind, cx: &mut Context<Self>) {
        if let Some(http) = self.kind.as_http_mut() {
            http.set_body_kind(kind, cx);
        }
        cx.notify();
    }

    pub fn set_menu_anchor(&mut self, at: gpui::Point<gpui::Pixels>) {
        self.menu_anchor = Some(at);
    }

    pub fn set_part_kind_menu(&mut self, ix: usize, at: gpui::Point<gpui::Pixels>) {
        self.part_kind_menu = Some((ix, at));
    }

    pub fn multipart_is_file(&self, ix: usize) -> bool {
        self.http().is_some_and(|http| http.multipart_is_file(ix))
    }

    /// The row whose type chip was clicked, and where it is. Consumed, so a stale click cannot
    /// place a later select.
    pub fn take_part_kind_menu(&mut self) -> Option<(usize, gpui::Point<gpui::Pixels>)> {
        self.part_kind_menu.take()
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
            // Params, form fields and multipart parts are HTTP's tables — a kind without them
            // has nothing to add a row to, so the verb is a no-op rather than a panic.
            RowKind::Query => {
                let row = KeyValueRow::new(true, "", "", "QueryCell", cx);
                match self.kind.as_http_mut() {
                    Some(http) => {
                        http.query.push(row);
                        http.query.last()
                    }
                    None => None,
                }
            }
            RowKind::Form => {
                let row = KeyValueRow::new(true, "", "", "FormCell", cx);
                match self.kind.as_http_mut() {
                    Some(http) => {
                        http.form.push(row);
                        http.form.last()
                    }
                    None => None,
                }
            }
            RowKind::Multipart => {
                let row = KeyValueRow::new(true, "", "", "PartCell", cx);
                match self.kind.as_http_mut() {
                    Some(http) => {
                        http.multipart.push(MultipartRow { row, is_file: false });
                        http.multipart.last().map(|part| &part.row)
                    }
                    None => None,
                }
            }
            // A different row type, so it cannot ride the shared `Option<&KeyValueRow>` return.
            // It moves focus itself, into the path cell rather than a name cell.
            RowKind::Capture => {
                self.push_capture(String::new(), String::new(), window, cx);
                None
            }
            RowKind::Assert => {
                self.push_assertion(String::new(), window, cx);
                None
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
        if let Some(http) = self.http() {
            if let Some(ix) = http.query.iter().position(|row| row.is_focused(window, cx)) {
                return Some((RowKind::Query, ix));
            }
            if let Some(ix) = http.form.iter().position(|row| row.is_focused(window, cx)) {
                return Some((RowKind::Form, ix));
            }
        }
        if let Some(ix) = self
            .http()
            .and_then(|http| {
                http.multipart
                    .iter()
                    .position(|part| part.row.is_focused(window, cx))
            })
        {
            return Some((RowKind::Multipart, ix));
        }
        if let Some(ix) = self.captures.iter().position(|row| row.is_focused(window, cx)) {
            return Some((RowKind::Capture, ix));
        }
        self.assertions
            .iter()
            .position(|row| row.is_focused(window, cx))
            .map(|ix| (RowKind::Assert, ix))
    }

    /// Flip a row's `enabled` flag. Multipart parts wrap their row, so this can't hand back
    /// a single `&mut Vec<KeyValueRow>` for every kind.
    fn toggle(&mut self, kind: RowKind, ix: usize) -> bool {
        match kind {
            RowKind::Header => flip_enabled(&mut self.headers, ix),
            // HTTP's own tables: a kind without them has no row to toggle.
            RowKind::Query => self
                .kind
                .as_http_mut()
                .is_some_and(|http| flip_enabled(&mut http.query, ix)),
            RowKind::Form => self
                .kind
                .as_http_mut()
                .is_some_and(|http| flip_enabled(&mut http.form, ix)),
            RowKind::Multipart => match self
                .kind
                .as_http_mut()
                .and_then(|http| http.multipart.get_mut(ix))
            {
                Some(part) => {
                    part.row.enabled = !part.row.enabled;
                    true
                }
                None => false,
            },
            RowKind::Capture => match self.captures.get_mut(ix) {
                Some(row) => {
                    row.enabled = !row.enabled;
                    true
                }
                None => false,
            },
            RowKind::Assert => match self.assertions.get_mut(ix) {
                Some(row) => {
                    row.enabled = !row.enabled;
                    true
                }
                None => false,
            },
        }
    }

    fn remove(&mut self, kind: RowKind, ix: usize) -> bool {
        let len = match kind {
            RowKind::Header => self.headers.len(),
            RowKind::Query => self.http().map_or(0, |http| http.query.len()),
            RowKind::Form => self.http().map_or(0, |http| http.form.len()),
            RowKind::Multipart => self.http().map_or(0, |http| http.multipart.len()),
            RowKind::Capture => self.captures.len(),
            RowKind::Assert => self.assertions.len(),
        };
        if ix >= len {
            return false;
        }
        match kind {
            RowKind::Header => drop(self.headers.remove(ix)),
            // `len` above is 0 for a kind without these tables, so `ix >= len` has already
            // returned — the `else` arms are unreachable rather than silently doing nothing.
            RowKind::Query => match self.kind.as_http_mut() {
                Some(http) => drop(http.query.remove(ix)),
                None => return false,
            },
            RowKind::Form => match self.kind.as_http_mut() {
                Some(http) => drop(http.form.remove(ix)),
                None => return false,
            },
            RowKind::Multipart => match self.kind.as_http_mut() {
                Some(http) => drop(http.multipart.remove(ix)),
                None => return false,
            },
            RowKind::Capture => drop(self.captures.remove(ix)),
            RowKind::Assert => drop(self.assertions.remove(ix)),
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

    /// Add a capture, focused on whichever cell still needs typing.
    ///
    /// Authoring from a response row fills both, so focus goes to the *name* — the path came
    /// from `path_to` and is right by construction, and the variable is the decision left.
    pub fn push_capture(
        &mut self,
        path: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefilled = !path.is_empty();
        self.captures.push(CaptureRow::new(
            &Capture {
                path,
                name,
                ..Default::default()
            },
            cx,
        ));

        if let Some(row) = self.captures.last() {
            let cell = if prefilled { &row.name } else { &row.path };
            let handle = cell.read(cx).focus_handle(cx);
            window.focus(&handle);
        }
        self.request_tab = RequestTab::Capture;
        cx.notify();
    }

    /// Say why captures did nothing, but only when someone asked for them.
    fn refuse(&mut self, requested: bool, why: &'static str, cx: &mut Context<Self>) {
        if requested {
            self.status = Some(SharedString::from(why));
            cx.notify();
        }
    }

    /// Publish now, against the response already on screen.
    ///
    /// **`environment` is passed rather than reused from the last send.** `capture_target` is set
    /// when a request goes out, so sending, switching environment, and *then* capturing a row
    /// would publish into the environment you had just left.
    pub fn publish_captures(&mut self, environment: Option<String>, cx: &mut Context<Self>) {
        self.capture_target = environment;
        self.run_captures(CaptureTrigger::Requested, cx);
    }

    /// The typed expectation as a status code, or `None` when the box is empty or nonsense.
    ///
    /// Nonsense reads as "no expectation" rather than as an error, for the reason the URL stays
    /// a raw `String`: people type through invalid states on the way to a valid one, and a box
    /// that complains at `2` on the way to `200` is a box nobody can type in.
    pub fn expect_status_value(&self, cx: &App) -> Option<u16> {
        self.expect_status.read(cx).text().trim().parse().ok()
    }

    /// Add an assertion and move focus into whichever cell still needs typing.
    pub fn push_assertion(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let prefilled = !path.is_empty();
        self.assertions.push(AssertionRow::new(
            &Assertion {
                path,
                ..Default::default()
            },
            cx,
        ));

        if let Some(row) = self.assertions.last() {
            // Authoring from a response row fills the path, so the value is what is left to say.
            let cell = if prefilled { &row.value } else { &row.path };
            let handle = cell.read(cx).focus_handle(cx);
            window.focus(&handle);
        }
        self.request_tab = RequestTab::Assert;
        cx.notify();
    }

    /// Step a row's operator. Three of them, so a cycle is cheaper than a picker and lands in
    /// one keystroke rather than three.
    pub fn cycle_assert_op(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.assertions.get_mut(ix) else { return };
        row.op = match row.op {
            Op::Exists => Op::Equals,
            Op::Equals => Op::Contains,
            Op::Contains => Op::Exists,
        };
        cx.notify();
    }

    fn captures_match(&self, base: &[Capture], cx: &App) -> bool {
        self.captures.len() == base.len()
            && self.captures.iter().zip(base).all(|(row, was)| {
                row.enabled == was.enabled
                    && row.secret == was.secret
                    && row.path.read(cx).text() == was.path
                    && row.name.read(cx).text() == was.name
            })
    }

    fn assertions_match(&self, base: &[Assertion], cx: &App) -> bool {
        self.assertions.len() == base.len()
            && self.assertions.iter().zip(base).all(|(row, was)| {
                row.enabled == was.enabled
                    && row.op == was.op
                    && row.path.read(cx).text() == was.path
                    && row.value.read(cx).text() == was.value
            })
    }

    pub fn toggle_capture_secret(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.captures.get_mut(ix) else { return };
        row.secret = !row.secret;
        cx.notify();
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
        self.http()
            .map(HttpEditor::body_label)
            .unwrap_or_else(|| SharedString::from("None"))
    }

    /// Point a binary body at a file, switching the body type to match.
    pub fn set_binary_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(http) = self.kind.as_http_mut() else { return };
        http.set_binary_path(path);
        cx.notify();
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
        let expected = match self.http()?.body(cx) {
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

        let url_focused = self.url_focus(cx).is_focused(window);

        div()
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            // Full width, above the split — see `request_pane::toolbar`.
            .child(request_pane::toolbar(self, &theme, url_focused, cx))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .child(request_pane::render(self, &theme, window, cx))
                    .child(div().w(px(1.)).flex_none().bg(theme.border))
                    .child(response_pane::render(self, &theme, window, cx)),
            )
    }
}
