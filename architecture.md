# Zuno — Architecture

> **Goal:** the most ridiculously good request → response loop possible.
> Open app → create request → send → inspect response → modify → resend.

**This was "Milestone One Architecture" for a long time after it stopped being that.** It now
records the design through M3 and past it — collections, environments, body authoring, response
search — and the M1 framing was actively misleading: a reader landing on the title would
reasonably assume everything after M1 is undocumented and go looking elsewhere. §13 still describes
M1 as shipped, deliberately, because the honest account of what that milestone did and didn't
deliver is worth keeping fixed in time.

The goal above has not changed, and the constraints in §1 still decide ties. Everything here exists
to serve that one loop; features that were once listed as out of scope have since been built *onto*
it without the rewrite §1 was designed to avoid, which is the main thing this document is evidence
for.

Pinned stack: `gpui = "0.2.2"` (crates.io release), Rust edition 2024.

---

## 1. Guiding constraints

Four rules that decide most of the design. When a later decision is ambiguous, these break the tie.

1. **The core never imports GPUI.** Request modeling, HTTP, JSON flattening, and text
   buffers must compile and unit-test without a window. This is enforced mechanically
   (see §2), not by discipline.
2. **Nothing parses or formats on the UI thread.** A 50MB response body is parsed,
   flattened, and measured on a background executor. Only a finished, indexable
   structure crosses back to the renderer. *Two things that don't look like parsing but are, both
   inline until an audit found them:* the response diff compares both bodies byte-for-byte and
   counts the newlines in each, and the session write serializes every open buffer. Assembling
   either one's input can need the UI thread — only it can read entities — but that part has to be
   a clone rather than a format, which is why `save_in_background` takes an owned `Session`.
3. **Bytes in, bytes stored.** Response bodies are `Bytes`, never `String`. Decoding to
   text is a lazy, display-time concern. Binary responses and invalid UTF-8 are normal, not edge cases.
4. **Latency is a spec, not a vibe.** §8 gives numbers. If they aren't asserted, "fast" drifts.

---

## 2. Repository layout

A **cargo workspace** with two members. `✅` marks what exists as of M1.0; everything else
is the slot it will land in.

```
zuno/
├── Cargo.toml              ✅ [workspace] members = ["core", "app"]
├── core/                   ✅ zuno-core — NO gpui dependency
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          ✅
│       ├── request.rs      ✅ RequestSpec, Method, Header, Body
│       ├── response.rs     ✅ ResponseData, Timing, SizeInfo, StatusClass
│       ├── engine/         ✅
│       │   ├── mod.rs      ✅ Engine handle, Command, Event, client cache
│       │   ├── error.rs    ✅ EngineError — owned, Clone, renderable
│       │   ├── build.rs    ✅ RequestSpec -> reqwest::Request (pure, unit-tested)
│       │   └── run.rs      ✅ execution, streaming, event emission
│       ├── json/           ✅
│       │   ├── mod.rs      ✅ JsonOutline, Row, Span, visible_rows
│       │   └── flatten.rs  ✅ iterative tokenizer -> Vec<Row>
│       ├── lines.rs        ✅ LineIndex for the raw-text fallback
│       ├── diff.rs         ✅ ResponseDiff — summary comparison of two runs
│       ├── curl.rs         ✅ curl command line <-> RequestSpec, both directions
│       ├── collection.rs   ✅ one-request-per-file on-disk format
│       ├── environment.rs  ✅ variables: two-layer resolution + on-disk format
│       ├── fuzzy.rs        ✅ subsequence scoring for the picker
│       └── search.rs       ✅ substring search over a response body
└── app/                    ✅ zuno — the GPUI binary
    ├── Cargo.toml
    └── src/
        ├── main.rs         ✅ bootstrap: window, keymap, theme, engine, boot timing
        ├── actions.rs      ✅ every keyboard-reachable verb, in one place
        ├── engine.rs       ✅ the Engine as a global (one pool per process)
        ├── body_view.rs    ✅ body classification + fold state
        ├── chrome.rs       ✅ titlebar + window controls + resize edges (CSD)
        ├── session.rs      ✅ window-session envelope, versioned + migrating
        ├── collections.rs  ✅ where collections live (a global, for tests)
        ├── picker.rs        ✅ the modal picker: filter + ranked list
        ├── commands.rs      ✅ the command palette's curated action table
        ├── settings_panel.rs ✅ per-request engine settings, as a modal
        ├── timing.rs       ✅ the ZUNO_TIMING switch, shared by boot and requests
        ├── theme.rs        ✅ Theme global; light + dark tokens; font resolution
        ├── ui.rs           ✅ icon set + asset source, icon/text buttons, tooltips
        ├── workspace.rs    ✅ root Render; owns buffers + all action handlers
        ├── request_view.rs ✅ one buffer: inputs + response + derived spec()
        ├── request_pane.rs ✅ method, URL bar, send, Headers/Params/Body tabs
        ├── response_pane.rs✅ status line, timing, body/headers tabs, body viewer
        ├── tests.rs        ✅ headless end-to-end tests (GPUI test platform)
        └── input/
            ├── text_input.rs ✅ single-line input primitive
            └── editor.rs     ✅ multi-line body editor
```

Three refinements the implementation forced, all worth recording:

- **`request_view.rs` is the buffer level** that §12's tabs hedge asked for. `Workspace` owns
  `Vec<Entity<RequestView>>` + `active_ix`; a `RequestView` owns one `RequestSpec`, its latest
  response, and the three focus handles. `request_pane` and `response_pane` are its two render
  halves — plain functions for now, promoted to entities in M1.1 when the request side grows
  state of its own.
- **All action handlers live on `Workspace`**, not on the panes. Action dispatch travels up the
  focus tree, and `Workspace` is the one element guaranteed to be on that path regardless of
  which region holds focus — including when focus is inside a `TextInput` nested two levels
  down. Handlers that need buffer state reach in through the entity.
- **There is no stored `RequestSpec`.** The `TextInput` entities own their text, and
  `RequestView::spec(cx)` assembles a spec on demand. The alternative — keeping a spec field
  and mirroring every keystroke into it — means two copies of every string and a desync bug
  waiting in each one. Deriving instead makes it structurally impossible for the request that
  goes on the wire to disagree with what's on screen. Non-text state (`method`, `body`,
  `settings`, per-row `enabled`) lives on `RequestView`, since nothing else owns it.

### Discoverability — keyboard-first is not keyboard-only

Added late, and the delay is the interesting part. The thesis is "speed and keyboard navigation are
requirements, not polish", and that quietly became *keyboard-only*: an audit counted **six of ~40
actions reachable by mouse, and nine with no affordance at all** — find, copy-as-curl, copy
response, save response, history, settings, import, save request, new tab. Every one of them had a
keybinding and a palette row, and neither is discoverable by looking at the window. A shortcut
nobody can find is a feature nobody has.

`app/src/ui.rs` holds the answer: an icon set, `icon_button`, `text_action`, and a tooltip. Four
decisions worth keeping:

- **The tooltip reads the live keymap** (`workspace::keybinding_label`), so the mouse path *teaches*
  the keyboard one instead of competing with it, and a rebinding can't leave a tooltip lying. That
  is the whole reason icons don't undercut the thesis.
- **Icons are embedded with `include_bytes!`**, not installed. Shipping SVG files would mean a new
  directory in the `.deb`, a path that differs between a cargo run and an installed binary, and a
  blank icon whenever the two disagree. Note this is the *opposite* choice from the application
  icon, which must be a real file in `hicolor/` precisely because the launcher — not Zuno — reads
  it. Same file type, opposite conclusion, because the reader is different.
- **`icon_button` stops propagation unconditionally.** One of these sits inside the drag-to-move
  titlebar, where a Bubble-phase click would also ask the compositor to start dragging the window —
  the bug the window controls shipped with for several milestones. Unconditional rather than
  per-site: harmless where no ancestor handles clicks, and impossible to forget when a button is
  later moved somewhere one does.
- **The `+` for a new tab lives in the titlebar, not the tab strip.** The conventional place is the
  end of the strip, but the strip hides itself at one buffer — so a button there would be missing in
  exactly the state where you want a second tab.

> **Three silent failure modes, one of which shipped.** gpui renders an SVG and keeps only its
> **alpha channel**, painting with `style.text.color`. So: a missing asset is swallowed by
> `log_err()`; a file that rasterizes to a transparent mask looks identical to a missing one; and an
> element with no `text.color` never reaches `paint_svg` at all. In every case the button keeps its
> bounds, its hover, its tooltip and its dispatch — only the pixels are absent.
>
> The third one shipped, and it is worth being precise about why. `icon_button` carried a comment
> explaining the rule, and then set `text_color` on the wrapping `div` — which an `svg()` does not
> inherit, because `compute_style_internal` starts from `Style::default()` and refines only with the
> element's own base style. The comment was correct and the code three lines below it was not.
> `ui::glyph` now takes the colour as a parameter, so the rule is enforced by the signature; hover
> reaches the glyph through `.group()` / `.group_hover()`, since `hover` doesn't inherit either.
>
> The two file-level modes *are* testable and now are: `every_icon_resolves_and_is_renderable_svg`
> loads each path through the real `AssetSource`, and `every_icon_rasterizes_to_visible_pixels`
> renders it with **gpui's own resvg version** — pinned deliberately, because a newer renderer could
> parse an icon that gpui's cannot and pass while the button is blank. The element-tree mode is not
> testable here at all; nothing in the headless platform can observe a paint. That asymmetry is why
> it became an API shape instead of a test.

> **And the audit found a bug while counting.** The `fold all` / `expand` buttons were calling
> `set_all_folded` directly instead of dispatching `FoldAll`/`UnfoldAll` — the same violation the
> body-kind chip was caught in, in the same file, found the same way. Two occurrences of one
> mistake is what promoted "actions, not direct calls" from a convention to something with a test
> per button.

**Why a workspace and not just modules?** Two concrete wins:

- The compiler enforces rule #1 — `zuno-core` cannot accidentally grow a GPUI import.
- `cargo test -p zuno-core` runs in ~1s instead of linking the entire GPUI stack. Your
  `target/` is already 3.6GB; the JSON flattener and the request builder are where most of
  your tests will live, and you do not want them gated behind a GPUI link every time.

---

## 3. The data model

### 3.1 Request

```rust
pub struct RequestSpec {
    pub id: RequestId,
    pub name: String,
    pub method: Method,
    pub url: String,               // RAW — may be invalid, may hold {{vars}}
    pub query: Vec<QueryParam>,    // ordered, individually toggleable
    pub headers: Vec<Header>,      // ordered, individually toggleable
    pub body: Body,
    pub settings: RequestSettings,
}

pub struct Header { pub enabled: bool, pub name: String, pub value: String }

pub enum Method { Get, Post, Put, Patch, Delete, Head, Options, Other(String) }

pub enum Body {
    Empty,                                     // named Empty, not None, so it never
                                               // reads as an Option at a match site
    Raw { text: String, kind: RawKind },       // Json | Text | Xml | Html
                                               // Stays a String — the rope was dropped
                                               // in M1.4, see §7
    Form(Vec<FormField>),
    Multipart(Vec<MultipartField>),
    Binary(PathBuf),
}

pub struct RequestSettings {
    pub timeout: Option<Duration>,
    pub follow_redirects: bool,
    pub max_redirects: u8,
    pub verify_tls: bool,
    pub accept_encodings: bool,
}
```

Three decisions here carry real weight:

**Headers are an ordered `Vec`, not a map.** An API client must send duplicate headers,
preserve the order you typed them in, and let you *disable* a row without deleting it — that
toggle is half of how people actually debug requests. A `HashMap<String, String>` makes all
three impossible, and it's the single most common way this model gets designed wrong.

