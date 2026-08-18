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
│       ├── engine/            (M1.2)
│       │   ├── mod.rs         Engine handle, Job, Event
│       │   ├── build.rs       RequestSpec -> reqwest::Request
│       │   └── run.rs         tokio runtime, execution, cancellation
│       ├── json/              (M1.3)
│       │   ├── mod.rs         JsonOutline (the stable interface)
│       │   └── flatten.rs     bytes -> Vec<Row> (swappable parser)
│       └── text/              (M1.4)
│           └── buffer.rs      rope-backed text buffer + edit ops
└── app/                    ✅ zuno — the GPUI binary
    ├── Cargo.toml
    └── src/
        ├── main.rs         ✅ bootstrap: window, keymap, theme, boot timing
        ├── actions.rs      ✅ every keyboard-reachable verb, in one place
        ├── theme.rs        ✅ Theme global; light + dark tokens; font resolution
        ├── workspace.rs    ✅ root Render; owns buffers + all action handlers
        ├── request_view.rs ✅ one buffer: inputs + response + derived spec()
        ├── request_pane.rs ✅ method, URL bar, headers/query tables, body
        ├── response_pane.rs✅ status line, timing, headers, body viewer
        ├── tests.rs        ✅ headless end-to-end tests (GPUI test platform)
        └── input/
            ├── text_input.rs ✅ single-line input primitive
            └── editor.rs        multi-line body editor (M1.4)
```

Three refinements the implementation forced, all worth recording:

- **`request_view.rs` is the buffer level** that §11's hedge asked for. `Workspace` owns
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
                                               // String becomes a rope-backed TextBuffer in
                                               // M1.4; nothing edits it before then
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

**On the parser:** start with `serde_json::Value` for correctness, then replace `flatten.rs`
with a span-emitting tokenizer when it hurts. `JsonOutline`'s public interface is the
contract; the UI never learns which parser is behind it. Be aware `Value` allocates the whole
tree, so it *will* hurt somewhere in the 10–50MB range — that's the signal to swap, not a
reason to hand-roll a tokenizer on day one.

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
| `Editor` (multi-line) | Request body only (M1.4) | Same input handler over a rope; soft-wrap off |
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

Use `ropey` for the buffer rather than `String`. Not for M1's tiny bodies, but because
`TextBuffer`'s edit API is the thing everything else builds on, and changing it later touches
every call site.

---

## 8. Latency budget

The numbers that make "Zed-level feel" testable rather than aspirational:

| Path | Budget | Measured |
|---|---|---|
| Cold start → interactive window | **< 100 ms** | 189 ms (M1.0, release, warm) — see below |
| Keystroke → glyph painted | **< 16 ms** (one frame) | — (M1.1) |
| `Send` keypress → bytes on wire | **< 5 ms** | — (M1.2) |
| Response arrives → status + headers painted | **< 50 ms** (at TTFB, not completion) | — (M1.2) |
| 10 MB JSON → first paint | **< 300 ms**, parse fully off-thread | — (M1.3) |
| Scrolling any response | **60 fps sustained** | — (M1.3) |

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

## 9. Dependencies to add

```toml
# core/Cargo.toml
reqwest      = { version = "0.12", default-features = false,
                 features = ["rustls-tls", "stream", "gzip", "brotli", "deflate", "cookies"] }
tokio        = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }
bytes        = "1"
http         = "1"
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
ropey        = "1"
thiserror    = "2"
anyhow       = "1"
[dev-dependencies]
criterion    = "0.5"

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

**M1.2 — Engine.** Tokio thread, `Engine::send`, `Event` stream, `build.rs` with typed URL
errors. Still no fancy rendering — dump the response as plain text. *Done when:* real request
goes out, real bytes come back, `Escape` cancels mid-flight, timing prints under `ZUNO_TIMING=1`.

> **This is the milestone that proves the thesis.** Ship nothing else until the
> type → send → see-something loop is genuinely tight. Everything after this is presentation.

**M1.3 — Response viewer.** `JsonOutline` + `uniform_list`. Status line, timing, headers
table. Folding. The >10MB raw-view cap. *Done when:* a 10MB JSON response scrolls at 60fps
and the UI never blocks.

**M1.4 — The loop.** Multi-line body editor. Resend, response diffing against the previous
run, request-local history, session restore of the scratch request. *Done when:* the full
edit → resend → compare cycle is pure keyboard and feels instant.

**Deferred by design, and it's worth naming them so they stop feeling like omissions:**
tabs/buffers, collections, the `Ctrl+P` / `Ctrl+K` palettes, environments and variables, auth
schemes, scripting, syntax highlighting, cookie jar UI, certificates. All of them are M2+.

---

## 11. Two open decisions

Neither blocks M1. Both get cheaper to decide with a working loop in hand.

**Persistence format.** "Local-first" fits both SQLite and a git-diffable file tree
(Bruno-style, one file per request). My recommendation: **file tree for collections**
(git-diffable collections are a genuine differentiator and match the local-first
philosophy) plus **SQLite for ephemeral state only** — history, response cache, window
session. M1 needs neither: serde the scratch request to a single file on quit and move on.
The `Serialize` derives from §3 keep this reversible.

**Tabs in M1 or M2?** `what.md` treats request-tabs-as-editor-buffers as thesis-level, but
`idea.md` scopes M1 to a single loop. I've put tabs in M2 above, on the reasoning that a
tab strip over a loop that doesn't yet feel good just multiplies a mediocre experience. The
counter-argument is real though: buffer semantics influence how `RequestSpec` ownership and
dirty-tracking are modeled, and that's cheaper to get right early than to retrofit. If you
lean that way, the concrete hedge is to have `Workspace` own `Vec<Entity<RequestPane>>` with
an `active_ix` from the start, and simply not render a tab strip until M2.
