# Zuno — Milestone One Architecture

> **Goal:** the most ridiculously good request → response loop possible.
> Open app → create request → send → inspect response → modify → resend.

Everything in this document exists to serve that one loop. Collections, environments,
scripting, auth, and certificates are explicitly **out of scope** for M1 — but the data
model is shaped so they don't require a rewrite.

Pinned stack: `gpui = "0.2.2"` (crates.io release), Rust edition 2024.

---

## 1. Guiding constraints

Four rules that decide most of the design. When a later decision is ambiguous, these break the tie.

1. **The core never imports GPUI.** Request modeling, HTTP, JSON flattening, and text
   buffers must compile and unit-test without a window. This is enforced mechanically
   (see §2), not by discipline.
2. **Nothing parses or formats on the UI thread.** A 50MB response body is parsed,
   flattened, and measured on a background executor. Only a finished, indexable
   structure crosses back to the renderer.
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
│       └── curl.rs         ✅ curl command line -> RequestSpec
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
        ├── timing.rs       ✅ the ZUNO_TIMING switch, shared by boot and requests
        ├── theme.rs        ✅ Theme global; light + dark tokens; font resolution
        ├── workspace.rs    ✅ root Render; owns buffers + all action handlers
        ├── request_view.rs ✅ one buffer: inputs + response + derived spec()
        ├── request_pane.rs ✅ method, URL bar, headers/query tables, body
        ├── response_pane.rs✅ status line, timing, headers, body viewer
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
    pub size: SizeInfo,            // wire vs decoded — both are interesting
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

**Two limitations found while implementing M1.2**, both inherited from reqwest:

- **Response header order is not wire order.** `http::HeaderMap`'s iteration order across
  different names is an implementation detail. Duplicates of the *same* name do stay in
  received order, so `collect_headers` stable-sorts by name: deterministic and readable,
  without scrambling duplicates. True wire order needs a lower-level client than reqwest.
- **`Timing.dns` / `connect` / `tls` stay `None`.** reqwest exposes no per-stage connection
  timings; getting them needs a custom hyper connector. `ttfb` and `total` are real. This is
  exactly why those three were typed as `Option` from the start rather than `Duration`.

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
Wire `Ctrl+C` / a re-`Send` to do both — an in-flight request must be abandoned the instant
you hit send again, or rapid resend feels laggy for reasons the user can't see.

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

**The viewer is read-only.** This is what makes M1 tractable: rendered rows plus
selection-for-copy, no editing, no cursor, no IME. All the editor complexity is confined to
the request side.

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

**Five deliberate changes from the upstream example**, made while adapting it in M1.1:
theme-driven colors instead of hardcoded literals; text style *inherited* from the parent div
(which is what lets one `TextInput` serve both the URL bar and the tiny table cells);
a caller-supplied key context identifier (see §10's note on leaf-only predicate matching);
newline sanitization moved into `replace_text_in_range` so it covers the IME and drop paths and
not just paste; and `character_index_for_point` returning `None` instead of asserting — the
example's `assert_eq!(last_layout.text, self.content)` panics whenever the placeholder is
showing, because an empty input lays out placeholder text rather than content.

**Explicitly deferred to M3+:** syntax highlighting (needs tree-sitter plus a highlight
cache), autocomplete, multi-cursor, code folding in the *editor*, bracket matching. The
"excellent request/code editor" in `idea.md` is a milestone of its own — treating it as a
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

> **And one where faithful was wrong.** curl treats any bare word as a hostname, so
> `curl this is garbage` parses with `url = "this"`. Faithful, and useless as an import —
> pasting arbitrary text would quietly build a nonsense request. Import now requires either the
> `curl` word or something that actually looks like a URL.

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
fits. Hit-testing and the IME rectangle both undo the offset, or clicking in a scrolled input
would land on the wrong character. The `overflow_hidden` clip is what makes it safe to paint
outside the box, so the two fixes are one mechanism.

In the editor the clamp uses the *cursor's* line width rather than the widest visible line —
the latter would make the scroll limit jitter as you scroll vertically, and would mean measuring
every line to know how far right the content goes.

**Still deliberately absent:** tabs, collections, the `Ctrl+P` / `Ctrl+K` palettes, environments
and variables, syntax highlighting, a method dropdown, a settings panel, and a history browser.
The navigation thesis from `what.md` is entirely M2.

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
| **Cookie jar** | **On by default.** `RequestSettings::cookie_store`, matching Postman and browsers. Nothing on screen says so, and it makes consecutive requests non-independent — the second carries the first's cookies. Wants an indicator or a toggle. |
| Timeout (30s) | Applied per request; not editable |
| Redirect following + max hops | Honoured; not editable |
| TLS verification toggle | Honoured; not editable. curl import sets it from `-k` |
| gzip / brotli / deflate / zstd | Negotiated; not editable |
| Form and binary bodies | The engine sends both correctly; the UI can only author raw bodies |
| Multipart bodies | Modeled, and curl import parses `-F` — but the engine returns `UnsupportedBody`. The one item here that is *not* just UI work |
| Response history | 10 deep, newest first. Only the diff surfaces it; there's no way to browse back |
| Custom HTTP methods | `Method::Other` sends anything; the UI only cycles the seven common verbs |

A settings panel and a history browser would each expose several of these at once, which makes
them unusually cheap for the value.

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

**Reaching a saved request** (new). Collections are written but not readable from the UI: Ctrl+S
saves, and nothing opens. That is deliberate — the opener is the picker (principle 2), not a
one-off list — but it means a saved request is only reachable while its tab is open. `collection`
has no `scan` yet for the same reason invariant 1 gives: it would have no caller until the picker
exists. Folder authoring is also absent; nesting works if you `mkdir` by hand.

**Where `Ctrl+P` and `Ctrl+K` get their content.** These are the thesis features from `what.md`
and neither exists. Note the dependency: `Ctrl+P` "find any request" is meaningless until
collections exist, which is why curl import came first — it's how requests get *into* the app at
all. A palette over one scratch request would be theatre.

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

**Not done, and it's the important half:** the *navigation* thesis. `what.md` named `Ctrl+P`,
`Ctrl+K`, fuzzy search across collections, and request-tabs-as-editor-buffers as the defining
features — the things that would make this Zed-like rather than Postman-like. None of them exist.
There is one request, no tabs, no collections, no palette.

That's the honest framing to carry into M2: **the loop is excellent and the differentiator is
unbuilt.** Also absent: syntax highlighting, a method dropdown (cycling only), a settings panel,
and form or multipart body authoring.

**Known defect, not a missing feature.** The editor clamps horizontal scroll per-line rather than
per-document, so the offset jumps when the cursor moves between lines of different lengths. It sat
in the list above for a while, which is a good way for a bug to never get fixed — it isn't waiting
on a milestone, it's just wrong.

The counts in this section describe M1 as shipped and are deliberately not updated as work
continues; `CLAUDE.md` carries the live test count.