**The URL stays a raw `String` in the model.** Users type invalid URLs on every keystroke,
and `{{baseUrl}}/users` will never parse. Validation and `Url` construction happen at the
send boundary in `engine/build.rs`, which returns a typed error the UI renders inline. The
model itself is never in an "unparseable" state because it never claims to be parsed.

**`Method::Other(String)`** — WebDAV, custom verbs, and typos all need to be sendable.

Derive `Serialize + Deserialize` on all of it now, even though M1 barely persists anything
(§7). It costs nothing today and keeps the storage decision cheap later.

### 3.2 Response

```rust
pub struct ResponseData {
    pub status: u16,
    pub status_text: String,
    pub version: HttpVersion,
    pub headers: Vec<Header>,      // ordered, exactly as received
    pub body: Bytes,               // raw wire bytes, post-decompression
    pub timing: Timing,
    pub size: SizeInfo,            // declared (Option) vs decoded — see the limitation below
}

pub struct Timing {
    pub dns: Option<Duration>,
    pub connect: Option<Duration>,
    pub tls: Option<Duration>,
    pub ttfb: Duration,
    pub total: Duration,
}
```

`body: Bytes` is load-bearing. It makes the body cheap to clone into a background task, and
it's what lets the JSON viewer hold *byte spans* instead of copied strings (§6).

**Three limitations found in the response model**, all inherited from reqwest:

- **Response header order is not wire order.** `http::HeaderMap`'s iteration order across
  different names is an implementation detail. Duplicates of the *same* name do stay in
  received order, so `collect_headers` stable-sorts by name: deterministic and readable,
  without scrambling duplicates. True wire order needs a lower-level client than reqwest.
- **`Timing.dns` / `connect` / `tls` stay `None`.** reqwest exposes no per-stage connection
  timings; getting them needs a custom hyper connector. `ttfb` and `total` are real. This is
  exactly why those three were typed as `Option` from the start rather than `Duration`.
- **The wire size is unknowable, so the compression ratio cannot be shown.** This section used to
  claim the opposite — that "wire vs decoded" is how you spot whether compression happened. It
  isn't. reqwest 0.13 delegates decompression to `tower-http`, which removes `Content-Encoding` and
  `Content-Length` *together* the moment it decodes a body, so the declaration is absent exactly
  when it would have been interesting. `SizeInfo::declared` is therefore an `Option` — the same
  admission the `Timing` fields above make, for the same reason — and it holds the server's claim
  rather than a measurement. Where the two numbers can differ is a `HEAD` or `304`: a length
  declared with no body behind it. Pinned by
  `a_compressed_response_is_decoded_and_reports_no_declared_length`, so a future reqwest that keeps
  those headers shows up as a test failure rather than as a silent chance missed.

---

## 4. The send loop

This is the part where GPUI and HTTP have to be introduced carefully.

**The problem:** GPUI's executor is smol-based. `reqwest` needs a tokio reactor. (Tokio
1.53 is already in your lockfile transitively — `gpui` pulls it via `zed-reqwest` — so this
adds weight you're already carrying.)

**The solution:** the engine owns a dedicated tokio runtime on its own thread. The UI never
awaits an HTTP future directly; it consumes an event stream over a channel.

```
  UI thread (GPUI / smol)                Engine thread (tokio)
  ───────────────────────                ─────────────────────
  Send action
    └─ engine.send(spec) ───── Job ────▶  build request
         → (JobId, Receiver<Event>)         └─ execute, stream body
                                                   │
    ◀────────── smol::channel ──────────────────────┘
    cx.spawn(async move |this, cx| { … })
      per event: this.update(…) + cx.notify()
```

```rust
pub struct Engine { /* job tx + runtime handle */ }

impl Engine {
    pub fn send(&self, spec: RequestSpec) -> (JobId, Receiver<Event>);
    pub fn cancel(&self, id: JobId);
}

pub enum Event {
    Started,
    Connected(Timing),                  // DNS/connect/TLS known
    Head { status: u16, headers: Vec<Header>, ttfb: Duration },
    Progress { received: usize, total: Option<usize> },
    Done(ResponseData),
    Failed(EngineError),
}
```

**Why a stream of events and not a single `Task<Result<ResponseData>>`?** Because the *feel*
lives in the intermediate states. `Head` lets the status line and headers paint at TTFB
instead of after the last byte. `Progress` gives a 50MB download a moving indicator rather
than a frozen window. A single future can only ever render "spinner, then everything."

UI side — note the exact 0.2.2 signature, `AsyncFnOnce(WeakEntity<T>, &mut AsyncApp)`:

```rust
fn send(&mut self, _: &Send, _window: &mut Window, cx: &mut Context<Self>) {
    let (id, rx) = self.engine.send(self.spec.clone());
    self.inflight = Some(id);
    self.task = Some(cx.spawn(async move |this, cx| {
        while let Ok(event) = rx.recv().await {
            this.update(cx, |this, cx| {
                this.apply(event);
                cx.notify();
            })?;
        }
        anyhow::Ok(())
    }));
}
```

**Cancellation has two halves,** and both are needed: dropping the `Task` stops the UI from
consuming events, but the socket keeps draining until you also call `engine.cancel(id)`.
Wire a cancel key and a re-`Send` to do both — an in-flight request must be abandoned the instant
you hit send again, or rapid resend feels laggy for reasons the user can't see.

> This sketch originally said `Ctrl+C`, and it shipped as `Escape` — `ctrl-c` belongs to
> `text_input::Copy` and a global binding would fight it. The stale sentence outlived the decision
> and turned into a user-facing one: the in-flight pane read "Ctrl+C or Escape to cancel" for
> several milestones. The hint is now read from the keymap via `workspace::keybinding_hint`, the
> same way the command palette gets its shortcuts, so it cannot name a key that isn't bound.

**API note:** gpui 0.2.2 has **no `cx.background_spawn`** (that's newer Zed-main API). Use
`cx.background_executor().spawn(fut)`. Anything CPU-bound — JSON parse, flatten, pretty-print,
size computation — goes there, never in `cx.spawn`.

---

## 5. Keyboard and focus architecture

Set this up on day one. Retrofitting focus contexts is genuinely painful.

```rust
actions!(zuno, [
    Send, Cancel, FocusUrl, FocusBody, FocusResponse,
    ToggleMethod, NextBodyTab, PrettyPrint, CopyResponse,
]);
```

`KeyBinding::new(keystrokes, action, context: Option<&str>)` takes a **context predicate** —
use it from the start. `Enter` must mean "send" in the URL bar and "insert newline" in the
body editor; that distinction is the context predicate's whole job, and it's unfixable later
if every binding is registered globally.

```rust
cx.bind_keys([
    KeyBinding::new("ctrl-enter", Send,       None),          // global
    KeyBinding::new("enter",      Send,       Some("UrlBar")),
    KeyBinding::new("ctrl-l",     FocusUrl,   None),
    KeyBinding::new("escape",     Cancel,     None),
]);
```

> **Linux gotcha, worth calling out because it will cost you an hour:** `examples/input.rs`
> ships macOS bindings (`cmd-a`, `cmd-v`, `cmd-c`). On Linux `cmd` never fires. When you
> adapt that example, translate every `cmd-` to `ctrl-`. Better: define a `mod_key()` helper
> now so the eventual macOS build isn't a find-and-replace.

---

## 6. The response viewer — the real engineering problem

GPUI gives you `uniform_list(id, item_count, |range, window, cx| -> Vec<impl IntoElement>)`.
It renders only the visible range, but it demands one thing: **an O(1)-indexable flat list of
fixed-height rows.** A JSON *tree* is not that. So the core's job is to turn a tree into a
flat index, off-thread, once.

```rust
// core/json/mod.rs — the stable interface
pub struct JsonOutline {
    source: Bytes,
    rows: Vec<Row>,
    visible: Vec<u32>,     // visible-index -> rows index (fold support)
}

pub struct Row {
    depth: u16,
    kind: RowKind,          // ObjectOpen | ArrayOpen | Entry | Close
    key: Option<Span>,      // byte range into `source`
    value: Option<Span>,    // byte range into `source`
    subtree_len: u32,       // rows to skip when folded
}

impl JsonOutline {
    pub fn parse(source: Bytes) -> Result<Self, JsonError>;  // background only
    pub fn visible_len(&self) -> usize;
    pub fn row(&self, visible_ix: usize) -> RowView<'_>;
    pub fn toggle_fold(&mut self, visible_ix: usize);
}
```

**`Span` (a byte range), not `String`.** Rows point into the original `Bytes`. For a 50MB
response this is the difference between ~50MB and several hundred MB of resident memory,
and it eliminates millions of small allocations during the flatten pass.

**Folding** is `visible: Vec<u32>` rebuilt on toggle, using `subtree_len` to skip folded
ranges. O(rows), which is fine — and moved to the background executor above a threshold.

**On the parser — this plan was wrong.** The original idea was "start with
`serde_json::Value`, swap in a span-emitting tokenizer when it hurts". That was never viable:
**`Value` discards byte offsets**, and every `Span` above depends on them. A position-tracking
parser wasn't a later optimisation, it was the only way to build this at all. `flatten.rs` is
therefore a hand-written tokenizer from the start — ~330 lines, and **iterative rather than
recursive**, because a viewer eats arbitrary server output and deeply nested JSON is a trivial
way to blow a recursive parser's stack (there's a 50,000-deep test).

It is deliberately **permissive about string contents and strict about structure**: `\u`
escapes aren't validated beyond "a byte follows the backslash", because this is an inspector,
not a validator, and refusing to display a response over a malformed escape is unhelpful.
Structural errors *are* rejected, since flattening them would produce nonsense.

**Set a hard, visible cap.** Above ~10MB, default to a raw/line-oriented view with an explicit
"parse as JSON anyway" affordance. Pretty-printing a 200MB body is a bad idea at any speed.
Whatever the cap is, *say so in the UI* — a silently truncated response reads as a wrong
response, and that's a trust bug, not a perf one.

**Two caps, answering different questions — and only one of them existed for a long time.** The one
above is a *display* cap (`body_view::MAX_AUTO_PARSE`, 10MB): past it the body is shown as raw text
with a button, and every byte is still held. It says nothing about whether the body should have been
held at all, and nothing did — `run.rs` collected the stream into an unbounded `Vec<u8>`, so a URL
pointing at a release artifact instead of an API endpoint buffered the lot, with `HISTORY_LIMIT`
retaining up to eleven of them per buffer. `run::MAX_BODY_BYTES` (100MB) is the *transfer* cap.

Unlike the display cap it **fails rather than degrading**, which is the opposite of `MAX_DISPLAY_LINE`
and deliberate: a truncated body is not the response, so `SaveResponse` would write a corrupt file
from it and the viewer would report a parse error at the cut. Being told the transfer was refused
beats both. It is checked twice — against a declared `Content-Length` before any body moves, which
is the cheap half, and again while streaming, because a declared length is a claim and a chunked
response makes none. The limit is a *parameter* of `run::execute` rather than a constant read inside
it, so the streaming guard can be tested with a 64KB limit instead of pushing 100MB through a
socket.

**The viewer is read-only.** This is what makes M1 tractable: rendered rows plus
selection-for-copy, no editing, no IME. All the editor complexity is confined to the request
side.

> This sentence said "no cursor" until row selection landed, and the distinction it was
> reaching for is worth keeping rather than deleting: there is a **row** cursor now, and still
> no *text* cursor. Nothing addresses a character, nothing has a selection anchor, and no
> element accepts input — so none of §7's cost arrives. See "Selection" below.

### The pane is tabbed, and that was a bug fix rather than a feature

The headers table was rendered inline *above* the body, unbounded, in a pane that is
`overflow_hidden` and has no scroll anywhere. Everything above was carefully virtualized and
the one un-virtualized list was the one that broke the layout: a Cloudflare-fronted response
carries two dozen headers, ~620px of them, which pushed the body region past the bottom edge.
Not merely small — **unreachable**, since there was nothing to scroll.

So `Body` and `Headers` are now tabs, `Body` default because it's the answer you sent the
request to get. Four decisions:

- **The status line, the historical notice, and the diff bar sit *above* the tabs**, because
  they describe the response as a whole. The notice especially: it exists so the pane can't be
  mistaken for the live run, and putting it inside the Body tab would have recreated exactly
  the confusion it was added to prevent.
- **The header count rides on the tab label** (`Headers 24`). It's the one thing hiding the
  table costs you — without it there's no way to tell a two-header response from a thirty-header
  one without switching.
- **The choice is per-`RequestView` and sticky.** Deliberately *not* the history browser's
  "sending returns you to live" rule: watching one header change across sends is the reason to
  be on that tab, so snapping back on arrival would undo the thing you were doing. Per-buffer
  because two requests are open for different reasons.
- **Only the inactive tab is clickable.** One cycling action serves both tabs, so a handler on
  both would make clicking the tab you're already on switch *away* from it. Leaving the active
  tab inert makes "click a tab, land on that tab" true — and it works only because there are
  exactly two. A third tab has to split this into per-tab actions.

The Headers tab scrolls rather than virtualizing. Header counts are tens, and `uniform_list`
would impose the fixed row height that the rest of this section is built on, which is the wrong
constraint for values that ought eventually to wrap.

**The request pane is tabbed too now, and the third tab is why it isn't the same code.** Headers,
query and body used to stack, so the two sections you weren't editing still cost a header row and
an empty-state row apiece — about 130px to say "nothing here" — while the body editor got whatever
was left. `Headers │ Params │ Body` (`Alt+Q` forward, `Alt+Shift+Q` back), each with the slim
control row that used to be its section header, and the four request verbs at the far end of the
strip where the response pane keeps its own.

Four decisions, and the first is the one that matters:

- **Three tabs need three actions.** The bullet above about only the inactive tab being clickable
  *predicted* this: cycling works for two because the single inactive tab is always one step away,
  and with three, clicking Body while on Headers is two steps — so a cycling handler sends the
  click to Params. `clicking_a_request_tab_lands_on_that_tab` fails with exactly that, `Query`
  where `Body` was asked for. Hence `ShowHeadersTab`/`ShowParamsTab`/`ShowBodyTab` alongside the
  cycle pair. The active tab stays inert, which still earns its keep: a click that dispatches its
  own tab is harmless but advertises a change that never comes.
- **Cycle order is visual order, not most-recently-used.** `Alt+Tab`'s actual behaviour was the
  brief, but MRU on a fixed three-item strip means one keystroke lands somewhere different each
  time and destroys the muscle memory the strip gives for free. (`alt-tab` is also unbindable —
  the compositor's window switcher takes it before Zuno sees the key.)
- **Every verb that acts on a hidden section reveals it first.** `Ctrl+Shift+H` on the Body tab
  would otherwise add a header you cannot see, which reads as a dead keystroke; the same applies
  to query rows, all three body verbs, and `FocusBody`, where focusing an unpainted editor is the
  "keymap goes dead with nothing on screen" failure. This isn't a new rule — it's what
  `Ctrl+Shift+F` already did by switching the body to a form. It's also *already tested*: deleting
  the reveal from `add_header` alone fails nine pre-existing tests, because they all reach header
  cells by pressing that key.
- **Sticky per buffer with Body default**, matching the response pane rather than the history
  browser's snap-back-to-live rule, and for the same reason: watching one section across sends is
  the reason to be on it.

`RequestTab::Query` is labelled **Params**. The label is the only place the word changes —
`RowKind::Query` and `RequestSpec::query` keep theirs, because that serde field name is in every
saved collection file and renaming it would fail them with `missing field query`, which is the
`cookie_store` lesson exactly.

**Tab traversal deliberately shrank.** A hidden tab isn't painted, so its `TextInput`s are no
longer tab stops: `Tab` walks the active section instead of every row in the pane. That is the
intended consequence, not a side effect — but it is the sort of change that silently alters focus
order, which is why it's recorded here.

### Horizontal scrolling — and the one-row measurement that decides it

Soft-wrap is off (§7), so a long line runs off the right edge. Until this landed there was
nowhere for it to go: the response body had no horizontal scrolling at all *and* no cursor to
fake one, so anything past the pane's width was unreachable rather than merely awkward. The
request editor was better off only by accident — its `h_offset` follows the caret, so `End`
reached the text a trackpad could not.

**Wrapping was never an option here, whatever one's taste.** `uniform_list` demands
O(1)-indexable fixed-height rows, which is the entire premise of this section; a wrapped row has
a height that depends on its content and on the viewport width. Wrap in the response viewer means
a different element and a different virtualization strategy, not a setting.

- **The content width comes from a single sampled row.**
  `with_horizontal_sizing_behavior(Unconstrained)` looks like the whole fix and is half of it:
  `measure_item` measures *one* row, the one named by `with_width_from_item`, which defaults to
  index 0. Row 0 of a JSON document is `{`. So the obvious version switches horizontal scrolling
  on and gives it nothing to scroll to, which is indistinguishable from the feature not working.
  `BodyView` therefore computes the widest row while indexing — background executor, invariant 3
  — and hands over its *visible* index.
- **The widest row is estimated, not measured.** Character counts weighted by depth, because
  gpui shapes whatever row it is given and takes the real pixel width from that; this only has to
  pick the right index. It works because the viewer is monospace, which is the one assumption
  that would make it wrong elsewhere. The raw view has the same problem and a different index,
  `LineIndex::widest_line` — measured **as drawn**, so a minified megabyte on one line doesn't
  size the scroll region to a megabyte of blank space past the display cut.
- **The extent follows folding, and that is not a detail.** It is recomputed over the *visible*
  rows on every fold, in `rebuild_visible` — the same funnel the selection clamp uses. Computed
  once at index time over the whole document instead, collapsing a response left the scroll
  region as wide as the longest row it used to show, so the view stayed parked in blank space
  with nothing out there to find. Folding is *how* a wide response is made readable, so an extent
  that ignores it defeats the feature it is part of. Once the extent shrinks, gpui's own prepaint
  clamp pulls the offset back with it, which is why the fix is one place and not two.
- **The indicator is a `UniformListDecoration`, and it has to be.** A scrollbar drawn as a
  sibling `div` reads `max_offset` and `bounds` off the scroll handle — both written during
  `interactivity.prepaint`, which runs *after* the surrounding tree is built. So it draws nothing
  on the frame the body first appears and then waits for an unrelated repaint to show up.
  Decorations are computed inside that same prepaint and laid out at the list's own bounds.
  `a_wide_body_shows_the_scroll_indicator` fails against the sibling version.
- **It is an indicator, not a control.** Three pixels, no hover, no pointer cursor. Dragging
  would mean mirroring the track geometry onto the view plus a drag mode, to duplicate a gesture
  the trackpad, the wheel and `left`/`right` already perform — and a thing that looks draggable
  and isn't is the dead-control bug this codebase keeps finding. The answer is to not look
  draggable.
- **The headers tab does not scroll sideways at all — it wraps.** It briefly did scroll, by
  making the container a flex row so the table could exceed it, and that was wrong twice over: a
  short table stopped filling the pane, and scrolling carried the *name* column off the left edge
  so you lost track of which header you were reading. Dropping `truncate` from the value lets it
  wrap instead, which is the answer this section named for the tab in the first place — it is not
  virtualized and header counts are in the tens, so a variable row height costs nothing. That is
  precisely what the body cannot do, being virtualized on fixed-height rows.
- **A sideways swipe must not drag the document vertically, and `stop_propagation` cannot stop
  it.** The container's wheel handler gates on `hitbox.should_handle_scroll`, which only
  hit-tests and never consults propagation, so it runs whatever a listener does. Declaring *both*
  axes `overflow_scroll` on the editor is the fix: `allow_concurrent_scroll` is false by default,
  so with two non-zero deltas gpui zeroes the smaller axis itself. The x axis is never actually
  scrolled there — the element is `relative(1.)` wide — but declaring it is what makes gpui look
  at `delta.x` at all.
- **`left`/`right`/`home` scroll, scoped to `ResponsePane`.** `up`/`down` already move the row
  selection there, so this completes that idiom rather than inventing one. In the editor a wheel
  writes `h_offset` directly, which composes with the caret clamp instead of fighting it: prepaint
  starts from the previous offset and overrides it only when the caret would otherwise be
  off-screen, so a manual scroll survives until you type. Shift-wheel is translated to the
  horizontal axis by hand — a trackpad reports a real `x` delta, a wheel mouse reports `y` and
  leaves the convention to the application.

> **Four guards were written and then deleted, because none could be shown to do anything.** A
> clamp on the response body's scroll offset — gpui re-clamps `scroll_offset.x` to `[-max, 0]` on
> every prepaint, from a content width fresher than any the caller holds. `min_w(100%)` on the
> rows in place of `w_full`, on the reasoning that a fixed width caps a row at the viewport and
> clips it: it doesn't, because the list lays each row out with
> `available_width = viewport + |scroll_x|` and shifts the row origin by `scroll_x`, so a 100%
> row spans exactly the visible region at every offset. And two on the headers tab — making the
> value cell `flex_none` and dropping its `truncate`, and marking the table `flex_none` — where
> only the container becoming a flex row mattered.

**This section describes the second attempt.** The first shipped broken on every surface it
touched, with a green suite throughout, and the failure was uniformly in what the tests asserted:
`max_offset > 0` and "the offset changed" are both true of a region sized to the wrong row, a
scrollbar painted along the top edge, a thumb travelling the wrong way, and an editor that snapped
back to column zero on the next frame. Four things it got wrong, each now asserted at the
consequence:

- **The headers tab could not scroll at all**, and `overflow_scroll` could never have made it.
  See below.
- **The scrollbar was drawn along the top edge and slid out of the viewport.** A decoration is
  laid out as a *root* at the list's origin, so a 3px-tall element with `bottom_0` puts the bar at
  the bottom of its own 3px box. And it is a child of the list, so gpui translates it with the
  content — on **both** axes. The bar is now placed by arithmetic off the `bounds` `compute` is
  given, cancelling both. Two tidier versions each pinned one axis and broke the other:
  `justify_end` ignores a top margin on the child, since flex end-alignment pins it regardless;
  and a `relative` root with an `absolute` child lost the horizontal pin. The vertical half went
  unnoticed for a round because the bug only appears once the body is tall enough to scroll down,
  and the test used `down` — which moves the row *selection* and scrolls nothing until the
  selection leaves the viewport.
- **The scroll region was sized from the wrong row, then in the wrong font.** The per-character
  advance in `widest_json_row` was 7.0 against a measured ~8.5, which over-weighted depth enough
  to rank a deep-short row above a shallow-long one. Correcting it was not enough: `measure_item`
  runs *before* `interactivity.prepaint` pushes the list's text style, so gpui shaped the sampled
  row in the ambient font rather than the `mono`/`text_xs` it draws in. The pane now computes the
  width itself from an advance measured in the render font, and the rows carry that style so any
  measurement of them is taken in it.

  **This one is barely testable and the docs should say so.** The headless platform's ambient
  font measures *wider* than the render font, so the bug made the region too large there and
  every assertion passed; in the real window it went the other way and the line's end stayed out
  of reach. `the_widest_row_can_actually_be_reached` brackets the total from both sides, which is
  a font-metric assertion and fragile on purpose — it is the only thing that can distinguish
  which font decided the answer.
- **The editor was worst.** Its clamp bounded `h_offset` by the width of the *cursor's* line, so a
  caret parked on `{` gave a maximum of zero and any scroll snapped home on the next frame; and
  when that line scrolled out of view the clamp never ran at all, so the text could be pushed
  arbitrarily into blank space. Two opposite bugs from one wrong reference line. It now bounds
  against the document's widest line — stable, unlike §7's rejected "widest *visible* line" — and
  follows the caret only when the caret has actually moved.

### Search — over the bytes, not over what's drawn

`Ctrl+F`. The decision that shapes everything else: `core/src/search.rs` scans the **source
bytes**, not the rendered rows.

Searching what's drawn is the obvious choice and it's wrong twice. The match count would depend
on the fold state, because a folded container renders as `{ … 3 items }` and its contents aren't
on screen at all. And the raw fallback truncates every line at `MAX_DISPLAY_LINE`, so anything
past 4KB on a line — which for minified JSON is the whole body — would silently not be findable.
The bytes are the one answer that doesn't move.

That choice pushes the work onto *reaching* a match, which is where the interesting parts are:

- **Offset → row is a merge, not a binary search, and only for JSON.** A row's source position
  isn't stored: `Row` carries spans for its key and scalar value, but an open row inside an array
  and every close row have neither — the tokenizer consumes `{ [ } ]` without recording where.
  Adding a `start: u32` field would grow `Row` by 4 bytes, which is 5MB across the 1.31M rows a
  10MB body produces, to serve one caller. So `rows_for_offsets` reconstructs positions in one
  forward walk, where a spanless row inherits **where the previous row ended**. Inheriting its
  *start* instead was the first version, and it put every trailing close row on top of the last
  scalar — so a match nested four deep resolved to the outermost `}`, wrong by exactly the
  nesting depth. `LineIndex` needs none of this: every line has a recorded span, so it binary
  searches.
- **Jumping unfolds.** A match inside a folded subtree has no visible row, so `BodyView::reveal`
  opens the target's ancestors — found by forward scan, since `Row` records no parent — and only
  those. Unfolding everything would discard the collapsing someone did to make the response
  readable in the first place.
- **`uniform_list` addresses items by visible index, not row index.** With anything folded above
  the target the two diverge, and `scroll_to_item` with a row index scrolls somewhere else or
  past the end. `reveal` returns the translated index for exactly this reason.
- **Three notices, because the honest count and the useful count differ.** The scan stops at
  `search::MAX_MATCHES` (5000) — searching 10MB for `"` finds ~2M occurrences, and a `Vec` of
  them costs 8MB — so the bar says `first 5000 only` rather than letting a capped count read as a
  total. It says `past this line's display limit` when the current match sits beyond the raw
  view's cut, because the row is on screen and the match isn't. And on JSON it says `matching raw
  bytes`, since a key includes its quotes and structural whitespace is searchable.
- **Smart case**, matching every editor: an all-lowercase needle is case-insensitive, one
  uppercase character makes the whole query case-sensitive. Folding is ASCII-only — doing it
  properly means decoding UTF-8 per candidate, and a body isn't guaranteed to be UTF-8 at all.

Measured in release on 10MB / 1.31M rows: a **full-body miss in 6.9 ms**, a capped hit scan in
423 µs, and the offset-to-row mapping in 148 µs. So the scan comfortably fits a frame — and still
goes to the background executor, because the transfer cap is 100MB and invariant 3 isn't
conditional on today's body being small. Only the mapping runs on the UI thread, and it has to:
it reads the live `BodyView`, which a background task can't borrow.

**Match highlighting is per row, not per character.** A row is assembled from separately styled
key, punctuation, and value elements, so highlighting the exact bytes means splitting a shaped
text run — that's the syntax-highlighting problem, which principle 4 puts last.

### Selection — a row cursor, and the two verbs that need one

`Ctrl+F` built a cursor the *search* drove; this is one the reader drives. `up`/`down` step it,
a click places it, and `Ctrl+C` / `Alt+C` copy the row's value or its path. It closes the last
item on ROADMAP's egress list, which had been "row selection first, then copy" through two
slices.

Five decisions, and the first three are all the same mistake avoided in different places.

- **The selection is a *row* index, not a visible one.** Folding rewrites `visible` underneath
  it, so a visible index would silently retarget the selection at whatever row slid into that
  slot. The translation to a visible index happens at the two moments that need one — scrolling
  and rendering — and nowhere else.
- **Folding the container you are standing in moves the selection to the container.** The row
  leaves `visible` entirely, and a selection nothing paints is a cursor the reader has lost: the
  next `down` jumps from wherever it secretly still was. Every rebuild of `visible` goes through
  one `rebuild_visible`, so the clamp cannot be forgotten by one of the three callers — the same
  funnelling argument as `Workspace::activate`.
- **It is deliberately *not* the search cursor.** A match is where the search is; a selection is
  where you are, and a row is routinely both. Sharing one would mean stepping through matches
  drags a selection the user placed. They render distinctly and stack: the accent bar says "this
  is a match", the fill says "you are here".
- **Copy value is decoded, not merely unquoted.** `json::unquote` turns the source token into
  the string it denotes. Stripping the quotes alone is the tempting middle option and the worst
  of the three: a value holding `\n` pastes as a backslash and an `n`, it *looks* decoded, and
  nothing says otherwise. Copying the token verbatim would at least be honest. It is permissive
  in the same way `flatten` is — a broken escape passes through as written, because an inspector
  that mangles a response in order to show it is worse than one that shows it raw. Non-string
  scalars and containers pass through untouched, which is why no call site matches on
  `ScalarKind`.
- **On an open or close row, copy value gives the whole container.** A `{` is a row you can land
  on, so the verb has to mean something there or it is dead on roughly half a nested document.

**Reconstructing a container's braces is the one genuinely awkward part**, and for the reason
`rows_for_offsets` already documents: the tokenizer consumes `{ [ } ]` without recording where.
`value_span` walks forward from the start of the document resolving each structural token against
the source, stopping at the container's own close row — so a small object near the top costs a
short walk and only the root costs a full one.

The walk carries a **cursor past each brace as it is resolved**, and that is the whole
correctness of it. `row_bounds` treats a close row as a zero-width point, so several nested `}`
all inherit the same position; scanning forward from it finds the *innermost* one, and the
container comes back one brace short per level of nesting. That was the first version. The test
that catches it asserts on the **root of a nested document** — a flat object passes either way,
which is exactly the weak-assertion shape `CLAUDE.md` tracks.

Between tokens the scan crosses whitespace, `:` and `,` and nothing else; reaching any other byte
means the reconstruction has lost the thread, and it gives up rather than returning a range that
would copy the wrong text.

**Paths are JSONPath, built top-down from `ancestors_of`.** Consecutive pairs in that chain are
parent and child, and the *parent* decides how the segment reads: an object member by the child's
key, an array element by counting siblings — hopping whole subtrees, since a nested container is
one element however many rows it spans. Counting rows instead yields `$.users[4]` where `[1]` was
meant.

A bracket segment carries the key's **source token verbatim, quotes and escapes included**. That
is already valid JSONPath, so the one place a wrong path could be produced is the one place this
does no work. Only a plain ASCII identifier takes the `.name` form; anything uncertain takes
brackets, because the bracket form is always correct.

A close row reports the path of the container it closes rather than nothing — `}` is a row you
can land on, and refusing it would make the verb look broken on every third row.

**No path in the raw view**, where there is no structure to name a position within. The
affordance is *absent* there rather than inert, and the keystroke says why. Copy value still
works and gives the **whole** line, not the `MAX_DISPLAY_LINE` truncation the viewer draws —
`LineIndex::full_line` exists for exactly that one call site, and the test that holds it uses a
line past the cut, because a short one makes `line` and `full_line` indistinguishable.

**The verbs are reached by right-click, and the toolbar labels are gone.** `value` and `path`
sat in the response pane's action row for one slice, rendered only once a row was selected — so
the mouse path was findable only by someone who already knew the keyboard path. That is the
discoverability audit's own finding one level down, and it shipped. A right-click is a blind
reflex, which is what makes it the right gesture; the labels were removed rather than kept
alongside, since two paths to the same verb bought nothing and the action row stopped shifting as
the selection changed.

Double-click folds a container, the file-tree convention. Deliberately *not* the menu gesture: the
text inputs already use double-click for select-word and triple-click for select-line, and one
gesture meaning two things across panes is worse than a menu nobody finds.

`app/src/context_menu.rs` is a **primitive**, not a response-pane feature, for principle 2's
reason — saved requests want delete/rename, the tab strip wants close/rename, header rows want
toggle/remove. It is also the first genuine consumer of `anchored()`, a question §12 left open
twice: the picker chose modal, then the method dropdown turned out not to want anchoring either. A
menu settles it, because appearing where you clicked *is* the feature.

Five decisions:

- **Owned by `Workspace`, beside the picker and the settings panel.** It has to be. `modal_open`
  cannot see a menu the view owns, and the response pane is `overflow_hidden`, which masks an
  absolutely-positioned child just as it would any other — a menu opened near the bottom of the
  pane would be clipped by it.
- **The click position travels on the view, not in the action.** A data-carrying action needs
  `build(serde_json::Value)` and therefore `schemars`, which is a dependency for one `Point`. So
  the row parks the position on its `RequestView` and `OpenRowMenu` carries nothing; the handler
  `take`s it, so a stale anchor can never place a later menu.
- **Items adapt rather than disable.** No path on a raw body, no fold on a scalar. A greyed row
  that can never apply is noise in a menu this short — the same rule the pane already followed.
- **Every item is an action**, with its keystroke read from the live keymap. So the menu teaches
  the shortcut instead of replacing it, and it cannot drift from what dispatch does.
- **Fold became one verb on the selection.** `ToggleFold` acts on the selected row rather than
  taking an index, because all three surfaces that reach it — chevron, double-click, menu — select
  first. The chevron used to call `toggle_fold` directly; three surfaces is where "actions, not
  direct calls" stops being a style preference, and this is the third time that convention has
  been caught (after the body-kind chip and the fold-all buttons).

> **The chevron still does not stop propagation**, now for a third reason on top of the two above:
> the row handler's select is what makes a verb with no index possible at all.

**And the rows became hitboxes, which they were not.** Both body row builders called `.flex()`
without `w_full()`, so each row was as wide as its own text inside a full-width list — the
picker's bug (§12), in the surface with 1.31M rows. It had been merely ugly while the only click
target was the 12px fold chevron: the search highlight ended mid-row. Adding a click target is
what would have made it a dead-control bug, so the fix and the feature are one change. The test
clicks the far right of the list and is measured against the **list's** bounds, since the row's
own bounds agree with the bug.

> **The chevron deliberately does *not* stop propagation, against the usual rule.** A clickable
> nested in a clickable normally needs `cx.stop_propagation()` (§2, and the window-controls bug).
> Here the ancestor's effect is wanted — folding a container and standing on it is one intent —
> but that is not the reason it had to go. `track_focus` transfers focus by registering an
> **ordinary Bubble-phase mouse listener**, so stopping propagation suppressed *that* as well:
> clicking a chevron folded correctly, left the pane unfocused, and the next arrow key did
> nothing. Found by probing rather than by reading, and it is also why `select_body_row_at`
> moves no focus of its own — an explicit `window.focus` there was dead code, proved by deleting
> it and watching the test still pass.

### `TextInput` emits `Changed`

Search is why. The picker used to notice typing by storing the query it last ranked and comparing
it every frame, with a comment saying it did so only because the input emitted nothing. That
works for re-ranking a few dozen rows synchronously; it's the wrong shape for *spawning a
background task*, and it's a mirror of state the input already owns. `TextInput` now emits from
the two methods that mutate content — which between them are every edit path, since backspace,
delete, paste and cut all route through `replace_text_in_range` and the IME through
`replace_and_mark_text_in_range`. The picker was migrated onto it in the same slice, so there is
one mechanism rather than two.

---

## 7. Text input — the biggest hidden cost

Be clear-eyed about this: **gpui 0.2.2 does not ship a text editor.** `src/input.rs` contains
only `EntityInputHandler` and `ElementInputHandler` — the IME/platform plumbing. The reference
implementation is `examples/input.rs`, and it is **746 lines for a single-line input** with
cursor, selection, mouse drag, clipboard, and IME.

M1 plan, in cost order:

| Piece | Scope | Approach |
|---|---|---|
| `TextInput` (single-line) ✅ | URL bar, header cells, param cells | Adapted from `examples/input.rs`; every `cmd-` translated to `ctrl-` |
| `Editor` (multi-line) ✅ | Request body only | Same input handler; **no rope** — see below; soft-wrap off |
| Response body | — | Read-only rows; **no editor at all** |

**Six deliberate changes from the upstream example**, five made while adapting it in M1.1 and one
found later by audit:
theme-driven colors instead of hardcoded literals; text style *inherited* from the parent div
(which is what lets one `TextInput` serve both the URL bar and the tiny table cells);
a caller-supplied key context identifier (see §10's note on leaf-only predicate matching);
newline sanitization moved into `replace_text_in_range` so it covers the IME and drop paths and
not just paste; and `character_index_for_point` returning `None` instead of asserting — the
example's `assert_eq!(last_layout.text, self.content)` panics whenever the placeholder is
showing, because an empty input lays out placeholder text rather than content.

The sixth: **the composed-selection offset adds `range.start` to both ends**, where the example adds
`range.end` to the end. That overshoots by the width of whatever was replaced, so an IME replacing a
non-empty range leaves a selection running past the end of the content — which `copy` and `cut` then
slice with, and panic. Invisible while `range.start == range.end`, which is every ordinary
insertion, and that is why it survived being copied in. `editor.rs` had it right all along; the two
had silently disagreed since M1.4.

**Word-level movement was simply missing, and it was missing everywhere.** `Ctrl+Left`/`Right`
and their shifted pair did nothing in the URL bar, in every table cell, in the find bar, or in the
body editor — the upstream example has no word movement and nothing added it. `input::{prev,next}_
word_boundary` now backs all four actions, and lives in `input/mod.rs` **shared by both entities
rather than implemented twice**: two definitions of "a word" would drift, and the URL bar would
expose it immediately since a URL is mostly punctuation.

Three characters classes — whitespace, word (alphanumeric plus `_`), and everything else — with the
runs between them as the boundaries. That is what makes `https://api.example.com` step
`https` → `://` → `api` rather than jumping the whole string, which a whitespace-only rule would.
Movement is deliberately **asymmetric**, matching every code editor: `Ctrl+Right` stops at a word's
*end*, `Ctrl+Left` at its *start*.

One binding each, scoped to `Some("TextInput")`, serves both surfaces — the editor receives them
because its own leaf context string is `"TextInput BodyEditor"`, not through nesting (§10's note).
That is also the failure mode the keystroke test exists for: scoping them to the wrong identifier
compiles and silently does nothing in the editor while still working in the URL bar.

**And then the rest of that audit's list landed**, all of it shared between the two entities the
same way: word deletion (`Ctrl+Backspace`/`Delete`), document ends (`Ctrl+Home`/`End` — distinct
from `Home`/`End`, which stay per-line in the editor), double-click for a word and triple-click for
a line, `PageUp`/`PageDown` in the editor only, and undo/redo.

**Undo is the one with a real design decision in it.** `input::History` holds whole-`String`
snapshots rather than diffs — the same bet §7 makes about not needing a rope, since these are URLs,
header values and hand-authored bodies. The selection is *part of* the snapshot, because undo that
restores text but leaves the caret where it happened to be makes the second undo land somewhere
unpredictable.

Coalescing is **structural, with no clock**: a run of typed characters collapses to one entry, and
the run closes on a deletion, a paste, a newline, or the caret moving. Rejected: the idle timer
most editors use, which feels marginally better and puts wall-clock time in the edit path — this
repo already lost six hours of CI to one timing race, and a deterministic rule that is 95% as good
is the better trade. One history per entity, so `Ctrl+Z` in the URL bar cannot reach into the body.

Two subtleties worth keeping. A single-character insert opens a run **even when it replaced a
selection**, so select-all-then-type undoes in one press instead of stranding the first character
as its own entry — requiring an empty range there was the first version and it was wrong. And the
IME path records only as a composition *opens*: it is called on every keystroke while a candidate
is being edited, so recording each call would bury the history under states nobody typed.

> **`break_run` on a caret move looked redundant and isn't.** The contiguity check already splits
> a run when the caret moves *somewhere else*, so the first test written for this passed with the
> call deleted. It earns its keep only when the caret moves away and comes **back** to the same
> offset — left then right, or a click landing where it already was — which is what
> `moving_the_caret_starts_a_new_undo_entry` exercises. Another instance of a unit test covering
> the type while leaving the call site unheld.

**Explicitly deferred to M3+:** syntax highlighting (needs tree-sitter plus a highlight
cache), autocomplete, multi-cursor, code folding in the *editor*, bracket matching. The
an "excellent request/code editor" is a milestone of its own — treating it as a
sub-task of M1 is the most likely way this project stalls.

**The rope was dropped, deliberately.** Two things decided it. `ropey`'s current release is
`2.0.0-beta.1`, so a stable version requirement won't even resolve to it. And the benefit is
unmeasurable at these sizes: request bodies are hand-authored, so a 100KB body means the line
index rescan is a ~10µs `memchr` sweep per keystroke against a 16ms frame budget. One text
model shared with `TextInput` is worth more than an O(log n) edit nobody can feel. Revisit when
bodies routinely exceed ~1MB, or when in-buffer undo history needs cheap snapshots.

---

## 8. Latency budget

The numbers that make "Zed-level feel" testable rather than aspirational:

| Path | Budget | Measured |
|---|---|---|
| Cold start → interactive window | **< 100 ms** | 189 ms (M1.0, release, warm) — see below |
| Keystroke → glyph painted | **< 16 ms** (one frame) | — (M1.1) |
| `Send` keypress → bytes on wire | **< 5 ms** | not yet isolated |
| Response arrives → status + headers painted | **< 50 ms** (at TTFB, not completion) | structural ✅ |
| 10 MB JSON → first paint | **< 300 ms**, parse fully off-thread | **48 ms** ✅ |
| Scrolling any response | **60 fps sustained** | structural ✅ |

### The startup budget needs recalibrating

M1.0's measured breakdown (release build, GNOME/Wayland, warm page cache):

```
[zuno] runtime ready         120.16ms     <- Application::new() + platform init
[zuno] theme + keymap        120.91ms     <- 0.75ms: font resolution, palette, keymap
[zuno] window open           189.24ms     <- 68ms: first frame laid out and presented
```

The useful finding is *where* the time goes. **120ms is spent before a single line of Zuno's
code runs** — that's GPUI constructing the `Application` and bringing up the Wayland/GPU
platform layer. Our own controllable share is ~69ms, and the work this milestone actually
added (font enumeration, building both palettes, registering nine bindings) costs 0.75ms.

So the `< 100 ms` target is not reachable on gpui 0.2.2 on this platform no matter how fast
Zuno gets, because the floor is already 120ms. Two honest options rather than quietly missing
the number every milestone:

- **Re-baseline the budget** to `< 100 ms of Zuno-controlled time` (currently ~69ms, passing),
  and track GPUI's platform init as a separate fixed cost we don't own.
- **Or investigate the 120ms** — some of it is likely GPU/driver enumeration that a later gpui
  release or a warm shader cache improves. Worth one timeboxed look, not a milestone.

Either way: measure per-stage, not end-to-end. An end-to-end number would have hidden the fact
that our own code is 0.75ms and told us to optimize the wrong thing.

Instrument these from the first commit — a `ZUNO_TIMING=1` env var that prints stage timings
to stderr, plus criterion benches on `zuno-core` for parse/flatten. A budget you don't measure
is a budget you've already blown.

---

## 9. Dependencies

> **Rule, learned the hard way.** Check the registry (`cargo info <crate>`) before writing a
> version requirement — do not write it from memory. A caret requirement pins the *major*
> line, and for `0.x` crates the minor **is** the major: `"0.12"` means `>=0.12.0, <0.13.0`
> and can never reach 0.13. `reqwest` was declared `"0.12"` here and silently stayed a whole
> major line behind while 0.13.4 was current. Every other crate was declared at its latest
> major (`"1"`, `"2"`), so resolution picked the newest release and they were all current —
> which is exactly why the one mistake was easy to miss.

Declared below; all verified current against crates.io.

```toml
# core/Cargo.toml — versions verified, not remembered
reqwest      = { version = "0.13", default-features = false,
                 features = ["rustls", "stream", "gzip", "brotli",
                             "deflate", "zstd", "cookies", "http2"] }
tokio        = { version = "1", features = ["rt-multi-thread", "sync", "time", "net"] }
async-channel = "2"          # runtime-agnostic: tokio writes, smol reads
futures-util = "0.3"
bytes        = "1"
http         = "1"
url          = "2"
serde        = { version = "1", features = ["derive"] }
thiserror    = "2"
# M1.3+: serde_json, ropey, criterion

# app/Cargo.toml
gpui         = "0.2.2"
zuno-core    = { path = "../core" }
```

**Use `reqwest` directly — not gpui's re-exported `http_client`.** That crate
(`gpui_http_client`) is built for Zed's own needs: it's a `HttpClient` trait abstraction with
GitHub-release-download helpers and proxy plumbing. An API *testing* client needs the opposite
of an abstraction — raw header order, per-request TLS and redirect control, connection timing
hooks, and streaming bodies. Go straight to `reqwest` and keep `gpui`'s copy out of your
call paths.

---

## 10. Build order

Five stages. Each one ends somewhere you can actually run the thing.

**M1.0 — Shell. ✅ Shipped.** Workspace split. `Theme` global with light/dark tokens.
`actions!` + keymap + focus contexts. Two-pane layout (request left, response right) rendering
`RequestSpec::sample()` / `ResponseData::sample()`. `ZUNO_TIMING=1` boot instrumentation.
7 core unit tests, zero warnings. *Done when:* window opens in <100ms, `Ctrl+L`/`Tab` move focus
visibly, theme toggles.

> Everything in the shell is read-only by design — the point of this stage is that layout,
> theming, and focus dispatch are correct *before* any text editing exists. The URL bar and body
> region are real focus targets with real key contexts; they simply don't accept keystrokes yet.

**M1.1 — Input. ✅ Shipped.** `TextInput` (~570 lines) adapted from gpui's `examples/input.rs`
with theme-driven colors, inherited text style, grapheme-aware movement, IME composition, and
clipboard. URL bar, method cycling, and fully editable headers/query tables — add, mute,
remove, by keyboard or mouse. `RequestSpec` derived on demand. `SendRequest` dumps the
assembled spec to stderr as the honest stand-in for the engine. 8 headless GPUI tests.

> **Three GPUI facts worth keeping.** (1) Key context predicates match only the *leaf*
> context — `Identifier(name) => contexts.last().contains(name)` — so nesting a `key_context`
> div around an input does **not** let a binding target it. Both identifiers have to go in one
> context string (`"TextInput UrlBar"`), which works because `KeyContext::parse` accepts
> whitespace-separated identifiers. (2) `TabStopNode` orders by tab_index path *then* paint
> order, so leaving every input at the default tab_index 0 makes visual order the tab order for
> free. (3) A focus handle needs an explicit `.tab_stop(true)` or `focus_next()` skips it
> entirely — the bug the `tab`-reaches-the-value-cell test now guards.

> *Done when:* you can type a real request and get the correct spec back — now enforced by
> `typed_text_reaches_the_derived_spec` rather than by eyeballing it.
>
> **Two things deliberately not built.** A method *dropdown* needs an anchored popover; cycling
> via `Ctrl+M` / click covers the same ground for now, and the popover is worth building once
> rather than twice. The body stays read-only until M1.4 — a multi-line editor is a different
> build from a single-line one, and pretending otherwise is how M1 stalls (§7).

**M1.2 — Engine. ✅ Shipped.** Dedicated tokio thread, `Engine::send` returning an event
stream, per-settings client cache, `build.rs` with typed errors, streaming body with throttled
progress, two-part cancellation. Response pane gained in-flight and failure states; the Send
button becomes Cancel while a request is live. 40 tests across three layers: pure build-time
units, end-to-end over real sockets, and full-stack through simulated keystrokes.

> **Verified:** a real request goes out and real bytes come back (`a_real_request_goes_out_and_
> real_bytes_come_back`, over a real socket), `Escape` cancels mid-flight
> (`escape_cancels_an_in_flight_request`), and `ZUNO_TIMING=1` prints per-request ttfb/total.
> A separate `#[ignore]`d test hits real HTTPS — 200 over HTTP/2, TTFB 74.7ms — because
> localhost never exercises DNS, rustls, or ALPN. It stays ignored so CI never depends on the
> internet.

> **The bug worth remembering.** `Url::parse("https://{{baseUrl}}/users")` **succeeds** — it
> reads the placeholder as a hostname. Zuno would have done a DNS lookup for a literal
> `{{baseurl}}` and reported "could not connect to {{baseurl}}". Unresolved `{{…}}` is now
> caught before parsing, in the URL and in header names and values (sending
> `Authorization: Bearer {{token}}` literally is worse than failing). Deliberately *not*
> checked in bodies, where `{{` occurs legitimately inside JSON.

> **Three design points.** (1) `EngineError` owns all its data so it's `Clone` and can travel
> the event channel into view state — and `is_local()` distinguishes "nothing left the machine"
> from a network failure, which is what the response pane uses to say *Request not sent* rather
> than *Request failed*. (2) Clients are cached per distinct TLS/redirect/encoding combination,
> because those are client-level in reqwest while timeout is per-request — one client per
> request would have thrown away the connection pooling that makes resend feel instant.
> (3) `Progress` is throttled to one event per 33ms; per-chunk emission floods the channel with
> events the UI cannot paint.

**M1.3 — Response viewer. ✅ Shipped.** `JsonOutline` + `uniform_list`, folding by click or
`Alt+F`/`Alt+E`, the >10MB cap with an explicit *parse as JSON anyway*, and a virtualized
raw-text fallback. 44 new tests, including a perf suite.

> **Measured (release, 10.5MB / 1.31M rows):** flatten **47.9 ms (209 MB/s)**, `visible_rows`
> **6.7 ms** unfolded and **7.3 µs** with the root folded, line index **5.7 ms**. All of it on
> a background executor, so the UI thread sees only a finished index.

> **Four things worth remembering.**
>
> 1. **The raw fallback needed virtualizing too.** A 10MB *text* body has just as many rows as a
>    10MB JSON one; rendering it as `Vec<String>` would have blocked exactly as hard. Hence
>    `LineIndex` — byte spans, same shape as `Row`.
> 2. **Minified JSON is one 10MB line.** Virtualization doesn't help when there's a single row,
>    because shaping that one text run stalls the frame regardless. Lines are truncated at 4KB
>    for *display*, on a UTF-8 boundary, and the row says so rather than silently ending early.
> 3. **Fold state is inferred from the visible index at render time, not captured.** The render
>    closure must be `'static`, so capturing the `Vec<bool>` of fold flags would clone ~1.3MB
>    every frame. Instead `is_folded_at` uses the fact that an unfolded open row is always
>    followed by row `ix + 1` — anything else means the subtree was skipped. The closure holds
>    two `Arc`s and nothing else.
> 4. **Content-Type is a hint, not an oracle.** Plenty of real APIs return JSON as `text/plain`
>    or with no type at all, so the first non-whitespace byte is sniffed too. But an explicit
>    `text/html` is respected — an HTML error page that happens to start with `{` must not be
>    parsed as JSON.

> **The cap is a memory limit, not a speed limit.** 10MB flattens in 48ms; the problem is that
> it produces 1.31M rows at ~32 bytes each, so the index costs more than the body. Past the cap
> the user gets a raw view and an explicit button, because silently spending hundreds of MB is
> worse than asking.

**M1.4 — The loop. ✅ Shipped.** Multi-line body editor (line-aware movement, per-line
Home/End, auto-indented newlines, cross-line selection, IME, viewport-only shaping), body-kind
cycling, `ResponseDiff` against the previous run with a summary bar, ten-deep response history,
and session restore of the scratch request. 27 new tests.

> **The bug worth remembering.** `compute_line_starts` originally dropped the final line start
> when content ended in `\n`, copied from `LineIndex`. In a *viewer* that's right — a phantom
> blank line at the end is noise. In an *editor* it's wrong twice over: the last line's text came
> back as `"a\n"`, which trips `shape_line`'s newline assertion, and pressing Enter at the end of
> the buffer left the cursor with no row to sit on. The two types now differ on purpose, and the
> reason is commented in both.

> **Three design points.** (1) The diff is a *summary* — status, timing, size, which headers
> moved, whether the body is byte-identical — because the loop's question is "did my change do
> anything?", and a full inline body diff would bury that signal. (2) `date`, `age`,
> `x-request-id` and friends are excluded from header comparison; otherwise "headers changed"
> would be permanently true and therefore worthless. (3) A failed send **keeps** the last good
> response as the diff baseline while showing the error, so the next successful send still has
> something to compare against.

> **Session persistence is a global, not a constant path.** The suite drives `SendRequest`, and a
> send is a save point — without an injectable path, running `cargo test` would overwrite the
> developer's own session file. (It did, once, before the path was made injectable.)

---

### M1.5 — Fixes and curl import ✅

Three nuisances and one feature, before planning M2.

**Save on every exit path.** `session::save` was reachable only from the Send and Quit
*actions*, so closing the window with the window manager's button lost every edit since the
last send. Now `Workspace` registers `cx.on_app_quit`, and `main` registers
`cx.on_window_closed` → `cx.quit()` — GPUI does not quit on last-window-close by default, so
without the second hook the process would linger with nothing on screen *and* never reach the
first. Note SIGTERM still bypasses both; that's the OS's call, not something to paper over.

**The cookie jar is a setting, not a hardcoded surprise.** It was `.cookie_store(true)` in the
client builder with nothing in the model and nothing on screen, which quietly made every
request non-independent. It's now `RequestSettings::cookie_store`, defaulting to `true` to match
Postman and browsers, and it fragments the client cache correctly.

> **The bug that fix caused, and the rule it produced.** Adding `cookie_store` broke
> deserialization of every session written by an earlier build — `missing field cookie_store` —
> so a real saved session silently fell back to the sample. `RequestSettings` now carries a
> container-level `#[serde(default)]`, so any future setting is tolerated. `RequestSpec`
> deliberately stays strict, because a corrupt file must be rejected rather than quietly become
> an empty request; new fields *there* need a per-field `#[serde(default)]`. There's a
> regression test pinned to the exact pre-`cookie_store` JSON shape.

**curl import** (`Ctrl+Shift+V`, from the clipboard). Handles the full realistic flag set —
`-X`, `-H`, `-d`/`--data-raw`/`--data-binary`/`--json`, `-F`, `-u` (→ Basic auth, base64 written
inline rather than adding a dependency), `-G`, `-b`, `-A`, `-e`, `-k`, `-L`, `--max-time`,
`--compressed` — plus shell tokenization with single/double/`$'…'` quoting and line
continuations. 35 tests.

> **Two decisions.** (1) **Unknown flags are reported, not fatal.** curl has hundreds of
> options, most about output; refusing an import over one unrecognised flag would break the
> feature exactly where it's most useful. Anything skipped comes back in `ignored` and is named
> in the status bar, so an import never silently loses part of a command. (2) **The query string
> stays in the URL** rather than being split into editable rows — splitting means decode then
> re-encode, which can invalidate a signed URL, and presigned URLs are precisely what people
> paste.

> **A faithfulness trap worth knowing.** `curl -d 'a=1'` with no `Content-Type` sends
> `application/x-www-form-urlencoded`. Import adds that header explicitly, because otherwise
> Zuno would infer `text/plain` from the raw-body kind and the imported request would behave
> differently from the command it came from.

> **The same trap in the other direction, found later by audit.** Import started from
> `RequestSpec::default()`, whose `follow_redirects` and `accept_encodings` are both **on**, while
> curl's are both off. So `-L` and `--compressed` were no-ops — and, worse, their *absence* was
> unrepresentable: `curl https://x/redirects-to-login` imported as a request that follows the
> redirect and reports the login page's 200 instead of the 302 you were investigating. The imported
> spec now starts from curl's defaults for those two.
>
> The line is drawn at **wire-observable** behaviour. `timeout` keeps Zuno's 30s even though curl
> waits forever, because that is a local guard rather than something a server can tell apart, and
> "no timeout by default" is a worse default than a slightly wrong one; `max_redirects` keeps Zuno's
> 10 rather than curl's 50, since it only applies once `-L` is present. `-k` was always faithful
> because there the polarity lined up: both verify by default, so the flag only ever turned
> something off.

> **And one where faithful was wrong.** curl treats any bare word as a hostname, so
> `curl this is garbage` parses with `url = "this"`. Faithful, and useless as an import —
> pasting arbitrary text would quietly build a nonsense request. Import now requires either the
> `curl` word or something that actually looks like a URL.

### curl export — the other direction, added much later

`Ctrl+Shift+X` copies the active request as a runnable curl command. `curl.rs` now holds both
directions deliberately: a flag the exporter emits and the importer drops is a bug visible in one
file, and `a_command_round_trips` asserts it rather than trusting it.

**Variables are resolved except the secret ones**, via `Resolver::without_secrets`. This is the
decision worth recording, because both obvious answers are wrong. Resolving everything puts a live
credential in the clipboard and therefore in the issue or chat message the command is being pasted
into — precisely the leak the committed/gitignored split exists to prevent. Resolving nothing makes
the command un-runnable, which defeats "here's the repro". So `dev.json` values are substituted and
`dev.local.json` values come out as `{{token}}`, and the status bar names what it withheld so the
placeholder reads as deliberate rather than broken.

The split does that work for free, which is the argument for it having been a *file* distinction
rather than a per-variable flag all along: nothing had to be marked for export to get this right.

Four implementation points, each with a rejected alternative:

- **`without_secrets` removes the values rather than adding a redaction pass.** `resolve` already
  leaves an unknown placeholder verbatim, so "withheld" is just "undefined" — one substitution path
  instead of two sets of rules to keep in step. A redacting mode could forget a field the way
  `apply` once forgot form bodies.
- **The URL goes through `build::resolve_url`**, so the exported URL is the one the engine would
  request, percent-encoding and all. It *fails* when a secret sits in the URL — normal here, not an
  error — and falls back to appending rows unencoded, which is honest for a command the recipient
  must finish editing anyway.
- **A form body is one `--data-raw` carrying `build::encode_form`'s output**, byte-identical to what
  Zuno sends. Rejected: one `--data-urlencode` per field, which lets *curl* do the encoding and
  differs whenever a field **name** needs escaping, since curl only encodes after the `=`. Sharing
  `encode_form` is what stops the two drifting, and a test compares the exported body against
  `build_body`'s bytes.
- **Flags follow the same wire-observable line the import draws**: `-L`, `--compressed`, `-k`, and
  `--max-time` only when it isn't the default. `--max-redirs` is deliberately absent — the import
  already judged it not worth faithfulness, and emitting a flag the importer doesn't read would make
  every exported command report an ignored flag on the way back in. The cookie jar has no
  representation at all: `cookie_store` is an in-process jar shared per client config, curl's `-b`
  and `-c` are files, and inventing a flag would export a request that behaves differently.

> **The bug a test caught, and it was in the reporting rather than the export.** `withheld_in`
> scanned every row instead of only enabled ones — and the sample request ships a *disabled*
> `Authorization: Bearer {{token}}`, so a fresh buffer announced that a secret had been withheld
> from a command that never referenced one. A status line has to describe what was exported, not
> what is merely typed on screen. Same class as the disabled-row rule everywhere else.

> **Shell quoting is checked by re-tokenizing, not by inspection.** `quote` wraps in single quotes
> and rewrites an embedded `'` as `'\''` — close, escape, reopen. The first test asserted the
> rendered command had an even number of quotes, which is simply false: POSIX has no escape inside
> single quotes, so correct output is routinely odd. The test now runs the command back through this
> module's own `tokenize` and requires the payload to come out as one token, which both proves the
> shell would reproduce it and pins the exporter's quoting to the importer's parsing.

**Window chrome.** `WindowOptions::window_decorations` defaults to `None`, so GPUI was never
told which mode to use and the window came up **client-decorated with nothing drawing the
decorations** — no close/minimize/maximize, and no way to resize. Confirmed by logging
`window.window_decorations()`, which reported `Client { tiling: … }`.

`chrome.rs` now draws them: an app-named titlebar that drags to move and double-clicks to
maximize, platform-aware control buttons (`WindowControls` says which the compositor supports),
and eight invisible 6px resize strips — corners emitted last, because later children win
hit-testing and a corner has to beat the two edges it overlaps. Client-side is also the right
mode to commit to on Wayland: GNOME prefers CSD and won't reliably draw a server titlebar.

**A rendering bug the screenshot caught, and the feature it was hiding.** A long URL painted
straight over the Send button, and long header values pushed the row's `×` out of view.
`truncate()` sets text-overflow *styling*, which does nothing to a custom-painted element like
`TextInput`'s shaped line — that needs a real `overflow_hidden()` clip.

Clipping alone only converted the bug into a worse one: the hidden text became unreachable. Both
`TextInput` and `Editor` now carry a **horizontal scroll offset that follows the cursor** —
recomputed each prepaint, clamped so the text never scrolls past its end or leaves a gap when it
fits. *Cursor*-following was the whole story for several milestones, and it meant a trackpad did
nothing: `Editor` now also takes a wheel delta, which §6's horizontal-scrolling section covers. Hit-testing and the IME rectangle both undo the offset, or clicking in a scrolled input
would land on the wrong character. The `overflow_hidden` clip is what makes it safe to paint
outside the box, so the two fixes are one mechanism.

In the editor the clamp uses the *cursor's* line width rather than the widest visible line —
the latter would make the scroll limit jitter as you scroll vertically, and would mean measuring
every line to know how far right the content goes.

**Still deliberately absent:** tabs, collections, the `Ctrl+P` / `Ctrl+K` palettes, environments
and variables, syntax highlighting, a method dropdown, a settings panel, and a history browser.
The navigation thesis is entirely M2.

**Deferred by design, and it's worth naming them so they stop feeling like omissions:**
tabs/buffers, collections, the `Ctrl+P` / `Ctrl+K` palettes, environments and variables, auth
schemes, scripting, syntax highlighting, cookie jar UI, certificates. All of them are M2+.

---

## 11. Built, but not reachable from the UI

Worth knowing before building anything in M2: **there is more product in here than the window
shows.** Each of these is honoured on every request and has no way to see or change it. Most are
UI work, not engine work.

| Capability | State |
|---|---|
| ~~**Cookie jar**~~ | **Reachable.** Toggle plus a `cookies on` badge in the status bar, and `Engine::clear_cookies` — see below for why the toggle alone wasn't enough |
| ~~Timeout (30s)~~ | **Reachable** in the settings panel, 1–600s |
| ~~Redirect following + max hops~~ | **Reachable** in the settings panel |
| ~~TLS verification toggle~~ | **Reachable** in the settings panel. curl import still sets it from `-k` |
| ~~gzip / brotli / deflate / zstd~~ | **Reachable** in the settings panel |
| ~~Form bodies~~ | **Reachable.** `Ctrl+Shift+B` picks the type, `Ctrl+Shift+F` adds a field, and the fields use the same table widget as headers and query rows |
| ~~Binary bodies~~ | **Reachable.** `Ctrl+Shift+O` picks a file through the native dialog; only the path is held, and `build.rs` reads it at send |
| ~~Multipart bodies~~ | **Reachable.** `Ctrl+Shift+M` adds a part, `Ctrl+Shift+O` attaches a file to the focused one. reqwest's `multipart` feature is enabled, and `build_body` reduces parts to bytes so `PreparedBody` keeps its derives |
| ~~Response history~~ | **Reachable.** `Ctrl+H` lists every retained run; choosing one shows it and re-indexes its body. Until then the retention was *write-only* — nothing read it, not even the diff |
| ~~Custom HTTP methods~~ | **Reachable.** The method picker offers the typed text as a verb when it isn't one of the seven, so `Method::Other` finally has a UI path |

**Nothing remains.** `Ctrl+,` closed five, the method picker a sixth, `Ctrl+H` a seventh, and body
authoring took form, binary, and multipart — the last of which was the only item here that ever
needed engine work rather than UI.

The section stays as the record of *how* the gap opened: the engine was built ahead of the views,
which is a reasonable order and a predictable debt. Two things it left behind are worth keeping in
mind:

- **`preserved_body` is gone.** Multipart was the last body type the UI couldn't author, so
  `RequestView::load` now matches every `Body` variant exhaustively with no catch-all — adding a
  variant is a compile error until someone decides how to edit it. The compiler forced the change:
  once multipart was authorable the catch-all became unreachable and `-D warnings` rejected it.
- **An explicit `Content-Type` cannot win for multipart**, unlike every other body. `multipart`
  generates the boundary and writes the header itself, and a user-supplied `multipart/form-data`
  without that boundary is unparseable. `conflicting_content_type` therefore reports nothing for it.

A caution for whoever reads this section as a to-do list: it tracks *unreachable engine
capabilities*, so by construction it cannot name a gap where the engine was never involved. The
biggest hole found after this list was down to two — that nothing in the app can copy a response —
appears nowhere in it. See ROADMAP's audit.

Worth recording about the history one, because it wasn't only a missing feature: `history` was
written, truncated, and read by **nothing** — not even the diff, which is computed once when a
response lands. Ten `ResponseData` per buffer were retained where nothing could reach them, and
`Bytes` being refcounted means retaining pins the underlying buffers. So surfacing it was also what
made the memory it was already costing worth paying.

Two things the settings panel turned up that are worth knowing before touching either half:

- **The cookie jar was never per-request.** `ClientKey` includes `cookie_store`, so every request
  with the same client-level settings shares one `Client` and therefore one jar. Toggling cookies
  off doesn't empty anything — it routes through a *different* cached client, and toggling back
  returns the original jar intact. So a toggle on its own would have created the confusion it was
  added to remove, and `Engine::clear_cookies` (drop the cached clients; the next request builds a
  fresh jar) shipped with it. reqwest owns the store behind `cookie_store(true)` and exposes no way
  to empty it, which is why eviction rather than clearing.
- **Settings are per-request, and stay that way for now.** `RequestSettings` lives on `RequestSpec`,
  so it already persists per collection file. A global-defaults layer needs a scope model
  (global → environment → request) — the same one environments has to build in M3 — and doing it
  twice would mean throwing one away.

---

## 12. Open decisions

Deliberately **not** pre-answered. Each got cheaper to decide once the loop worked, and a
confident guess written down now would mislead more than it helps.

**Persistence format — decided.** "Local-first" fits both SQLite and a git-diffable file tree
(Bruno-style, one file per request). Both halves are now settled, and they went different ways on
purpose:

- **Collections: a directory of one-request-per-file JSON** (`core/src/collection.rs`). The reason
  is git. A collection you can commit, review in a pull request, and merge is a genuine
  differentiator, and that's only true if one request is one file with stable serialization —
  pretty-printed, newline-terminated, and byte-identical when nothing changed, so an unrelated save
  doesn't dirty the working tree. A single-file bundle or a SQLite database each turn "added a
  header" into an unreadable diff. Rejected for that reason, not for weight.
- **Window session: a versioned JSON envelope** in `app/src/session.rs` — which buffers are open,
  which was in front, and which file each came from. Deliberately in `app/`, not `core/`: that is
  window state, not part of the request model a future CLI shares. JSON rather than SQLite because
  it is one file and one write; nothing forecloses moving history and the response cache into
  SQLite later, which is where that dependency would start to pay for itself.

Two consequences worth knowing before touching either:

- **`RequestId` is written as 0 in collection files** and reassigned by `Workspace::next_id` on
  open. It's a session-local handle, so persisting the live value would put churn in every diff and
  manufacture merge conflicts over a number nothing reads across runs. Normalizing keeps the format
  a plain `RequestSpec` — no parallel `StoredRequest` type to drift out of sync.
- **Filenames are derived from the request, not from an id** — `posts.json`, not `7f3a.json`, since
  the point is a readable directory. Derived names therefore collide, and a derived name is *not*
  an identity: `collection::allocate` never overwrites, and `RequestView::path` remembers where a
  buffer lives so a second Ctrl+S overwrites its own file instead of breeding `posts-2.json`. That
  path is what the session envelope's v2 bump exists to persist.
- **`slug` is a security boundary.** The label feeding it comes from a URL, so
  `https://x.test/../../.ssh/config` would otherwise write outside the collection. Containment is
  held up twice over — `label_for` yields only a single path segment or a host, and `slug` then
  strips separators — and both layers are tested independently, because a test that passes when
  either one works cannot tell you which is load-bearing.

`serde_json` moved from a dev-dependency to a real dependency of `zuno-core` for this: the format
lives in core precisely so a future CLI can read and write collections, which means core has to
serialize rather than only model.

Still open: **nothing about the format.** What's missing is *reach* — there is no way to open a
saved request back into a buffer, which is the picker's job (principle 2), and no folder authoring
beyond nesting a directory by hand. See §12's remaining entry.

**Tabs — decided and built.** `Workspace` owns `Vec<Entity<RequestView>>` with an `active_ix`,
restores every saved buffer, and persists all of them on quit and on send.

Three decisions worth recording, since each had a plausible alternative:

- **Focus travels with the switch, via a single `activate`.** A `FocusHandle` belongs to the entity
  that made it, so setting `active_ix` alone leaves focus in the old view — and after a close, in a
  dropped one, where no key context matches and every binding silently stops working. Funnelling
  all four verbs plus both mouse paths through `activate` is what makes that unforgettable rather
  than a rule to remember. Rejected: letting each handler move focus itself, which is how the bug
  gets reintroduced.
- **Closing the last buffer opens a fresh one; it does not quit.** An empty `views` makes `active()`
  return `None`, which every handler reads as "do nothing" — a window that is still there and
  silently inert. Rejected: quitting on the last close, which conflates Ctrl+W with Ctrl+Q and can
  lose work.
- **Tab labels derive from the URL** (`label_for` in `core/src/request.rs`), not from
  `RequestSpec::name`. Nothing can edit `name` — it's only ever set from the URL at import — so a
  request since pointed elsewhere would keep advertising its old target, which is exactly what a
  real session file showed. The derivation is shared with curl import's `derive_name` so the two
  can't drift, and takes `&str`s rather than a `&RequestSpec` because the strip asks every buffer
  every frame and `spec()` clones every header. A rename action should later prefer a user-set
  `name`, which needs a way to distinguish "typed" from "guessed".

Curl import now opens a **new** buffer. Replacing was only defensible while there was nowhere else
to put the result; an import over unsaved work destroyed it with no undo. `RequestView::load`
remains for genuine in-place replacement.

Still deliberately **unanswered: what a *dirty* buffer means.** With no collections there is no
saved baseline to be dirty against, so any meaning invented now would be rewritten when the
collection format lands. Also absent by choice: tab reordering and renaming.

**Reaching a saved request — answered.** `Ctrl+P` opens the picker over open buffers *and*
`collection::scan`, and choosing a file opens it as a buffer with its `path` set, so the next
Ctrl+S overwrites rather than duplicates. The one-way door — Ctrl+S writing files nothing could
read — is closed.

Three decisions in `picker.rs` worth recording:

- **Concrete, not a `PickerDelegate` trait.** Principle 2 says build the picker once; the picker
  owns `Vec<Item>` where each carries a `Target` it never interprets, so a new consumer is a new
  variant rather than a rewrite.

  This entry used to justify that with "one consumer today", and **the count has since reached
  seven** — buffers, files, actions, methods, environments, runs, body types. The decision is
  unchanged, but for the reason originally written *after* the count: the trait earns its
  complexity at a consumer that wants **different rendering**, and none of the seven does. They
  differ only in the data they carry, which is what `Target` exists to absorb, and all seven draw
  as label plus dimmed detail. Reconsider on a row shape that doesn't fit — a preview pane, an
  icon column — not on the eighth variant.
- **Modal, not `anchored()`.** A palette is centred over the window, so it's a full-size `absolute`
  overlay. `anchored()` positions relative to a point; both exist in 0.2.2 and this needed the
  simpler one.

  **This sentence guessed twice at what would want anchoring and was wrong both times**, which is
  worth keeping rather than tidying away. It first named the method dropdown; M4 found a centred
  picker was better there, one idiom and keyboard-first. The row context menu is the real answer —
  a menu that doesn't appear where you clicked isn't a context menu — and it is a *separate
  primitive* rather than a picker mode, because the picker's centred overlay is the one thing it
  must not be. See §6 and `app/src/context_menu.rs`. The pattern: a guess about the future consumer
  of an unused API is worth less than the reason the current one didn't need it.
- **Scan on open, off-thread, results streamed in.** The picker opens instantly with the buffer rows
  and gains saved requests when `scan` returns (invariant 3). Caching at startup was rejected: a
  collection is a git directory, so it changes under us on every pull. `Picker::extend` re-ranks
  against whatever has been typed meanwhile, because on a slow disk you can finish typing first.

A fourth, added after an audit found it missing: **a modal owns the keyboard exclusively, and that
is enforced by a guard rather than by key contexts.** `Workspace::modal_open` is consulted by every
opener *and* by `FocusNext`/`FocusPrev`. Two things forced it. The openers had drifted — four checked
both modals, `Ctrl+P` and `Ctrl+K` checked only the picker, so a picker could stack over the settings
panel and closing it restored focus to the buffer behind, leaving the panel stranded. And `Tab` did
the same thing directly: the panes behind a modal are still painted, so their inputs are still tab
stops and `focus_next` walks past the scrim into them.

*Rejected: scoping the `tab` binding with a context predicate.* GPUI matches only the **leaf**
context, so "not inside a modal" cannot be written once — it has to be restated for every modal
context that ever exists, and the failure mode when someone forgets is a dead keymap with nothing on
screen explaining it. A guard on the handler is one place and cannot be forgotten by a *new* modal,
only by a new focus-moving action.

Deliberately absent: **no highlighting of matched characters.** It needs match positions threaded
out of the scorer and styled text runs, and the picker is useful without it.

**Two defects in the row itself, both found from a screenshot rather than from a test.** Recorded
together because they had the same cause — a value that looked right at the call site and was wrong
about the thing it was actually feeding.

- **A row is a hitbox, and it wasn't one.** `uniform_list` lays each item out as a taffy *root* and
  hands it the list's width as definite available space, which reads like a stretch instruction. It
  isn't: taffy auto-stretches a root to its available width only for `display: block`, and every
  row here calls `.flex()`. The rows were **76px wide inside a 620px list** — the selection
  highlight ended at the label, and the other 88% of each row silently swallowed clicks. Fixed with
  `w_full()`, and the test asserts a *click* at the far right of the list, deliberately measured
  against the container: the row's own bounds are the narrow box, so anything derived from them
  passes against the bug. Same weak-assertion shape `CLAUDE.md` tracks, avoided by choosing the
  reference frame the bug can't move.
- **`theme.border` was serving as a text colour**, here and in `settings_panel`. In the dark theme
  `border` and `bg_hover` are the *same value*, so the detail column — which for `Ctrl+K` is the
  keybinding — was invisible on the selected row. That inverts the palette's stated purpose:
  §2 argues the mouse path exists to *teach* the keyboard one, and the row is where that teaching
  happens. `Theme::text_faint` is the token for tertiary text now, and `theme.rs` carries a
  contrast matrix over every text token × every surface — `bg_hover` included, because a colour
  that reads at rest can still disappear under the cursor, which is exactly what happened.

  Rejected: reusing `text_muted`, which flattens the row's label-over-detail structure into two
  equal fields. The matrix is a *token* test and cannot see a bad *use site* — nothing headless
  observes a paint (§2's third silent failure mode) — so `border_is_too_dim_to_read_as_text` asserts the
  low ratio on purpose, pinning why `border` must stay a divider colour rather than being
  brightened the next time something dim is wanted.

**Where `Ctrl+P` and `Ctrl+K` get their content — answered.** `Ctrl+P` lists open buffers then
`collection::scan`; `Ctrl+K` lists `commands::palette()`. Both go through the one picker.

The `Ctrl+K` half was mis-estimated for a while, and the correction is the useful part: a palette is
*not* a loop over `cx.all_action_names()`. That returns namespaced strings for every registered
action, including all the text-editing ones, with no human labels. `commands.rs` is a curated table
of real action **values** — so a rename is a compile error, not a dead row — and a drift test
requires every `zuno::` action to be either offered or excluded with a stated reason.

Two ordering facts worth keeping, both verified against the vendored source rather than assumed:

- `Window::dispatch_action` captures the focused id and then `cx.defer`s the dispatch. So a command
  chosen in the modal resolves against the frame the modal was in, and close-then-dispatch is
  indistinguishable from dispatch-then-close *for actions*.
- It is **not** indistinguishable for `Buffer`/`File` targets, because `activate` focuses
  synchronously — close afterwards and the focus restore clobbers the switch, leaving `active_ix`
  and focus disagreeing so you type into the request you just left. That is why the picker closes
  before acting, and `choosing_a_buffer_leaves_focus_in_that_buffer` is the guard.

**Global settings defaults** (new, deferred deliberately). Nothing can change what a *new* request
starts with: `new_tab` builds `RequestSpec::default()`, so `RequestSettings::default()` is hardcoded
in Rust — 30s timeout, TLS verification on, cookies on, 10 hops. Work against a dev box with a
self-signed certificate and you turn TLS verification off on every new request, forever.

An earlier note here claimed this needed the same scope model as environments. That was too strong,
and the correction matters because it changes the cost. **Two separable problems:**

| | What it takes |
|---|---|
| *Defaults for new requests* — what a fresh buffer starts from | One `RequestSettings` in a config file (`~/.config/zuno/settings.json`), copied into new buffers. Reuses the `install`/`install_at` global pattern from `session.rs`, so test isolation comes free under invariant 6. No model change, no serde risk, independent of environments. |
| *Inheritance* — "this request inherits TLS-off from the environment unless overridden" | Every field becomes `Option<T>` (`None` = inherit) or needs a parallel override mask, plus resolution at send. Real model change, hits invariant 7, touches every read site. |

Only the second shares anything with environments. The first is cheap and could land any time; it's
deferred because it wasn't asked for yet, not because it's blocked. Two sub-questions when it does:
whether saving defaults is an explicit action (recommended — a panel that silently changes every
*future* request is a nasty surprise) and whether the shipped values stay as they are (recommended —
they match Postman and browsers, and the config file is the place to disagree).

**The cookie jar's visibility** (new). It's on and invisible. Options: a status-bar
indicator, a per-request toggle, or a jar viewer. Leaning toward an indicator plus a toggle in the
settings panel that several other items in §11 also need — that section lists them.

---

## 13. What Milestone 1 delivered, and what it didn't

**Done:** the full loop — author a request (URL, method, ordered toggleable headers and query
params, multi-line body editor), send it over real HTTP with streaming progress and cancellation,
read the response through a virtualized JSON viewer that handles 10MB at 60fps, diff it against
the previous run, and come back to it after a restart. Plus curl import, light/dark themes,
window chrome, and 176 tests across three layers.

**Not done, and it's the important half:** the *navigation* thesis. The original brief named `Ctrl+P`,
`Ctrl+K`, fuzzy search across collections, and request-tabs-as-editor-buffers as the defining
features — the things that would make this Zed-like rather than Postman-like. None of them exist.
There is one request, no tabs, no collections, no palette.

That's the honest framing to carry into M2: **the loop is excellent and the differentiator is
unbuilt.** Also absent: syntax highlighting, a method dropdown (cycling only), a settings panel,
and form or multipart body authoring.

**A "known defect" that wasn't one — retracted.** This section listed the editor's per-line
horizontal scroll clamp as a bug: "the offset jumps when the cursor moves between lines of
different lengths … it's just wrong." §7 and two comments in `editor.rs` described the same
behaviour as a deliberate choice with a named rejected alternative. Reading the code settles it in
§7's favour, so the entry is gone.

The clamp is `max_offset = cursor_line.width - viewport + caret`. Land on a short line and the view
returns to x=0 — which is the only correct thing a cursor-following viewport can do: the cursor sits
at x≈20 on a 50px line in a 500px viewport, so staying scrolled right would push it off-screen to
the left. The rejected per-document clamp would leave `h_offset = 20`, scrolling away the start of a
line that fits entirely. And scrolling with the wheel doesn't move it at all: when the cursor's line
falls outside the viewport the lookup returns `None` and the offset is left alone, which is exactly
what the "widest *visible* line" alternative would have broken.

Worth recording as its own failure mode, because it is the mirror of the one this project already
tracks. The lessons in `CLAUDE.md` are about **code drifting away from a correct comment**. This was
a **doc asserting a bug the code never had** — and it is the more expensive direction, because a
confident "known defect, it's just wrong" sends the next reader hunting for something that isn't
there, and reads as licence to "fix" working code. When two sections disagree, the one that names a
rejected alternative is usually the one that was written while looking at the problem.

The counts in this section describe M1 as shipped and are deliberately not updated as work
continues; `CLAUDE.md` carries the live test count.
