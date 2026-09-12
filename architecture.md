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
│       │   ├── flatten.rs  ✅ iterative tokenizer -> Vec<Row>
│       │   └── format.rs   ✅ outline -> pretty/minified text, copying byte spans
│       ├── lines.rs        ✅ LineIndex for the raw-text fallback
│       ├── import.rs       ✅ one Import shape; sniffs which parser reads a document
│       ├── openapi.rs      ✅ OpenAPI 3.x -> requests, over serde_json::Value
│       ├── postman/         ✅
│       │   ├── mod.rs       ✅ Postman collection v2.x -> requests, folders, variables
│       │   └── script.rs    ✅ test scripts -> captures, assertions, expect_status
│       ├── diff.rs         ✅ ResponseDiff — summary comparison of two runs
│       ├── body_diff.rs    ✅ BodyDiff — the line-by-line body comparison
│       ├── disposition.rs  ✅ the filename a Content-Disposition asks for, sanitized
│       ├── hex.rs          ✅ hexdump -C of a body that isn't text
│       ├── html.rs         ✅ pulling readable text out of an HTML response
│       ├── curl.rs         ✅ curl command line <-> RequestSpec, both directions
│       ├── collection.rs   ✅ one-request-per-file on-disk format
│       ├── environment.rs  ✅ variables: two-layer resolution + on-disk format
│       ├── fuzzy.rs        ✅ subsequence scoring for the picker
│       ├── highlight.rs    ✅ JSON lexer for syntax colouring — tolerant, no cache
│       ├── search.rs       ✅ substring search over a response body
│       ├── headers.rs      ✅ common request header names + matching a partial one
│       └── version.rs      ✅ comparing a released version against the running one
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
        ├── context_menu.rs  ✅ the anchored menu primitive: rows, separators, commands
        ├── collection_panel.rs ✅ the collection tree — the browser beside Ctrl+P's finder
        ├── import_panel.rs  ✅ the import dialog: one field, URL or path, no format picker
        ├── commands.rs      ✅ the command palette's curated action table
        ├── settings_panel.rs ✅ per-request engine settings, as a modal
        ├── timing.rs       ✅ the ZUNO_TIMING switch, shared by boot and requests
        ├── theme.rs        ✅ Theme global; light + dark tokens; font resolution
        ├── ui.rs           ✅ icon set + asset source, icon/text buttons, tooltips
        ├── update.rs       ✅ the release check — a notice, never an installer
        ├── workspace.rs    ✅ root Render; owns buffers + all action handlers
        ├── request_view.rs ✅ one buffer: inputs + response + derived spec()
        ├── request_pane.rs ✅ method, URL bar, send, Headers/Params/Body tabs
        ├── response_pane.rs✅ status line, timing, body/headers tabs, body viewer
        ├── tests.rs        ✅ headless end-to-end tests (GPUI test platform)
        └── input/
            ├── mod.rs        ✅ word boundaries + undo history, shared by both
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
- **Form and multipart bodies had no add affordance at all**, which the icon work only found by
  accident: the four-way match written for the add control had two arms that nothing reached.
  `section_header` is drawn for Headers and Params; the Body tab draws `body_header`, so
  `Ctrl+Shift+F` and `Ctrl+Shift+M` were the *only* way to add a field or a part and nothing on
  screen said so. `add_control` is shared by both headers now. This is the discoverability audit's
  finding recurring a third time, and worth noting how it surfaced — not by looking for missing
  buttons, but because unreachable code in a `match` is visible where a missing element is not.
- **`ui::icon_text_action` is the third member of the family**, for a control where the icon
  carries the verb but a word says which table it acts on — a bare `+` in a section header would
  not. The add control was `"+ add"`, a glyph and a word hand-rolled together with no tooltip,
  and it called `add_row` directly rather than dispatching; it now names its keystroke and goes
  through `AddHeader`/`AddQuery`/`AddFormField`/`AddMultipartField`. The four arms differ only in
  which action they name, so the test asserts the *other* table stayed put — that is what catches
  a wrong one being wired in, and an assertion on the target table alone does not.
- **The window controls and the row-delete `×` are icons too.** They were `–`, `□`/`▣`, `✕` and
  `×` — characters, so a font missing a codepoint rendered tofu with no error, and `▣` for
  restore-vs-maximize is a distinction nobody reads. The row `×` also gained the tooltip naming
  `RemoveRow`'s keystroke, and a test: it acts on *its own* row index while the action resolves
  from focus, so the two paths can disagree in a way the action's own test cannot see.
- **The find bars use the same buttons as everything else.** Their step/replace/close controls
  were literal characters (`‹`, `›`, `×`) in a local `step_button` helper with no tooltip — so
  eight controls sat outside the rule this section exists for, and a missing codepoint would have
  rendered tofu with no error. `step_button` is gone and they are `icon_button`s; the two `×`
  buttons reuse the tab strip's `Close`. Conditional rendering keeps them out of `affordances()`,
  so `every_find_bar_button_is_painted_once_its_bar_is_open` covers them instead.
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
- ~~**`Timing.dns` / `connect` / `tls` stay `None`.**~~ **Two of the three are real now, and the
  third does not exist.** This bullet said reqwest exposes no per-stage timings and that getting
  them needs a custom hyper connector. Half right, and the half that was wrong was expensive: two
  hooks on `ClientBuilder` — `dns_resolver` and `connector_layer` — are enough for the lookup and
  for the connection, and neither is a connector. What genuinely does need one is splitting TCP
  connect from the TLS handshake, so those two are now **one** span rather than a `tls` field that
  could never be filled. `Timing` carries a `Connection` instead of three `Option`s; see §6h.
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

> **`Connected` was never built, and the timeline did not need it.** It is in this sketch
> because M1.2 assumed per-stage timings would arrive as their own event; nothing ever emitted
> one, and no reader noticed for several milestones because the fields it would carry were
> hardcoded `None`. When §6h finally measured them, an event turned out to be the wrong shape
> anyway: a redirect chain opens its sockets across the *whole* send, so a `Connected` fired
> once — necessarily before `Head` — would report the first hop's setup and leave the rest
> misattributed. The counters are read at `Done` instead. Left in the sketch with this note
> rather than quietly deleted, since "why is there no Connected event" is a reasonable question
> to have answered.

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

  **There are three now, and that last sentence was the whole cost of adding one.** The Timing
  tab (§6h) split `ToggleResponseView` into `NextResponseTab`/`PrevResponseTab` plus a
  `ShowResponse*` verb per tab, exactly as the request pane's strip already had to. The active
  tab stays inert, now for the weaker of the two reasons: a click dispatching its own tab is
  harmless, it merely advertises a change that never comes.

  It also turned a test from decoration into coverage without touching its assertions.
  `clicking_the_headers_tab_switches_the_response_view` carried a comment admitting it could not
  tell a dispatch from a direct call, because with two tabs and a cycling action both routes end
  in the same state. Timing is *two* steps from Body, so a cycling handler lands on Headers and
  the new block fails with exactly that — the same shape as the `curl -L` case in CLAUDE.md's
  Lessons, where the thing that made the old test vacuous was itself the defect.

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

### Find and replace in the request body

`Ctrl+F` worked everywhere except the surface you type into. It now means "find in what you are
looking at" — the body in the editor, the response anywhere else — which is the same shape as bare
`enter` sending in the URL bar and inserting a newline in the editor.

- **Two bars, not one made target-aware.** Both can be open at once, because what you are sending
  and what came back are different questions. One bar would have to be told which it meant and
  then moved between panes to sit beside it. They share `TextSearch` and `step_button`, which is
  where duplication would actually have cost something.
- **`ResponseSearch` became `TextSearch`**, and `rows` now means "which display line" in whichever
  surface owns it — outline rows for the response, editor lines for the body. Leaving the old name
  would have had every reader assume the request side kept its own copy of this logic.
- **The body scan runs on the UI thread**, unlike the response's. A response can be 100MB and
  invariant 3 is not conditional on today's being small; a request body is hand-authored, which is
  the same argument §7 makes for dropping the rope. Spawning a task per keystroke to search a few
  kilobytes costs more than the search.
- **Stepping moves the caret**, which is what makes this an editor's find rather than a viewer's:
  it is where typing resumes, it is what `ReplaceNext` acts on, and it drags the horizontal scroll
  onto the match for free through the caret-following clamp.
- **The first match is revealed as you type**, not on the first `Enter`. Only stepping revealed at
  first, which the response's `apply_search` had never done — so a query selected nothing and
  scrolled nowhere until you pressed Enter, and with a single match that meant pressing Enter to
  travel to the match you were already on.
- **Vertical scrolling had to be added; horizontal came free.** Prepaint drags `h_offset` to the
  caret whenever the caret has moved, so a match far along a line arrives on its own. Nothing did
  the same downwards, so a match a hundred lines down was reported and then left off screen.
  `Editor::scroll_caret_into_view` is the missing half.
- **Replace-all runs backwards.** Replacing front to back invalidates every offset after the one
  just written the moment the replacement is a different length — the test that catches this uses
  a *longer* replacement on purpose, because with equal lengths the bug cannot appear.
- **A replace-all is one undo entry.** It is one gesture, so it has to come back in one press.
  `Editor::replace_ranges` records a single snapshot and suppresses the per-splice ones, still
  going through `replace_text_in_range` so the line rescan and the `Changed` emit are not
  duplicated.

> **The weak assertion that hid the undo bug is worth naming.** The test asserted `assert_ne!`
> against the post-replace text — true after *any* change, including an undo that unwound a single
> match and left the rest. It passed while a replace-all took one press per match. Asserting the
> original text instead is what turned it load-bearing. Sixth of these now.

### Syntax highlighting — a lexer, not a parser

The response outline was always coloured, which made this look done: `json_row` reads the `Row`'s
typed fields — the `key` span, `ScalarKind`, `RowKind` — and paints from `theme.syntax`. That is
*structural colouring of an already-parsed outline*, and it only works because the body was
flattened off-thread first. Two surfaces had no colour at all: the request body editor, and the
raw view that non-JSON, over-cap and failed-to-parse bodies fall back to.

**Principle 4 priced this as tree-sitter plus a highlight cache, and that was wrong for JSON.**
Three things it did not account for:

- **A lexer, not a parser.** `json::flatten` rejects structural errors, which is right for a
  viewer and useless for an editor: text being edited is invalid on most keystrokes. Colour
  survives that, because knowing a token is a string never requires knowing whether it nests.
  `core/src/highlight.rs` is therefore tolerant by construction — an unterminated string runs to
  the end of the line and is still a string, which is the state of every string between typing its
  opening quote and its closing one.
- **No cache, because JSON has no multi-line tokens.** Strings cannot contain a raw newline and
  there are no block comments, so every line is lexed independently and only the visible ones are
  lexed at all. The cache that made this expensive is a requirement of XML and HTML.
- **Styled runs already existed.** The editor was building a three-run `Vec<TextRun>` for the IME
  pre-edit underline. Colour is *more runs*, not new machinery.

**Overlapping styles must be split, not layered**, and this is the one real trap.
`StyledText::compute_runs` walks its highlights doing `range.start - ix`, so it needs them sorted
and disjoint — a gap paints the wrong text and an overlap underflows a `usize`. A search match
inside a coloured token cannot be pushed on top of it; the token has to be cut at the match's
edges. `ui::split_spans` collects every boundary either rule cares about and asks, per segment,
what applies. Nesting the two rules was the first attempt in the editor and it needs a branch per
way one range can straddle another; this needs none. The editor turns those segments into
`TextRun`s and the response into `HighlightStyle`s, which is the only part that differs.

**One mechanism, two sources of tokens.** The outline already knows its tokens structurally and
does not use the lexer; the editor and raw view have no structure and do. Both then go through the
same splitter and the same palette — `ui::syntax_colour` is the single match from kind to colour,
because a body authored in the editor and the response it comes back in reading as two different
languages is the failure that matters here.

Two consequences worth recording:

- **`json_row` became one `StyledText` instead of up to five divs**, which is what makes a
  per-character match highlight possible at all — and incidentally collapses five elements per row
  into one on the surface that has 1.31M of them. §6's note that match highlighting is per-row is
  retracted above.
- **`body_kind` is private now.** The editor's colouring is derived from it and they live in
  different entities, so a public field is how the two drift: assigning it directly still compiles
  and leaves JSON painted flat, or XML painted as JSON. `set_body_kind` is the funnel, for the
  reason `Workspace::activate` is one.

*Still deliberately absent:* XML and HTML. The lexer is JSON-only, and the editor turns colour
*off* for other kinds rather than mis-colouring them — lexing HTML as JSON tints a stray attribute
green for no reason. Those need the cross-line state this design skipped, and they can have it
when someone wants them.

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

**Match highlighting was per row, and is now per character.** A row used to be assembled from
separately styled key, punctuation and value *elements*, which can only be coloured whole — so a
match tinted its entire row. Highlighting the bytes meant splitting a shaped run, which is the
syntax-highlighting problem, which is why the two landed together. See §6's highlighting section.

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

**The application menu is the second consumer, and it is deliberately not a mouse palette.**
Every verb already has an icon button and a palette row, so a menu repeating them would be a
second command list with no drift test watching it. It carries what had no home instead — the
version, the links, and quitting — plus three ways *in* for someone who does not yet know
`Ctrl+K` exists. `F10` opens it, the desktop convention, so the button teaches a keystroke like
every other. It is excluded from the palette with a reason: reaching a discovery menu from the
palette is backwards.

Three things the primitive grew for it. `MenuRow::Separator`, because Quit sitting next to a link
is how a misclick happens — and `select` steps *over* separators, since landing on one is a
keypress that appears to do nothing and `confirm` there emits nothing either. `MenuCommand`, so a
row can open a URL instead of dispatching; both travel out through `Chose` so the opener still
closes before acting, which is one ordering rule rather than two. And the detail column stopped
being called `keystroke`, because the About row puts the version there.

The menu anchors at a fixed point below the titlebar rather than at the cursor: it belongs to a
button, and one that opens wherever you clicked reads as a context menu.

`app/src/context_menu.rs` is a **primitive**, not a response-pane feature, for principle 2's
reason — saved requests want delete/rename, the tab strip wants close/rename, header rows want
toggle/remove. Two of those three have since arrived (§6a's row menu, §12's tab menu), which is
the leverage that argument predicted; header rows still have none. It is also the first genuine consumer of `anchored()`, a question §12 left open
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
- **Hover moves the selection; it does not add a second highlight.** The rows had a selection
  background and no mouse feedback at all, and the obvious fix — a `hover` style — would light
  the pointed-at row while `selected` still lit another, so `Enter` fired the row you were *not*
  pointing at. Moving the selection keeps one highlight that always says what Enter will do, and
  it is testable at that consequence: hover the second item, press Enter, and the clipboard holds
  the path rather than the value. The pointer cursor is the other half.
- **Every item is an action**, with its keystroke read from the live keymap. So the menu teaches
  the shortcut instead of replacing it, and it cannot drift from what dispatch does.

  > **This sentence was true of the intent and false of the code, from the day it shipped until
  > the collection panel's menu made it obvious.** `MenuItem::new` called `keybinding_label`,
  > which asks `Window::bindings_for_action` — and that matches against
  > `rendered_frame.dispatch_tree.context_stack`, a **build-time** stack that `pop_node` empties
  > as the frame finishes. An empty stack matches only `None`-context bindings, and all three
  > verbs here are scoped to `ResponsePane`, so all three drew a blank column. The fix is
  > `bindings_for_action_in`, which rebuilds the stack from a focus handle — so `MenuItem::new`
  > now takes the handle the menu restores focus to, which is the pane the verbs act on and
  > therefore the same question.
  >
  > Fourth of the "code drifted from a correct comment" family, and the most durable: the
  > failure is a *missing* string, so the menu looked deliberate rather than broken. The test
  > that should have caught it, `keybinding_label_matches_the_keymap`, asserts over
  > `advertised_actions` — every one of them globally bound. It pins the formatter and could
  > never see the lookup. `the_response_row_menu_names_its_keystrokes_too` asserts a scoped one.
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

## 6a. The collection panel — a browser, not a second finder

`Ctrl+P` was the only thing in the app that read a collection: `collection::scan` had **exactly
one caller**, the picker. A fuzzy finder answers *"take me to the thing I am thinking of"* and
needs you to know its name; nothing could answer *"what have I got in here"*, which is the
question you open a collection to ask after a week away. That is a browsing gap, not a search
gap, and no amount of ranking closes it.

So the two surfaces stay separate rather than one growing a mode. `Ctrl+P` ranks open buffers
first so it doubles as a tab switcher from the first press (ROADMAP), and folding a tree into
that makes it worse at both jobs. Every editor ships a file finder *and* a file tree.

**The tree is built in core and folded in the app**, the same split `JsonOutline` makes between
`rows` and `visible`. `collection::tree` turns `scan`'s entries into a flat, depth-tagged
`Vec<Node>`; which directories are collapsed is window state and never reaches core. Flat rather
than nested because the panel renders through `uniform_list`, which needs "what is row 40"
answered in O(1) — the same constraint §6 is built on, at hundreds of rows instead of 1.31M.

Decisions worth keeping:

- **Directories sort before files at each level, and that ordering has to be rebuilt.** `scan`
  returns entries sorted by relative path, which interleaves the two: `alpha.json` precedes
  `beta/x.json` because `.` sorts before `/`. Inheriting that order puts a root-level request
  above a directory, which no file tree does. `tree` builds a nested `BTreeMap` and flattens it,
  so each level is ordered as it is filled and no comparator has to know which kind it is
  looking at.
- **`Node::path` is a path, not a display name.** It is the fold key and the identity a later
  delete or rename will act on, and two directories at different depths can share a name — a
  `HashSet<String>` of collapsed names would fold both. `two_directories_sharing_a_name_get_
  distinct_paths` pins it.
- **`NodeKind::Request` carries the method and URL, not the `RequestSpec`.** The panel draws
  both; a spec carries the body, which for a large request would be cloned on every scan and
  held for as long as the panel is open.
- **The selection is restored by *path* across a rescan, not by index.** This is the one that
  bites. `panel_selection` indexes into `tree`, and `refresh_tree` replaces `tree` wholesale, so
  saving a request whose name sorts earlier shifts every row after it and the same index
  silently means a **different request** — with nothing on screen saying so. The test uses an
  alphabetically-first new name on purpose: with a later one the index survives by luck and the
  test passes either way.
- **No selection clamp on fold, and a guard for it was written and deleted.** The response
  viewer needs one because a fold can hide the row its cursor is on. This cannot: both fold
  paths — the click and `CollectionCollapse` — select the directory *before* folding it.
  Breaking the guard on purpose changed no test, which is what proved it unreachable; four other
  guards in §6 went the same way. A new fold path must select first, and `rebuild_tree_visible`
  says so.
- **Opening a request leaves focus in the panel**, on both paths, and the explicit re-focus is
  what makes them agree. `open_collection_file` routes through `activate`, which focuses the URL
  bar; on a *click* the panel's own `track_focus` listener fires afterwards and takes focus
  back, so without the explicit call `Enter` and click would land focus in different places —
  the "click and keybinding are different verbs" failure, from a direction no convention
  catches. Staying is also the better of the two: browsing means opening several in a row, and
  that needs the arrow keys to keep working.
- **The selection follows the active buffer, not only the other way round.** Clicking a row
  activated that buffer and the strip followed; switching buffers left the tree highlighting
  whatever had last been clicked in it. The fix lives in `Workspace::activate` — the one funnel
  every switch already goes through — so the four verbs, a tab click, middle-click, the picker
  and the panel all inherit it. Three decisions in it:

  A buffer with **no file leaves the selection alone** rather than clearing it. Beyond keeping
  your place in a large tree, the reason that matters is that New request and New folder *read*
  this selection to decide where they create things, so clearing it on every `Ctrl+T` would
  silently move them to the collection root. Break-testing the alternative fails seven tests,
  which is that side effect measured.

  A file inside a **collapsed folder expands it**, ancestors first and then the selection —
  because a selection on a row nothing paints is a cursor the reader has lost, which is
  `rebuild_tree_visible`'s own warning and the thing collapse-all had to learn. The cost is
  accepted: switching tabs can unfold a folder you deliberately closed.

  It **never moves focus.** Focus belongs wherever the switch put it.

- **The rows had to be `w_full`.** Third time. `uniform_list` hands a row the list's width as
  *available space*, and taffy only stretches a root node to fill it for `display: block`; a
  `.flex()` row sizes to its content. The picker (§12) and the response body (§6) both shipped
  this. The test clicks the far right of the row and measures against the **list's** bounds,
  because the row's own bounds agree with the bug.
- **Visible by default, and persisted.** A browser nobody discovers is the discoverability
  failure §2 is mostly about, so it starts on; a panel you dismissed reappearing every launch is
  the kind of small disobedience that makes an app feel like it is not listening, so the flag
  rides in the session envelope (v4). Spelled out as its own `SessionV3` rather than given a
  serde default, per invariant 8 — a default cannot tell "written by an older Zuno" from
  "written by this one, with the panel hidden", and those two want opposite answers.
- **Resizable, and the clamp runs at render rather than at the drag.** The width joins the
  visibility flag in the envelope (v5, with a spelled-out `SessionV4` for the same reason), and
  is stored **unclamped**: the ceiling is `min(600, 50% of the viewport)`, so clamping on the way
  in would permanently shrink a width just because the window happened to be narrow when it was
  set. Reading it back through `clamp_width` instead means a 600px panel restored onto a laptop —
  or a window since dragged narrower — is reined in on the frame that draws it. Clamping only on
  input leaves the panel eating the request pane with nothing to notice.

  **The drag is `on_drag`/`on_drag_move`, not `on_mouse_down`/`on_mouse_move`,** and the obvious
  pair is not a style choice but a bug: a `div`'s move listener is gated on `hitbox.is_hovered`,
  so the drag dies the moment the pointer outruns the 5px strip — which a fast drag does inside
  one frame, making it work when you drag slowly and feel broken when you don't. `on_drag_move`
  fires in the capture phase for every move anywhere in the window while the drag is live.
  Rejected alternative: a full-window transparent overlay painted during the drag, so the move
  listener always has a hovered hitbox. It works, and it is one more element plus a piece of
  state that has to be torn down; gpui clears `active_drag` itself, so the typed-payload route
  has no end-of-drag flag to forget.

  **The pointer maps to a width absolutely, never by accumulating deltas**, and the first
  version got this wrong in a way worth recording because of how it presented. It added
  `position.x - bounds.center().x` to the panel's *current* width, which reads as
  self-correcting and is not: `DragMoveEvent::bounds` is last frame's hitbox, while the current
  width has already been advanced by every earlier event in the same batch — and a mouse
  reporting at 500Hz against a 60Hz window delivers six or seven moves per frame. Each one
  re-added the whole travel from a reference that had not moved, the frame painted the
  overshoot, and the next batch measured back from there and pulled it in. The report was
  **"it flickers — you see the current and past frames at once"**, not "the width is wrong",
  so it reads as a vsync or rendering fault rather than as arithmetic. `width_from_drag` pairs
  the stale bounds with the width *those bounds were painted at* — captured in the closure at
  render, deliberately not re-read — which recovers the row's left edge, a value that does not
  move during a drag.

  This is the "a frame behind" rule (§6's scrollbar) arriving from a direction that looks safe,
  because an *event* feels current even when the bounds attached to it are not.

  **The handle is absolutely positioned over the seam, not a sibling in the row**, so a strip
  wide enough to hit costs neither the panel nor the panes any width. It is emitted last,
  because paint order is what decides hit-testing between overlapping siblings. Hover and drag
  paint over the panel's right border — which is also its focus ring — and that override is
  deliberate: hover is transient and only visible while the pointer is on the seam, while focus
  stays legible on the panel's other three edges.
- **Collapse-all is the new fold path `rebuild_tree_visible` warned about.** That comment says
  the panel needs no selection clamp because every fold path selects the directory before folding
  it, and that a new one must do the same. This is that new one: it folds the whole tree at once,
  so a selection on a nested request would be left on a row nothing paints — invisible until the
  next `down` jumps from wherever the cursor secretly still was. `outermost_ancestor` walks the
  selection up to its depth-0 row *before* collapsing, while the depths still describe a visible
  tree. Expand-all needs none of this, and the asymmetry is the point: expanding only ever adds
  rows, so nothing selected can stop being drawn.

  **Two buttons, not one that toggles.** A single control has to read the tree to decide its
  meaning, and a half-collapsed tree has no honest answer — you could not expand-all from one
  without collapsing everything first. The response pane's `fold all` / `expand` pair is the same
  shape.

  The test asserts the *consequence* rather than the selection alone: after collapsing it presses
  `down` and requires the next row **on screen**. Left nested, the selection is still a valid
  index into `tree` and reads back fine — it is only the following keystroke that exposes it.

- **The toggle lives in the titlebar, not in the panel.** It shipped as a `×` in the panel's
  own header, which can only ever perform *half* of a toggle: pressing it took the button away
  along with the panel, and since `panel_visible` gates the whole render there is no rail or stub
  left behind — so the only ways back were `Ctrl+Shift+E` and the palette, neither of which is on
  screen. That is §2's discoverability gap inverted: the window taught you how to lose the panel
  and nothing taught you how to recover it.

  The glyph was wrong independently of where it sat. `Icon::Close` is the same `×` used by the
  window close, the tab close and the row delete, all of which destroy something; this only
  changes what is drawn, and the state is persisted, which makes it a view preference rather than
  a dismissal. `PanelLeftOpen`/`PanelLeftClose` name what the click will *do*, the rule the
  maximize button and the theme toggle beside it already follow.

  `ToggleCollectionPanel` is unchanged, and deliberately so — its three-state behaviour (hidden →
  show and focus, visible-but-elsewhere → focus, visible-and-focused → hide) is right for the
  binding, and a button that is always on screen no longer needs a verb of its own. The cost is
  known and accepted: clicking the toggle while the panel is open but *unfocused* focuses it
  rather than hiding it, so that case takes two clicks.

  **The test asserts the click, not the button's presence.** `debug_bounds` reads the last
  rendered frame and a removed element keeps its entry until another is drawn, so a lookup cannot
  distinguish "still painted" from "stale". `the_panel_toggle_is_still_there_once_the_panel_is_
  hidden` hides the panel and clicks the toggle, asserting the panel comes *back*;
  `affordances()` cannot cover this, because it renders the default state where the panel is
  visible and the button is present either way.
- **Not a tab stop**, unlike `response_focus`. `Tab` walks the active request's inputs, and a
  pane-level stop painted before all of them would turn the first `Tab` from "url → method" into
  "panel → url" for every existing user. It has a binding and a click target and loses nothing.
- **Fixed width, no drag handle.** A resizable panel means a stored width, a minimum, and a
  pointer mode, to serve a preference nobody has expressed. Recorded as a limitation rather than
  an oversight.

**The panel is a full-height column, and the tab strip belongs to the editor area beside it.**
It shipped the other way for a slice — the strip spanning the whole window, above both — which
drew a row of open *buffers* across the top of a tree of saved *files*, describing something the
panel has nothing to do with. The status bar still spans the width, and that is the same
convention rather than an inconsistency: a status bar describes the application, a tab strip
describes one pane.

Two consequences, and the second is the one worth holding:

- The editor column carries `min_w(0)`. The panel is `flex_none` at a width it sets itself, so
  the panes are what must give when the window narrows; without it the column's content sets a
  floor and the two together overflow instead. This is also why the panel's own width is clamped
  to a fraction of the viewport rather than only to a constant — `flex_none` means nothing else
  will stop it.
- **The panel no longer moves when the strip appears.** The strip hides itself at one buffer, so
  under the old layout opening a second one pushed the whole panel down — which is exactly what
  made `clicking_a_request_twice_activates_it_rather_than_opening_a_second_copy` read stale
  bounds between its two clicks and pass against the bug it was written for. A layout where a
  sidebar jumps on an unrelated event is a source of that class of defect, not just an eyesore,
  which is why `the_tab_strip_belongs_to_the_editor_area_not_the_window` asserts the position
  *and* the stability as two separate claims — each fails on its own.

**Opening a request already open activates it** rather than opening a second buffer. `Ctrl+P`
gets this by filtering open paths out of its list; the panel cannot, because a tree that hid the
requests you have open would be worse than the duplicate. So the rule lives in
`open_collection_file`, where both paths inherit it — and the picker gains the case its filter
cannot cover, since that filter is only as fresh as the scan behind it. Two buffers over one file
also means two `path`s pointing at it and a last-write-wins race on `Ctrl+S`.

### New request — and why saving needed no change

A folder could be created at any depth from the panel header or a row menu; a **request** could
only be created as a scratch tab (`Ctrl+T`, or the `+` in the tab strip). So the gesture a tree
most invites on a folder offered you another folder but not the thing folders hold, and filing a
request took three steps: new tab, save (to the root), move.

`Ctrl+N` in the panel, a `New request` row on both a folder *and* a request (where it means
"beside this one", the way New folder already did), and an icon in the panel header.

**The gesture is New folder's, exactly** — the same inline box, drawn at the same computed row and
depth, expanding a collapsed parent first, cancelled by focus-out. `NewFolderState` became
`NewNodeState` with a `kind` rather than growing a twin: the fiddly half is the row placement (a
collapsed parent, a *visible* index, the depth), and having that twice is having it drift.

**The point is that the request is born with a path.** `create_request` opens it through
`open_collection_file`, which is where a buffer *remembers its file* — so the next `Ctrl+S`
overwrites this request rather than deriving a fresh name at the root. That is the whole reason
this slice needed no change to saving, and it is the invisible half: the file landing in the right
folder is obvious on screen, and whether the buffer remembers it is not.
`a_request_created_in_a_folder_saves_back_into_that_folder` asserts the second save, not the
creation, and fails against a version that opens a plain buffer.

**`Ctrl+S` is unchanged, deliberately.** A destination picker on a never-saved buffer's first save
was considered and dropped for this slice — the common case is now "the request already knows where
it lives", and Save-then-Move still covers a scratch tab. What stays rejected for the original
reason is `Ctrl+S` reading the *panel's selection*: that is state you are not looking at when you
press the key.

The cost, stated: an abandoned empty request sits in the tree and in git. That is the identical
trade New folder already makes with an abandoned empty folder, and `delete` handles both.

**And it shipped with a folder glyph on a new request.** The inline row's *placement* was
generalised for both kinds and its *glyph* was not — `new_folder_cell` hardcoded `Icon::Folder`,
and reading the placement would never have shown that. Reported by the human after testing, which
is the expensive way to find it. Now that the two kinds of row no longer share a column,
`new_node_cell` calls the same cell constructor the row does.

**A vacuous test, caught on the way.** `Icon::ALL` is hand-written, so adding an `Icon::FilePlus`
variant did not add it to the list the two icon tests iterate — both passed without ever loading
the new asset, which had no `Assets::load` arm at all. Putting it in `ALL` turned them red
immediately. Worth recording because it is the sixth "a weak assertion reads exactly like a strong
one": the test was correct, comprehensive-looking, and enumerating a list that a new variant does
not join.

### Delete — two actions, because a file has no undo

`DeleteRequest` only *asks*; `ConfirmDeleteRequest` is the only thing that removes anything.
Splitting the verb is what lets the confirmation be an ordinary second menu rather than a modal
of its own — it inherits the primitive's keyboard handling, its `Escape`, and its occlusion, and
the destructive row names the file, because "are you sure?" without a subject is how the wrong
thing gets deleted confidently. `MenuCommand::Dismiss` is the way *out*: `Escape` already works,
but a menu asking to delete a file with no visible answer except the destructive one reads as
having no way back.

- **`collection::remove` takes one file and refuses a directory.** `remove_dir_all` on a path
  derived from a UI selection has no undo, and a folder can hold work the panel never showed —
  an unreadable request is skipped by `scan`, so it has no row, and would be destroyed anyway.
- **A file already gone is not an error.** The tree is a snapshot of the last scan, so a request
  deleted in a terminal a moment ago is still drawn; failing for reaching the state the caller
  asked for is noise.
- **The panel refuses a directory too, and that guard is separate from core's.** Core's refusal
  makes the *outcome* identical either way, so a test asserting "the directory survived" passes
  against a UI that offers the verb — the first version of that test did. What the UI guard adds
  is not offering a control that can only fail, so it is asserted on `menu_open`.
- **Any buffer open on the deleted file forgets its `path`.** `save_request` writes to a
  remembered path with **no existence check**, so leaving it set means the next `Ctrl+S` silently
  recreates the file you just deleted. The buffer itself stays open, which is right: the request
  is still in front of you, it simply has no file any more.
- **`delete` is bound in the panel; `ConfirmDeleteRequest` is bound nowhere.** A destructive verb
  one keystroke away is what the confirmation exists to prevent. Neither is offered in the
  palette — `DeleteRequest` would aim at a selection the palette cannot show you.

### The rest of the row menu

Eight verbs now, grouped by consequence — leave Zuno, make a copy, read something out, change or
remove the file. The two destructive rows sit last and together, so the pointer never crosses
them on the way to something harmless, and only one of them stops to ask.

- **Rename is inline, not a modal**, which is what let it land at all. A `TextInput` is drawn in
  the name's own place carrying its own key context (`"TextInput CollectionRename"`), so `Enter`
  and `Escape` mean commit and cancel *there* without touching what they mean anywhere else —
  and the binding for each sits **after** its global twin in `register_keymap`, because a
  leaf-matching predicate ties with a context-less one and the tie goes to later registration.
  Sixth time that ordering has decided behaviour. An earlier plan had rename waiting on a
  "type a new name" modal; the tree row *is* the text box, so no modal was needed.
- **Cancel on blur, not commit.** VS Code commits when a rename box loses focus. A rename here is
  a file operation, so the safe reading of "clicked somewhere else" is that it was not meant.
  `cancel_rename` `take`s the state, which also makes it idempotent — committing drops the state
  and then moves focus, which fires the blur listener, which dispatches a cancel that must find
  nothing to do.
- **The buffer follows a rename and forgets a delete.** Both act on an open buffer's `path`, in
  opposite directions, and for one reason: `save_request` writes to a remembered path with no
  existence check. After a rename the request still exists, so `path` is *updated* — left stale,
  the next Ctrl+S recreates the file under its pre-rename name and you have both. After a delete
  or a trash there is nothing to point at, so it is cleared.
- **`rename` takes a label, not a path.** It goes through `slug`, so a typed `../../evil` cannot
  walk out of the directory — the same boundary `allocate` relies on, and the reason the
  signature refuses a caller-built `PathBuf`. It never overwrites: a name already taken is an
  error, because the request being clobbered may be one the renamer has never seen. Renaming to
  the name it already has is a no-op rather than a collision, or opening the box and pressing
  Enter reports "already exists" about the file being renamed.
- **Duplicate copies bytes, and opens no tab.** Re-serializing a `RequestSpec` would normalize
  the file — rewriting anything a future field or a hand edit put there — and a duplicate that
  differs from its original is a bad duplicate. It does not open the copy: duplicating is how
  you take a backup before a risky change as often as it is how you start a variant, and a tab
  you did not ask for is the worse of those two failures.
- **Trash asks nothing and delete asks.** The asymmetry *is* the design: the confirmation exists
  because a delete cannot be undone, and a dialog in front of a recoverable action only trains
  people to dismiss dialogs. `trash` refuses a directory for `remove`'s reason — the path comes
  from a UI selection and `trash::delete` would take the whole folder.
- **Reveal and Open-in-default-app are one call each, and untestable.** `reveal_path` and
  `open_with_system` are both `unimplemented!()` in gpui's test platform, so nothing can drive
  them headlessly — the same shape as `prompt_for_paths`. The handlers are kept to a single call
  so the untestable part is as small as possible, and what *is* asserted is that the menu offers
  the row, which is where a mistake would actually be.

> **Trashing is deliberately not driven in any test, and that leaves a real gap.** The XDG trash
> is redirectable — `trash` reads `XDG_DATA_HOME` per call — but only through a process-wide env
> var, which is unsafe under edition 2024 and racy across parallel tests; the harness uses
> explicit globals (`install_at`) precisely to avoid env-based redirection, and trashing scratch
> files into the developer's own trash is invariant 6's territory. So `trash_request`'s
> bookkeeping is covered only indirectly: `forget_path` and `refresh_tree` are shared with
> delete, which *is* tested end to end. What nothing catches is `trash_request` failing to call
> one of them. Written down rather than papered over.

### Organising — new folder, and move

Until these landed the collection could only ever be **flat**. `save_request` calls
`allocate(&root, …)`, so every `Ctrl+S` writes to the collection root; nothing made a directory,
and nothing put a request in one. The tree showed a flat list to anyone who had not built folders
by hand in a terminal, which made the panel a viewer of a structure the app could not produce.

- **`Ctrl+S` still writes to the root, deliberately.** Saving into whatever the panel happens to
  have selected is faster and depends on state the user is not looking at when they press the
  key — the invisible-state failure this codebase keeps paying for. Save, then move: two steps,
  both predictable, and it is what a file manager would make you do too.
- **New folder follows the selection**, the file-tree convention: inside a selected directory,
  beside a selected request, at the root when nothing is selected. It has a button in the panel
  header as well as `Ctrl+Shift+N` and a menu row, because a verb reachable only by right-click
  is the discoverability gap §2 exists for. The button is `FolderPlus` rather than a bare `+`:
  the header holds one control now, so the glyph has to say *what* it adds on its own, and it
  shares the silhouette of the `Folder` rows it creates.
- **The name box is a row in the tree, as the *first* child of its parent.** It shipped as a strip
  under the header naming its destination (`billing` beside the box), on the reasoning that a
  phantom row would have to be threaded through the fold walk, the selection clamp and every
  index translation. That reasoning was about the cost, not the result, and the result was worse:
  a box that *says* `billing` describes the destination, while a box sitting one indent inside
  `billing` **is** the destination, which is what every editor does and what a reader already
  knows how to read.

  The cost turned out to be one index translation in the list closure and nothing else.
  `tree_visible` is left alone — no placeholder spliced into it — so the index the selection, the
  fold walk and `scroll_to_item` all address still means exactly one thing; the list is simply one
  row longer while the box is open, and rows at or past the insertion point shift down by one.
  Get that shift wrong and the rows below render the *wrong nodes*, which `tree_rows` cannot see
  because it reads workspace state rather than the rendered list — hence a test on the position
  itself.

  **First child, not last** — last was the first attempt, and in a folder holding a screenful of
  requests it opened the box off screen, which is exactly the folder you are most likely to be
  reorganising. Sorted position is the third option and is worse than both: the row would jump as
  you type, and the name is not final until Enter. The rescan that follows re-sorts it, which is
  the moment it *is* final.

  The box reserves **both** of a row's leading columns at their real widths. The first version put
  the folder glyph in the chevron column and shrank the next one to compensate, which started the
  input 14px left of where a folder name starts — the box did not line up with the row it was
  about to become. The chevron slot is reserved and empty: nothing to expand yet, and a chevron
  that toggles nothing is a dead control.

  Two more things follow from being in the tree: a collapsed parent is expanded first, or the box
  has nowhere to appear; and the list scrolls to it, since nothing else would. It shares
  `CommitRename`/`CancelRename` with rename, because `Enter` and `Escape` mean the same thing to
  both and a second pair of actions would be two ways to say one word.
- **A directory earns a row by existing, not by holding a request** — and this shipped wrong
  first, so the correction is the useful part. `tree` derived its directories from `scan`'s
  entries, which meant a folder you had just created was **invisible** until you put a request in
  it, and the move picker (deriving destinations the same way) offered only folders that already
  had rows. So **New folder and Move could not compose**: the one verb that fills a folder
  refused to see the folder you had just made. Neither was broken alone, which is why a test of
  each said nothing; `a_folder_you_just_created_can_be_moved_into` is the one that would have.

  The reasoning written down for it was the actual defect: *"offering a destination that will not
  appear afterwards is worse than not offering it."* Backwards — moving a request in is exactly
  what makes it appear. `collection::folders` walks the real tree now, with `walk`'s skip rules
  so the two agree about what a collection contains, and `tree` takes it alongside the entries.
  The status line's apology ("appears once a request is in it") went with it: a notice explaining
  a design flaw is not a fix.
- **Move is a picker, not drag-and-drop.** Drag is a gesture nothing else in Zuno uses, the
  headless platform cannot observe it, and it needs a drop-target hit test per row. The picker is
  the eighth `Target` variant and needed no new interaction — still no `PickerDelegate` trait,
  since it draws as label plus dimmed detail like the other seven.
- **The request's own folder is offered and marked, not filtered out.** Removing it would
  renumber the list depending on where the request happens to live, so the same collection would
  present a different set of rows for each request in it. `move_to` treats it as a no-op rather
  than reporting "already exists" about the file being moved — the same rule `rename` follows.
- **The buffer follows a move**, as it does a rename and for the same reason — the request still
  exists, so `Ctrl+S` must overwrite it where it now lives rather than recreate it where it was.

**A row's name is one line, clipped, with the full text on hover.** It shipped *wrapping*, which
is not the same failure and does not look like one: gpui's default is `WhiteSpace::Normal`, so a
long name reflowed onto a second line, and the row is a fixed `ROW_HEIGHT` because `uniform_list`
demands it — so the second line was sliced through the middle. That reads as a rendering fault
rather than as a name too long for a 232px panel, which is exactly how it got misdiagnosed as
horizontal clipping. `whitespace_nowrap()` is the fix, and it is the *dependable* half of
`truncate()`: a flag the shaper reads, with none of the cached-measurement fragility that makes
the ellipsis unreliable (CLAUDE.md).

The tooltip is attached **only when the name is over budget**, because one repeating a name you
can already read is noise on every row. `name_budget` computes that from the panel's width, the
row's chrome and the depth's indent, against the same `5.95px` advance `TAB_LABEL_CHARS` is tuned
to — computed rather than measured for the reason `elide` is: the test platform has no shipping
font, and a pure function over a string is something a unit test can check. Both its failure modes
are silent — zero puts a tooltip on every row, an enormous value on none — so the test asserts a
bounded range rather than a value.

**Requests carry the method's name, not a pictograph.** `DELETE` and `OPTIONS` are cut to `DEL`
and `OPT`, the only two that don't fit `METHOD_WIDTH`. HEAD and OPTIONS share `method_other`, so
the text is the only thing telling them apart — which is what the test asserts.

**Directories carry a folder icon in a narrower column of their own.** Sharing the method column
kept both kinds of name at the same x, which stopped being worth it once that column grew to hold
a label. `name_budget` returns more room for a folder name now, and its test asserts that
inequality rather than the old equality.

**The header is a menu button, and the empty state has three cases.** Opening a directory that
holds other things showed "Nothing saved yet", which is the message for an *empty collection* —
so `scan_counted` returns how many ordinary files it passed over and the notice says which
problem you have. Dotfiles are not counted: they are skipped everywhere else without comment.

The four workspace verbs shipped with a palette row each and one mouse path between them — §2's
failure recurring, since a binding and a palette row both satisfy the convention checklist and
neither can be seen. The panel header carries them now, as `ui::menu_button`: a word with a
trailing chevron, the mirror of `icon_text_action` whose glyph leads. Without the chevron the
header was muted text that happened to be clickable.

**And it was `flex_none`, which took the four controls beside it off the panel.** `flex: none`
pins `flex-shrink: 0`, so a long workspace name in a `justify_between` row pushes New request,
New folder, Collapse and Expand out — four controls with no mouse path left. §12's picker label
was the same bug. The test asserts each control's bounds against *the panel's* width, since an
off-screen button still has bounds that agree with the bug.

### Folder verbs — and the guard that had to be inverted

Right-clicking a folder opened nothing, because `open_collection_menu` guarded on
`selected_request`. Rename, trash and delete now act on whatever is selected — one `Rename` that
renames what you point at is what `f2` means in a tree, so they branch rather than gaining a
parallel `…Folder` action each. Four things it needed:

- **Separate core verbs, not a flag.** `rename` appends `EXTENSION` unconditionally, so on a
  directory it would produce `billing.json`; `remove` uses `remove_file` and `trash` refuses a
  non-file. `rename_folder`, `remove_folder` and `trash_folder` sit beside them.
- **The confirmation names the count.** A folder can hold work the panel never showed — an
  unreadable request is skipped by `scan` and has no row — so `request_count` answers "and how
  many requests" before anything is destroyed.
- **Buffers are retargeted by *prefix*.** `save_request` writes to a remembered path with no
  existence check, so a buffer holding `billing/x.json` after `billing` became `finance` would
  recreate the old folder on the next `Ctrl+S`. `retarget_prefix` rewrites the ones underneath a
  rename and clears them on a delete — the request rules, one level up.
- **Duplicate and Move to… are left out**, since both take a file. That is the rule the old
  guard was enforcing from the other side, kept rather than dropped.

> **The rename box was drawn only in the request arm**, so renaming a folder focused a handle
> whose element was never painted: the box appeared not to open, typing vanished, and `Enter`
> fell through to the panel's own binding. Invisible from state — `renaming_row()` reported it
> open — and found only because the test asserted the *typed text* rather than that a rename had
> started.

*Still absent:* duplicating or moving a folder, and nesting a new folder deeper than the
selection allows. `mv` still works.

---

## 6b. OpenAPI import — and the first real modal

The answer to a first run that feels empty. `ImportCurl` is one request per paste; a team with a
spec has its whole API in one document, and until this landed Zuno had no way to consume the
artifact that describes it. `Ctrl+Shift+I` takes a URL or a path.

**A hand-written walk over `serde_json::Value`, not a typed model.** `openapiv3` is small and
well-tested and was rejected on its own README: it covers 3.0.x and "does not cover OpenAPI v3.1
which was an incompatible change". Everything this reads — `servers`, `paths`, a method, a name,
parameters, a JSON request body — is *identical* across the two; the incompatibility is in schema
semantics, which is validation Zuno never performs. So the typed model would buy a dependency,
lock out every 3.1 spec, and still leave `$ref` resolution to be written by hand, which is the
only awkward part. `a_3_1_document_imports_the_same_as_a_3_0_one` is that argument as a test.

Decisions worth keeping:

- **Permissive like `curl.rs`.** An operation Zuno cannot read is skipped and *named* in
  `Import::skipped`, never fatal. A spec is written for many tools; refusing ninety requests over
  one unreadable body would break the feature where it is most useful.
- **A path template keeps its braces.** `{id}` is left literal rather than rewritten to Zuno's
  `{{id}}`, and that is the opposite of the obvious move: an unresolved `{{…}}` is *refused* at
  the send boundary, so the tidier version would import a collection where nothing can be sent
  until every path parameter is defined as a variable. A literal brace is a URL you can edit.
- **Only *required* parameters arrive enabled.** An optional one still imports — it documents
  what the endpoint takes — but sending every filter a spec mentions is not what anyone means by
  "import this API".
- **A body's shape is invented; a parameter's value is not.** A generated body is obviously a
  draft, and opening the editor on the right keys nested correctly beats an empty buffer. A
  pre-filled parameter row looks like a decision someone made, and a wrong value *sent* is worse
  than an empty one you have to fill.
- **`$ref` is local-only, and cycles end at a depth cap.** A remote `$ref` is an HTTP fetch in
  the middle of parsing — IO, a runtime, and failures a document cannot express. A cap rather
  than a visited-set because a `User` whose `manager` is a `User` is legal and common, and one
  rule covers that *and* the merely enormous.
- **A URL fetch goes through `Engine::send`.** A second HTTP client would be a second set of TLS,
  redirect and timeout decisions, silently different from every other request Zuno makes.
- **Everything lands under one folder named for the spec**, with each operation's tag as a folder
  inside it. Without the outer folder a hundred requests scatter through a collection someone had
  already organised; with it, an import is a thing you can find and a thing you can delete.
  `allocate` picks the filenames, so re-importing adds `-2` files rather than overwriting a
  request that has since been edited.

**And it is the first modal that is a form.** Rename got away with an inline box because a tree
row *is* a text field's worth of space; an import needs a field, a hint, and somewhere to report
what happened. `import_panel.rs` is deliberately **concrete, not a form framework** — one
consumer, one file, the bet `picker.rs` made and won by staying a `Vec<Item>` and a `Target`
instead of becoming a trait. When a second modal wants a text field — an environment editor,
opening a project — that is the moment to lift the shared part out.

Two details in it:

- **One field for both sources.** A URL/file toggle would be a mode to choose before typing, to
  describe a difference the text already carries: `http` at the front, or not.
- **A failure reports *in* the dialog and leaves it open**, with what you typed still in it. The
  fix for a wrong path or a non-spec URL is usually a character or two, and the status bar is
  cleared by the next thing that touches it.
- The `enter`/`escape` bindings are scoped to `ImportSource`, the *field's* leaf context — the
  panel's own `ImportPanel` context never holds focus, because the input does. Registered after
  their global twins, for the sixth time.

*Deliberately absent:* YAML, which most published specs use — the crate landscape is a graveyard
(`serde_yaml` is versioned `0.9.34+deprecated`, `serde_yml` is `0.0.13` and self-tagged the same),
and JSON-only is a real limitation recorded rather than hidden. Also absent: OpenAPI 2.0/Swagger,
a different document shape rather than an older version of this one, and refused with a message
that says so.

**What §6f changed here.** `Import` and `Imported` moved to `import.rs` when Postman arrived, and
`Imported::folder` became `folders: Vec<String>` — a tag is one level deep, a Postman folder tree
is not. `parse` also takes an already-read `serde_json::Value` now rather than bytes, because the
sniff has to read the document first and a megabyte export should not be parsed twice.

---

## 6c. The environment editor — and the merge that could not be saved

Resolution has existed since M3; authoring has not. `Ctrl+Alt+E` opens a modal listing every
environment beside the variables of the selected one.

**`Environment` is a merged view, and merged views cannot be written back.** `load` overlays the
`.local` sidecar onto the committed file and returns one map, which is exactly what `Resolver`
wants. It is also lossy in the one way that matters: a name may sit in *both* files — a placeholder
committed for whoever clones the repo, the real token only in the gitignored half — and the merged
map has nowhere to keep the pair. A save rebuilt from it would either drop the placeholder or push
the token into the committed file. `EnvironmentFile` holds the two halves and `Environment` is now
derived from it, so there is one merge rule rather than two that can disagree. Same shape as the
`preserved_body` lesson in §2: what the view cannot represent, the derivation destroys.

The rule has a second half that only a test found. Preserving a committed entry under a secret name
is right when the name was *already* secret, and wrong when it is being marked secret now — there
the committed value **is** the thing being hidden, and carrying it across copies the token into the
sidecar while leaving the original in the file that gets pushed. Invariant 10 broken by the fix for
invariant 10.

Decisions worth keeping:

- **`globals` is in the list**, pinned, unrenameable, undeletable. `scan` hides it because it
  cannot be *selected*; it can be edited, and leaving it out would move the text-editor problem
  rather than solve it. Its two verbs are absent rather than disabled — a control you cannot use
  teaches nothing.
- **Commit on the way out, with no discard**, the way the settings panel commits per row. An editor
  over files that can silently throw an edit away is a worse story than one that always lands, and
  a discard would import the dirty-buffer problem into a modal.
- **A save that changes nothing writes nothing.** Moving through the list saves on the way out of
  each entry, so an unconditional write created `globals.json` for anyone who merely opened the
  editor and pressed a key.
- **Reached from the switcher, not from a menu on the badge.** The badge stays a one-click switch;
  "Edit environments…" sits last in the picker, which is the surface you are already on when you
  want to change one. Cheaper than the workspace header's chevron menu and better placed.
- **Trash, not delete.** Sharper than the collection panel's reason: the `.local` half is
  gitignored, so it holds the only copy of every secret in it anywhere.
- **The badge has three states, not two.** It showed a bare globe with nothing selected and a
  bare word otherwise — two shapes for one control — and the switcher's "None" row said variables
  were left unresolved. That was simply false: `resolver` loads `globals.json` unconditionally, so
  the bottom layer applies whether or not an environment is chosen. It is one shape now, globe
  plus word, reading the environment's name, `globals`, or `none`. The flag behind the third state
  is **cached** rather than read in `render` — answering "is anything substituting" means opening
  and parsing a file, which invariant 3 forbids on the UI thread — and refreshed at the three
  points it can change: boot, a workspace swap, and the editor closing.
- **Values are shown, not masked.** `Environment::is_secret` says "masked on screen", and this is
  the one screen you opened deliberately to read them. Masking here would need a per-row reveal and
  a non-editable variant of `TextInput`; the switcher, which is the surface someone else might see
  over your shoulder, still shows counts and never values.

---

## 6d. Request chaining — the rule lives on the producer

`$.access_token → token`, recorded on the request that returns it and published into the selected
environment after a successful send. Consumers need nothing: `{{token}}` is an ordinary variable.

**The fork, and why this side of it.** The alternative is "run X before me" — a rule on the
*consumer*. Rejected on four counts, of which the first is principle 3: producer-side rides
entirely on machinery that exists (`Resolver` substitutes, `environment::save` writes, the
committed/gitignored split decides the file), while consumer-side needs a runner with ordering,
a dependency graph and a story for a failed prerequisite — none of which is chaining. It also
authors where the data is, since the path comes from the outline you are looking at; keeps the
token in a file you can read rather than in invisible state; and composes with no graph, because
two producers writing one variable is a last-write-wins you can see on disk. The cost is real and
accepted: an expired token means re-sending the producer by hand.

**Three refusals, each of which would otherwise be a silent wrong answer:**

- **Only on a 2xx.** An error body has fields too, and publishing one into `{{token}}` produces a
  chain that fails on the *next* request — the hardest kind to read back to its cause.
- **Only on the live run.** `index_body` also runs when you browse the history, so without the
  guard, *looking at* a response from three sends ago rewrites the environment with its expired
  token.
- **Only into a selected environment, never globals.** A captured token is environment-specific by
  nature — dev's and prod's are different values — so putting one in the always-active layer means
  switching environment does not switch the token. This also keeps §6c's `globals_active` cache
  honest, since nothing but the editor can write globals.

Decisions worth keeping:

- **`captures` is a `RequestSpec` field with a per-field `#[serde(default)]`.** Not a violation of
  the rule above `RequestSettings`: what `RequestSpec` refuses is the *container* default, so a
  corrupt file is still rejected rather than becoming an empty request. No session bump either —
  invariant 8 governs `Session`'s own fields.
- **`extract` descends, it does not scan.** The obvious build asks `path_to` of every row until one
  matches, which is O(n²) because `path_to` itself walks back to the document start for ancestors.
  Fine on a token response, unusable on a 50MB one — and the difference only appears on the bodies
  nobody tests with.
- **Segment matching is gated on the container's kind.** Without it `$.data[0]` resolves against an
  *object* by matching its first key, so a path with the wrong bracket captures a real value
  instead of reporting a miss.
- **`every_path_the_ui_can_copy_is_a_path_extract_can_follow`** pins `path_to` to `extract`. The
  writer is what `Alt+C` copies and what the capture editor is filled from, so a path the writer
  emits and the reader cannot follow is a chain that silently captures nothing.
- **Capturing a row publishes immediately**, against the response already on screen. It shipped
  deferring to the next send, which made the one path whose whole argument is "author it where the
  data is" the one path that looked at the data and declined to read it — and did so *silently*,
  under a menu row reading "Capture as variable" rather than "capture on next send". The test that
  covered it asserted the rule, the suggested name, the secret flag and the revealed tab, and
  never the file, so it passed against the gap for a slice. `CaptureTrigger` is what the fix
  turns on: after a send a refusal is not worth saying, and when someone asked for one every
  refusal has to name itself. The environment is passed in rather than reused from
  `capture_target`, or sending, switching, then capturing a row would publish into the environment
  you had just left.

  One cost, accepted: renaming a capture after it has published leaves the value behind under the
  old name. Visible in the editor, and the same shape as renaming any variable.
- **A fourth request-pane tab**, not a strip that appears only when a request has captures. A rule
  you cannot see is what the consumer-side design was rejected for; hiding this one until it exists
  would reintroduce the same complaint one level down.

---

## 6e. The collection runner — assertions first

Building in five slices; this section grows with them. **Slice 1: what a request expects.**

An assertion is `capture::extract`'s comparison half. Both address a value with a JSONPath in
`path_to`'s notation and both read it through the *same* function, so a path copied out of a
response with `Alt+C` works in either and cannot drift between them.

- **The status is not an assertion.** Every request wants to check it, so a design where it
  competes for a row in the table puts a row saying the obvious on every request in the
  collection. `expect_status: Option<u16>` is its own field; the table is for the body.
- **Three operators — `exists`, `equals`, `contains` — and no `<` or `>`.** `extract` returns
  text, so a numeric comparison needs a parse *and* a decision about whether `1.0` equals `1`.
  That is a real semantic to commit to for a case nobody has asked for; adding one later is
  additive, guessing now is permanent.
- **Values compare unquoted**, the way `extract` returns them: assert `ok`, not `"ok"`. Anything
  else would mean quoting by hand a value you can read on screen.
- **A body assertion against a non-JSON response fails.** The tempting alternative — nothing to
  check, so nothing failed — turns an endpoint that started returning HTML into a green run.
- **`check_all` skips disabled and half-typed rows itself** rather than leaving it to callers.
  The equivalent filter in the capture runner is written at its one call site, and a second
  caller forgetting it would fail a run on a row you were still typing.

`RequestSpec` carries both fields with a per-field `#[serde(default)]`, and `RequestView` holds
rows for them **before the Assert tab exists** — `spec` derives from the inputs, so a field the
view does not hold is destroyed on save, and a hand-written rule would be lost by opening the
request. `assertions_survive_a_load_and_save_before_any_ui_can_edit_them` is that assertion,
written against a file because nothing can author one yet.

**Slice 2: the loop, and it is entirely in `zuno-core`.** `Engine::send` hands back an
`async_channel::Receiver`, which has `recv_blocking` — so the runner is an ordinary synchronous
loop with no async runtime and no GPUI. The app drives it on a background executor; `zuno run
./collection` would drive it on its main thread, which is the reuse the crate split was for.

- **The step list is the only input.** A folder expands through `collection::scan`, already sorted
  by relative path; a flow names its steps explicitly. Neither the loop nor the report knows which
  produced it, so the two producers cost one loop between them.
- **The resolver is rebuilt per step and must never be hoisted.** The previous step's captures
  were written to the environment *on disk*, and this step resolves against them. Hoisting it out
  of the loop is the single change that makes a login-then-use flow silently send an empty token,
  which is why `a_captured_value_reaches_the_next_step_on_the_wire` asserts the bytes the server
  received rather than anything inside the process.
- **A failed step publishes nothing**, for the reason a failed send does not in the app: an error
  body has fields too, and one in `{{token}}` leaves every later step failing for a cause nothing
  points at.
- **Failures do not stop the run**, cancellation does. A run exists to say everything that is
  wrong in one pass.
- **The body is parsed, not sniffed.** A JSON body under `text/plain` is common enough that
  asserting on it should work, and a non-JSON body fails at its first byte — so trying costs
  nothing and refusing on a header costs a real case.

**`capture::publish` moved into core in this slice, and moving it found a bug.** The app's capture
path removed the committed entry for *every* secret name; the rule is that only a name which was
not already secret loses it, because for one that was, the committed value is a separate
placeholder. Written twice, invariant 10 was right in the environment editor and quietly wrong in
the capture path — and the editor's test could not see it, since it covers the other writer. Three
unit tests hold `publish` itself now.

The same lift closed a second gap: a capture writing the *first* secret into an environment now
arms `ensure_gitignored`, which previously only fired on an environment switch.

**Slice 3: the Assert tab**, a fifth beside Capture and built the same way — a table of rules, the
same authoring gesture from a response row (`Alt+Shift+A`, or "Assert on this" in its menu), and
`path_to` as the source of the path so a rule cannot quietly check one that never matches.

- **The expected status sits in the header, not the table**, for §6e's reason. It is a text box
  rather than a stepper because you type `404` in three keystrokes, and a control that rejects `4`
  on the way to `404` is one nobody can type in. **Parsed by `spec`, never mirrored** — an
  unparseable box means "this request states no expectation", the same tolerance the URL gets.
- **Three operators cycle rather than opening a picker.** A choice of three does not need a modal,
  and the cell is its own mouse path.
- **`exists` hides the value cell.** An empty box beside an operator that ignores it invites
  typing into something discarded.

**Building this found a bug in `is_dirty`, and the fix is structural.** It is a hand-written
mirror of `spec` — deliberately, because the tab strip asks it per frame and `spec` clones the
body — and `captures` was simply never added to it, so editing a capture left the tab clean and
`Ctrl+W` closed without asking. It now **destructures `RequestSpec` with no `..`**, so a new field
fails to compile until someone decides whether changing it makes a buffer dirty. Same discipline
as `RequestView::load`'s exhaustive `Body` match, applied to the other end of the same problem.

**Slice 4: running a folder.** `Ctrl+R` runs the folder the collection panel's selection sits in
— a directory row runs itself, a request row runs the folder holding it, nothing selected runs the
whole collection — and the report's title says which, so "what did that just do" is answered on
screen rather than guessed.

- **A panel, not a picker.** The picker is a chooser that closes when you pick. This is a report
  you read, whose most important content is the *second* line of a failed row, and whose rows
  happen also to open. Reusing the picker would have meant bending a one-line-per-row list around
  a variable-height one.
- **Rows arrive while the run is going.** `runner::run_with_progress` exists for exactly this:
  forty requests showing nothing until they finish is indistinguishable from a hang. The outcomes
  cross back on a channel, because `run` blocks and invariant 3 keeps it off the UI thread.
- **`escape` is two-stage** — it stops a run in flight and closes a finished report — because it
  means "back out of this" at whichever stage you are at.
- **A cancelled report says "stopped early".** The counts alone read as a complete result, and
  acting on "0 failed" when half the steps never ran is the worst thing this panel could cause.

**Two bugs found by writing the tests, both in cancellation.**

`Report::cancelled` was set only when the *loop* broke on the flag, so stopping during the last
step left nothing to break out of and the report claimed it had finished. It reads the flag
directly now — asserted with **zero steps**, the one shape where the loop cannot break and so only
the flag can answer.

Worse: `run_step` **blocked** on the event stream, so the cancel flag was only ever read between
events — and a request that sends `Started` and then hangs produces none. The stop button did
nothing on precisely the request you would most want to stop, under a comment claiming the
opposite. It polls every 10ms now; reverting that makes `a_request_that_hangs_can_still_be_stopped`
take **15 seconds instead of under one**, which is the assertion.

That test lives in core rather than the app suite for a reason worth recording: **`run_until_parked`
waits for background tasks, so no mid-run state is observable from the headless platform at all.**
An app-level test of the two-stage `escape` was written, could not see the running state, and was
deleted rather than weakened into one that passes for the wrong reason.

**Slice 5: flows — the ordered producer.** A collection is organised by *resource* and a workflow
runs *across* it: log in, create a user, read it, delete it. Those are two structures and one
cannot encode the other, which is why filename order — right for a folder smoke test — is wrong
here. `flows/` is a reserved directory holding a name and an ordered list of collection-relative
paths, and `collection::scan` skips it by name for the reason `environments/` needed first.

- **`runner::run` never learns which producer it got.** A folder expands through `scan`; a flow
  resolves its step list. One loop, one report shape, one set of tests.
- **A step whose file is gone is a *failure*, not a skip.** A flow quietly running three of its
  four steps and reporting "3 passed, 0 failed" is the most dangerous shape a green run can have,
  so `Step::spec` is an `Option` and the missing case gets its own line in the report.
- **A step may repeat** — logging in again around a teardown is ordinary — which a list gives for
  free and a set would not.
- **The selection follows the step when you move it.** Moving something up three places is three
  presses of the same key, and a cursor that stayed put would move a different step each time.
- **The name is the filename**, not a field in it, so a `mv` cannot leave a stale one behind.
- Flows stay **out of the collection tree**, like environments: the tree is the files in your
  collection, and a picker is what holds the rest.
- **A folder's row menu leads with running it**, named with its count for the delete prompt's
  reason. `Ctrl+R` was the only path for a slice — a verb wired up and never offered.

**And a keybinding clash that nothing was watching for.** `RunFlow` was given `ctrl-shift-r`,
which `FocusResponse` has held since M1. That does not fail to compile and does not fail loudly:
`binding_enabled` scores both context-less bindings at maximum depth, the tiebreak is registration
order, and the later one silently wins. What noticed was two *unrelated* response tests going red
— luck, not coverage, and the sixth time this ordering has decided behaviour here.

`register_keymap` now builds its list through `bindings()` so a test can read it, and
`no_two_global_bindings_claim_the_same_keystroke` fails with the offending pair named. Scoped
bindings are deliberately excluded: sharing a keystroke across contexts is what contexts are
*for*, and `ctrl-f` meaning the body in the editor and the response elsewhere is a design
decision, not a collision.

---

## 6f. Postman import — the friction that decides who uses Zuno

Every other item in this document improves the app for someone already inside it. This one
decides who gets inside. It was built because friends of the author agreed to migrate and then
said the migration itself was the obstacle — nobody retypes eighty requests by hand.

**One result shape, two parsers, one writer.** `core/src/import.rs` owns `Import`, `Imported` and
`Variable`; `openapi.rs` and `postman.rs` are parsers that answer with them, and the half that
creates directories, allocates free filenames, writes an environment and reports what was dropped
is written once in `Workspace::finish_import`. A third format is a parser plus one sniff arm.

**The format is sniffed, never chosen.** `import::parse` reads the document and decides:
`openapi` present → OpenAPI, `item` present → Postman collection, `values` plus a Postman marker
→ Postman environment, and then arms for every shape we can *recognise but not read*. A second `Import from Postman` verb would make someone classify their
own export before they could use it — friction of exactly the shape this feature removes. The
cost is paid in refusals instead, and that is the better trade: "this is a Postman v1 collection —
re-export it as v2.1" is a next step, and "unrecognised document" is a dead end. Swagger 2.0, v1
collections, and Postman *environment* exports each get their own sentence.

The action was `ImportOpenApi` and is now `ImportDocument`, because an action named for one format
that reads two is the stale-confident-name failure CLAUDE.md's Lessons section is mostly about.

**The Postman API wraps what the Postman app exports.** A share link answers
`{"collection": {…}}`; a file exported from the app is the bare object. The envelope is unwrapped
in the sniff rather than in `postman.rs`, because it is a property of the *transport* and not a
version of the collection format. This shipped reading only the export, so pasting a share link —
the path needing no export step at all, and therefore the one reached for first — refused a
perfectly good collection. Found by pasting a real one.

### What maps, and the calls made

**Postman's variable syntax is already Zuno's.** `{{baseUrl}}` needs no rewriting in a URL, a
header, a body or an auth token. It is the single largest reason this import is faithful rather
than approximate, and it is luck rather than design.

- **The item tree becomes directories**, nested as deep as it goes. `collection::MAX_DEPTH` is now
  `pub` because the importer has to respect it: anything deeper is **flattened** into the deepest
  folder that fits and named in `skipped`. A request written below the depth `scan` walks is on
  disk and invisible — the tree cannot show it and the picker cannot find it — which is worse
  than a folder in the wrong place.
- **Auth is lowered into a header.** Zuno has no auth model on purpose (ROADMAP records auth
  helpers as *dropped, not deferred*), and a header is what actually goes on the wire, so bearer,
  basic and API-key all become one. `basic` shares `curl.rs`'s `base64`, which had been sitting
  there tested with one caller. OAuth 2, SigV4, Digest, NTLM and Hawk are signing *procedures*
  with no value to copy, so they are named in `skipped` rather than half-imported — a request
  that looks complete and 401s is the worse outcome.
- **Auth inheritance runs collection → folder → request**, and `{"type":"inherit"}` keeps the
  parent's rather than reading as a type of its own. A request's own auth lives inside `request`,
  **not** on the item — only a folder's sits on the item. Reading it from the item gave every
  request in a folder the collection's credentials no matter what it declared, and the test that
  caught it was written before the code was.
- **The query is split off the URL.** `build.rs` merges enabled query rows into whatever the URL
  text already carries, so leaving `?limit=10` in both places sends it twice. The `query` array
  is preferred over the text when both exist, because Postman keeps *disabled* rows only there —
  and a bare string URL is split the same way, so an imported request presents identically
  whichever form the export used.
- **A path variable with a value is substituted.** `/users/:id` with `id = 7` imports as
  `/users/7`. A literal `:id` sends and 404s, which shows you what to fix; `{{id}}` would refuse
  to send at all. That is §6b's rule about server variables pointing the other way, for the same
  reason — the wall is what to avoid.
- **A GraphQL body imports as the JSON it would have been sent as.** GraphQL over HTTP *is* a
  JSON body, so `{"query":…,"variables":…}` is the faithful import and Zuno needs no GraphQL
  model, no second body type, and the query stays editable as the text it already was. Postman
  stores `variables` as a *string* of JSON; sending that verbatim would put a quoted string where
  the server expects an object.
- **A disabled row imports muted, not missing.** Importing it enabled sends a header someone
  switched off; dropping it loses the fact that they had it. Postman stores the negative
  (`disabled: true`), which is one inversion to get wrong per row type.
- **A `raw` body with no `options` is sniffed, not defaulted.** Postman's documented default
  language is `text`, but an export carrying no `options` at all is usually an older one whose
  bodies are JSON regardless, so the text decides.

### Variables become an environment, and it is selected

Collection-level `variable[]` goes to `environment::merge_imported`, named for the collection
through the same `collection::slug` as its folder so the two agree on screen.

**Only names the environment does not already have are written.** Re-importing has to bring
across a variable the collection gained and must not undo a `baseUrl` someone pointed at staging
— and there is no way to tell an edit from an original, so the existing value wins and the count
is reported. Allocating `billing-2` the way request *files* do would be worse here: a second
environment holding the same names is a thing to pick between rather than a thing to use.

Postman marks its own secrets (`type: "secret"`), which lands exactly on invariant 10's file
split, so the marking survives the crossing instead of being guessed from the name.

And the environment is **selected**, overriding whatever was active. An export whose every URL
begins `{{baseUrl}}` otherwise imports as a folder of requests that cannot be sent, and asking
someone to find the switcher first is the friction this feature exists to remove. The status line
names the switch, because a *silent* switch is the failure mode rather than the switch itself.

### Environment exports — and the one exact mapping in the feature

A Postman *environment* export is a different document and a different **outcome**, so
`import::parse` answers with a `Parsed` enum rather than an `Import` carrying no requests. The
enum is not ceremony: without it, a caller reports "no requests to import" about a perfectly good
export, which is the shape of half the bugs in this file.

- **`type: "secret"` goes to the gitignored half.** Postman marks its own secrets, so invariant
  10's file split survives the crossing instead of being guessed from a name — `token` and
  `apiKey` are heuristics, and the export is a fact.
- **A globals export lands on `environment::GLOBALS`.** Postman globals are the layer every
  environment resolves over, which is exactly what Zuno's are: the one place in this whole feature
  where the two models agree completely rather than approximately. Its own `name` is a workspace
  label and is dropped, because that layer is not a name anyone picks — `valid_name` refuses it on
  purpose, so `Target::Globals` is the deliberate way in.
- **"Switched off" is spelled oppositely by the two formats.** A collection variable carries
  `disabled: true`; an environment value carries `enabled: false`. Which one a given Postman
  version writes is not worth betting on, so either counts. Getting the polarity wrong is silent
  and exactly backwards — every dormant variable arrives live and every live one dormant — so both
  spellings are pinned by a test, in both directions.
- **The `.gitignore` rule is written from what was imported**, not through `protect_secrets`.
  That reads the *selected* environment, and nothing is selected yet at that point — a globals
  import never selects anything at all. Taking the convenient route leaves a `.local.json` full of
  tokens sitting there committable, which is invariant 10 defeated by a call order. Break-tested.
- **A named export is selected and a globals one says it needn't be.** "Globals are always active"
  in the status line, so nobody goes hunting the switcher for an environment that isn't there.
  The badge reads a cached `globals_active`, so an import that fills globals refreshes it.

An export with nothing live in it is refused rather than creating an empty environment and
reporting success.

### Scripts — recovered where the shape is exact, reported everywhere else

`event` blocks are JavaScript and no importer will ever run them. But the three shapes that make
up most real ones map onto what §6d and §6e built — `pm.environment.set` onto a `Capture`, a
status check onto `expect_status`, `pm.expect(…).to.eql` onto an `Assertion` — so
`postman/script.rs` is a pattern matcher over a closed set of forms. `postman.rs` became a
directory for it, the way `json/` and `engine/` are: walking JSON and matching JavaScript are
different jobs.

**The governing rule is that a wrong recovery is far worse than no recovery.** A rule nobody
wrote makes a run fail — or worse, pass — for a reason that is nowhere in the collection, and the
person has no way to know Zuno invented it. So every form is matched whole or not at all, and
anything unmatched is reported verbatim. Concretely refused, each with a test asserting the
refusal:

| Real line | Why there is no faithful translation |
|---|---|
`pm.expect(d.items.length).to.be.above(0)` | `Op` is Exists/Equals/Contains on purpose — §6e's note on why there is no `<` |
`pm.expect(pm.response.responseTime).to.be.below(500)` | not the response body |
`pm.expect(d.count).to.be.ok` | truthiness fails on `0`, `""`, `false`; `Exists` passes on all three |
`pm.expect(d.items[i].id)` | a computed index has no single answer |
`pm.environment.set("n", d.length)` | `.length` is a JavaScript property, not a member of the body |
`status("Created")` | `expect_status` holds a number, and a name-to-code table is a table of guesses |

**Three calls worth recording, because each had a cheaper wrong answer.**

- **A local variable bound to the body is followed.** The common real capture is two lines —
  `var jsonData = pm.response.json();` then `pm.environment.set("t", jsonData.token)` — so the
  path lives on a variable. One pass collecting those names is the difference between recovering
  a fraction of real captures and most of them, and it is bounded: a name is either bound to the
  parsed body or it is not a path this can follow. `JSON.parse(responseBody)` is recognised too,
  since older collections are full of it.
- **A guarded statement is refused, not recovered without its guard.**
  `if (pm.response.code === 200) { pm.environment.set("token", d.token) }` is extremely common
  and Zuno has no condition on a capture. Often the guard is *redundant* here — a capture that
  matches nothing writes nothing and is reported — but "often" is not a basis for inventing
  rules, and the conservative call is the reversible one: the block is reported, so re-adding it
  is a click. This was found by a break-test, not by design: the first version recovered the
  one-line form silently, which is exactly the failure the module's own doc comment forbids.

  The guard is tracked as **one flag per open brace**, not as a depth counter, because a
  `pm.test(…, function () {` wrapper opens a block too and has to be transparent. Counting it as
  a guard refuses the check inside every well-written script there is — which the first attempt
  did.
- **`.length` is refused, and it is the trap in this whole module**, because it reads exactly like
  a key. `pm.environment.set("user_count", response.length)` is an ordinary line, and it shipped
  translating to `$.length` — a capture that matches nothing on an array, or captures the wrong
  value on an object that happens to carry a `length` member. A body genuinely keyed `length`
  loses its capture and is reported, which is the right side of that trade. **Found by running a
  real collection through, not by reading the code** — the third time on this feature that a real
  document beat a synthetic fixture, after the API envelope and the request-level `auth`.
- **Only the request's own `test` scripts are recovered.** A collection- or folder-level script
  runs after everything beneath it, so copying it into all forty requests would be the faithful
  reading of Postman — and the wrong call here, because Zuno has no inheritance to represent it:
  forty requests would each declare a rule none of them wrote, with nothing on screen saying
  where it came from. A lossy copy that looks authoritative is worse than a note. `prerequest`
  scripts are not mined either: they run *before* a response exists, so
  `pm.environment.set("ts", Date.now())` is dynamic state and not a capture.

Comment stripping is quote-aware, because `pm.expect(d.url).to.eql("https://a.test")` puts a
`//` inside a string in the most ordinary way there is, and truncating there turns a good match
into an unread line. Same for brace counting — `to.eql("{}")` would otherwise open a block that
never closes and refuse the rest of the script.

The import reports how many rules it recovered, ahead of the skipped count: "12 skipped" alone
reads as "the scripts were lost" when most of what mattered in them is now on the requests.
Descriptions still have no `RequestSpec` field and are counted and reported once.

---

## 6g. Body prettify — a formatter that copies bytes

Zuno could *display* pretty JSON and not *produce* it: the response viewer renders a formatted
outline, and nothing turned JSON bytes back into formatted text. `Alt+Shift+F` formats the request
body, `Alt+Shift+M` minifies it.

**`serde_json::to_string_pretty` was the obvious answer and is wrong — measured, not assumed.**
`serde_json = "1"` carries no `preserve_order`, so `Value`'s objects are a `BTreeMap`:

```
in : {"zebra":1,"apple":2,"big":12345678901234567890,"exact":1.0}
out: {"apple":2,"big":12345678901234567890,"exact":1.0,"zebra":1}
```

Silently reordering a request body is not formatting it. Key order is often deliberate, and a
canonicalising signature scheme makes it load-bearing. (Numbers and escapes survive fine; ordering
alone disqualifies it.) Enabling `preserve_order` would fix that symptom and change `Value`
behaviour crate-wide, including the order `openapi.rs` walks `paths`.

**So `json/format.rs` walks the outline `flatten` already builds and copies each token from its
`Span`.** Only the whitespace *between* tokens is this module's decision — key order, number text
and escape sequences survive because nothing in it is in a position to change them. Three things
come free from building on `flatten` rather than beside it:

- **Invalid JSON is refused by the parse**, so there is no path that half-formats a broken
  document. The error carries a byte offset, which `json::line_col` turns into the line and column
  that make a syntax error in someone else's body actionable.
- **The parse is already a background-executor job** for the response viewer, so invariant 3 needed
  no new arrangement.
- **A scalar at the root works** — `"hello"`, `42`, `{}`, `[]` — because `flatten` emits a single
  row for those and the walk had to handle a document that is not a container.

Two details that read as arbitrary and are not: an empty container goes on one line (`{\n}` is what
a naive walk emits and it reads as a mistake), and there is **no trailing newline**, because this
lands in an editor buffer where a blank last line is something the person then has to delete.

**The rewrite goes through `Editor::replace_range`, so `Ctrl+Z` undoes it.** That is what makes
reformatting someone's body a safe verb rather than one needing a confirmation, and the app test
asserts the undo rather than only the format — if the rewrite ever stopped using the ordinary edit
path, formatting would become destructive and nothing else would say so.

**Gated on the body *kind*, not on whether the text happens to parse.** The chip on screen says
JSON or XML, and a verb that quietly works on a body labelled XML — or refuses one labelled Text
that holds JSON — is a verb whose behaviour you cannot read off the screen. "This body is XML"
points at what to change. The `Format` label in the body header appears only where the verb
applies, rather than greyed out: a present-but-dead control teaches nothing.

### Copy stays raw, and that was my mistake to check

The slice was planned with a second consumer — making `Ctrl+Shift+C` copy the formatted outline
instead of the raw bytes, on the strength of ROADMAP's line *"Copy gives the raw bytes, not the
pretty-printed outline on screen."* That line sits under **"Three decisions worth keeping"**, with
its reason stated (what you paste into a fixture or a bug report has to be what came back) and a
test enforcing it — `ctrl_shift_c_copies_the_response_body_verbatim`. It was read as a gap. It is a
decision, and a better-argued one than the change: reformatting the thing you are reporting
quietly changes it.

Reverted. The two failing tests are what caught it, which is the value of asserting a decision
rather than only a behaviour. **CLAUDE.md's heuristic held exactly** — when two readings disagree,
trust the one that names its rejected alternative.

XML and HTML prettify stay out, on the same argument as their highlighting.

---

## 6h. The timing timeline — where the engine was the missing half

Every other feature in this document had the engine ahead of the views: §11 exists to track
capability that was built and unreachable. **This one is the first inversion.** A timeline is an
axis, a few segments and some arithmetic; what was missing was anything to draw. `Timing` carried `dns`,
`connect` and `tls` as `Option<Duration>` from M1.2 and `run.rs` hardcoded all three to `None`
for four milestones, under a comment saying reqwest could not provide them.

That comment was half wrong, and the half that was wrong is the interesting part — see §3.2.
Two hooks on `ClientBuilder` are enough, and neither is the custom connector it named:
`dns_resolver` takes a `reqwest::dns::Resolve`, and `connector_layer` takes a tower `Layer`
around the connector service where one `call` is one connection. What actually does need a
hand-built connector is separating the TCP connect from the TLS handshake, which is why there is
no `tls` field any more rather than an empty one.

### Three states, not three `Option`s

The old shape could not distinguish **"this stage did not happen"** from **"this stage cannot be
measured"** from **"nobody looked"**, and all three rendered identically as a blank. Two comments
in the tree gave *different* reasons for the same `None` — `response.rs` said a reused connection
skips them, `run.rs` said reqwest does not expose them — and each was true of a situation the type
could not tell apart from the other. That is the CLAUDE.md failure mode where a confident comment
stops being checked, arrived at from both ends at once.

It matters because of what a chart asserts. A zero-width DNS bar says the lookup took no
measurable time; on a pooled connection the truth is that no lookup ran. So `Connection` is an
enum — `Opened { dns, connect, sockets }`, `Pooled`, `Unknown` — and `Pooled` is the *common*
case, not an edge one: clients are cached per `ClientKey` precisely so a resend reuses its
socket (§10, M1.2). The state the timeline shows most often is the one the old type could not
express.

`Unknown` is reachable only through `Timing::default()`, and it is kept rather than collapsed
into `Pooled` for the reason `SizeInfo::declared` is an `Option`: "not measured" and "measured as
nothing" are different claims, and the pane says which.

### Four phases, computed in core

`Timing::phases()` returns `Vec<Phase>` with each bar's offset already accumulated, contiguous,
summing to exactly `total`. **Deliberately not in the pane.** A chart computing its own offsets is
one that can disagree with the number printed above it, and this is arithmetic a unit test can
hold — where a paint is not observable at all. Stages that did not happen are *absent* rather
than zero-length, so a pooled connection yields two phases and not four with two empty.

`Wait` covers everything between the connection being ready and the first response byte: our own
request build, the upload, and the server's thinking. Splitting a "request sent" bar out of it
would need a measurement inside the upload that nothing takes, so the phase is named for what it
actually contains instead of being divided on a guess. A clamp against `ttfb` should never bind —
`ttfb` is wall time around the whole send while the connection spans are nested inside it — and
exists so that a bookkeeping error presents as a wrong number rather than as bars running past
the end of their own track. `phases_stay_inside_the_total_when_the_setup_spans_overlap` pins it
with a deliberately impossible `Timing`.

### Attribution, which is the only hard part

The client is shared by every job, so the resolver and the layer are shared too and a measurement
has to find its way back to one request. A **tokio task-local** does it: `run::execute` installs a
fresh `Probe` for the job's task and both hooks write into whatever probe their task is under.

That works because of *where* hyper polls the connector, and this is the fact the whole design
rests on: `hyper_util`'s legacy client does `future::select(checkout, connect).await` inside
`connection_for`, so **both halves are polled by the caller's task** rather than a spawned one.
Read out of `hyper-util-0.1.20/src/client/legacy/client.rs` rather than assumed — "verify, don't
remember" is not only about gpui. If a later version spawns the connect instead, `try_with` starts
failing and every connection silently reads as `Pooled`, which is what
`a_second_request_reuses_its_socket_and_says_so` fails on.

Two races, and both resolve the right way:

- **The pool can win that `select`.** A half-built connection is then spawned to finish in the
  background — outside the task-local, so nothing is recorded, which is correct because the
  request did travel on a pooled socket. It does mean a lookup can be recorded for a connection
  that was abandoned, so `Probe::connection` keys entirely off whether a *socket* completed and
  ignores a stray DNS measurement.
- **Redirects can open several.** Both counters sum rather than keeping the first, because the
  elapsed time really did contain all of them and attributing only the first would move the rest
  into `Wait`, where it reads as a slow server. reqwest surfaces only the final response, so
  `sockets` is carried out to the pane: it is the only way the extra round trips are visible at
  all, and the summary line says "3 connections opened — redirects were followed" rather than
  leaving the number unexplained.

The layer's span *contains* the lookup, since the connector calls the resolver itself, so
`connect` is a subtraction. Done once in `Probe::connection` rather than in the pane, for the
reason the phases are: two places doing it is two places to disagree.

### The axis, and the redesign that produced it

**The first version shipped and was rejected on sight**, which is worth recording because the
fault was structural rather than cosmetic: it drew four bars, each on its own grey track, with
**no axis at all** — no ticks, no elapsed labels, no reference of any kind. So there was no way to
see where 50 ms fell. It was a proportion chart wearing a timeline's name, and the four stacked
track boxes were doing the work one hairline should do.

Found the way §5's layout bugs and the picker's dead rows were found: by opening the window. No
test could have caught it — every assertion was about arithmetic, and the arithmetic was right.
Worth pairing with §2's note that nothing headless observes a paint: what that means in practice
is that *composition* is unfalsifiable here, so it has to be looked at, and looked at early.

The rebuild is one axis with the phases as contiguous segments on a single line, plus a marker at
first byte — the one landmark a single request has. **The cascade was dropped because its offsets
carry no information.** The phases are strictly sequential and contiguous, so each segment's start
is determined by the previous one's end; four rows spend four times the height restating what one
row already says. A browser's waterfall earns its rows because it shows *many* requests, which can
genuinely overlap. One request cannot overlap itself.

`core::axis_ticks` decides the scale, in core beside `phases` and for the same reason — a pure
function a unit test can hold. Two decisions in it:

- **Round the leading digit up to 1, 2 or 5 × a power of ten.** Rounding *down* looks equally
  reasonable and yields twice the ticks asked for: a 142 ms span wants a step near 28 ms, and
  rounding that down to 20 gives seven labels on an axis sized for four. Break-tested — both
  count assertions fail on the down-rounding version.
- **Drop a tick that crowds the end.** The total is drawn separately, in the summary line, so a
  tick at 95% stacks two numbers and leaves the edge label nowhere to go. `no_tick_crowds_the_
  end_of_the_axis` holds it, and **the first version of that test was vacuous** — with every span
  in its list the step was large enough that the tick past the limit also landed past the total,
  so the loop stopped on its own and the assertion passed whether or not the limit existed. A
  105 ms span is the one input the limit actually decides, and it was added after break-testing
  found the gap. Seventh of these.

**Two label placements are decided by gpui rather than by taste**, and both are the same
constraint: 0.2.2 has no transform, so an element cannot be centred on a position without knowing
its own width — and reading a width at render time is a frame behind (§6's decoration note).

- **Tick labels are left-aligned at their tick**, ruler-style, which needs no measurement. This is
  also *why* `axis_ticks` excludes the total: with left alignment a label at the far right would
  run off the pane, and dropping it is better than clipping it.
- **The `first byte` caption is pinned by its right edge** and sits *before* the marker, because
  TTFB is late in the ordinary case — the download is usually the short phase — so pinning the
  left edge would push the caption off the pane on nearly every response. It flips to left-pinned
  below the midpoint, for the unusual early-first-byte case. A caption clipped at the pane edge is
  the dead-control shape this codebase keeps finding, one step down.

Two smaller notes. The segments are square-cornered and 6px, against the first version's rounded
10px, which read as decoration next to every other surface in the app. And a segment narrower than
`SEGMENT_MIN_WIDTH` is drawn at that width — **a deliberate small lie**, the same one browsers
tell: a 2.4 ms lookup inside 142 ms is 1.7% of the track and rounds to nothing, so the phase would
have a label, a duration and no mark, which reads as a rendering fault rather than as "that was
quick". The legend always carries the true number.

The legend's last row keeps its border *width* in the pane's own colour rather than dropping the
border, or the rows would differ in height by a pixel — `view_tab`'s idiom, and the reason the
codebase reaches for a conditional colour where a `FluentBuilder::when` would also have worked.

### What is asserted, and what a person has to look at

The mechanisms are separable and each was broken on purpose:

| Reverted | Fails |
|---|---|
`record_socket` never called | both socket tests, and the DNS one — everything reads as `Pooled` |
`record_dns` never called | `resolving_a_hostname_is_reported_as_a_dns_phase`, and nothing else |
`connection()` never returns `Pooled` | `a_second_request_reuses_its_socket_and_says_so`, at the socket level |
the two `ClientBuilder` hooks removed | `a_send_through_the_app_reports_a_measured_connection` |
every tab dispatches the cycle | `clicking_the_headers_tab_switches_the_response_view` |
`axis_ticks` rounds the step **down** | `axis_ticks_round_up_to_nice_numbers` and the tick-count range test |
`CROWD_LIMIT` raised to `1.0` | `no_tick_crowds_the_end_of_the_axis` — but only after 105 ms joined its inputs |

`a_second_request_reuses_its_socket_and_says_so` is the one worth reading. Its server accepts
**once** and serves **twice**, deliberately without `Connection: close` — the inverse of
`serve_twice_setting_a_cookie`, whose close header is what gives each request its own socket. It
returns how many requests it served, and that number is the load-bearing half: only one `accept`
happened, so "served 2" is independent proof both requests travelled on one socket. Reading our
own probe back would only prove the probe agrees with itself.

The DNS test binds its listener to `localhost` by *name* rather than to `127.0.0.1`, so whatever
that resolves to on this machine is what is being listened on and the test does not depend on
whether `::1` or `127.0.0.1` comes back first. Still no network — this is `getaddrinfo` against
`/etc/hosts`. It asserts `is_some()` and never `> 0`, because a warm lookup genuinely lands inside
a nanosecond and a test demanding a positive duration would fail for being fast.

**The segment colours are the part no test should hold.** A `TimelineTheme` groups the four, beside
`SyntaxTheme` and for its reason — one module reads them and what matters is that they work
together. Deliberately *not* the `status_*` tokens, which was the expedient option: a phase of a
request and a class of HTTP status are unrelated, and borrowing one palette for the other means
retuning "redirect orange" restyles the chart.

The first version of that test demanded 1.6:1 of luminance between every pair, so the chart would
survive greyscale. **The premise was false and the test was corrected rather than the palette.**
Every phase carries its own row, its own label and its own duration, so colour is reinforcement
and not the channel identifying anything — demanding a luminance ramp across four bars would have
forced four muddy shades to satisfy a requirement nothing has. What is left is the failure an eye
cannot catch in review: two tokens accidentally *equal* from a copy-paste between palettes, which
makes the chart monochrome with no line of code looking wrong. Plus 3:1 against every surface,
where the non-obvious pairing is `bg_elevated` — that is the bar's own track and a different value
in each theme, so a colour tuned against the pane can still sink into the track it sits in.
Whether the four *read* well together is a paint, and a person looking at the window is the only
instrument for it.

One thing came free: the tab reads `view.displayed()` like every other region in the pane, so
browsing the history shows that run's timing rather than the live one, with no rule of its own.

---

## 6i. The proxy — a default nobody chose

reqwest 0.13's `ClientBuilder` starts with `auto_sys_proxy: true`, and hyper-util's matcher reads
`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY`. So every request Zuno has ever sent went
through the system proxy when one was configured, with nothing on screen saying so and no way to
override it. That is precisely what `RequestSettings::cookie_store`'s own comment says a
behaviour like this must not be — *"visible and switchable rather than silently hardcoded on"* —
and it was the same shape one dependency down.

Found by reading the vendored source rather than by using the app, which is why §11 never listed
it: that table records capability Zuno *built* and did not surface, and this was inherited.

### Three states, because two cannot say it

`ProxyMode` is `System | Off | Url(String)`. An `Option<String>` was the obvious model and cannot
express this: `None` would have to mean both "whatever the environment says" and "nothing", and
those are different requests on the wire. Same correction `Connection` needed in §6h, arrived at
for the same reason.

**`Off` has to call `.no_proxy()`.** Merely not setting a proxy leaves `auto_sys_proxy` on, so
"off" would go on quietly using the env var — `Engine::clear_cookies`' problem exactly, one
setting over. `Url` needs no such call, because `ClientBuilder::proxy` clears the flag itself.

The default is `System`, which is what Zuno already did. Changing it would silently stop working
for anyone behind a corporate proxy, and that failure presents as a network problem rather than
as a setting.

### App-level, and that decides three other things

It lives in `app.json`, not in `RequestSettings`. A proxy is a property of this machine and its
network rather than of any request — and `app.json` is never committed, so a URL carrying
`user:pass@` cannot reach a collection file the way a `RequestSettings` field would (invariant
10). Putting it in the request would have meant resolving `{{vars}}` in it so the password could
live in `dev.local.json`; app-level makes that whole question disappear.

Three consequences, each of which shortened the slice:

- **No curl representation at all**, in either direction. `-x`/`--proxy` stays reported-and-
  ignored on import, and the exporter emits nothing — for the reason §10 already gives about the
  cookie jar: it is process-level machine config, not part of the request, and exporting it would
  hand someone a command that behaves differently from the one you ran.
- **`ClientKey` carries the mode and lost `Copy`** (a `String` in the enum). Worth it: with the
  mode in the key, changing the proxy misses the cache by construction, where a key without it
  would leave every cached client quietly routing through the old proxy until somebody remembered
  to evict. `changing_the_proxy_misses_the_client_cache` pins it.
- **`Engine::set_proxy` is a command, not a lock.** It lands on the engine thread in order with
  the sends around it, and an in-flight job keeps the client it already has — `clear_cookies`'
  rule, right for the same reason. `app_state::set_proxy` is the one funnel that tells both the
  file and the engine, because a saved value the engine never heard about is a status bar naming
  a proxy nothing uses.

### The picker, and why not the settings panel

`Ctrl+,` would have been the obvious home and it does not work: `settings_panel`'s rows are
toggles, a stepper and an action row — it holds **no `TextInput` at all** — and its arrow keys
move between rows, so a URL field would have to fight that model. A proxy needs a URL.

So it is a `Target` on the picker, which already answers "pick one of these, or type your own":
`System`, `Off` and **every proxy you have entered** are rows, and `set_fallback` offers the typed
text — the trick that lets an unknown verb become `Method::Other`. `ProxyMode::from_input` decides
whether the query could work, in core where it is unit-tested, so the picker cannot offer a row
that fails at the next send. It fills in a missing scheme (`localhost:3128` is what people type)
and **refuses `socks5`**, which reqwest rejects without its `socks` feature.

**The saved list was the correction that mattered.** The first build offered a URL row only while
that proxy was *current*, so switching to System or Off deleted it and the only way back was
retyping — losing a setting rather than changing one. `app.json` keeps the list; removal is its
own verb (*Remove a saved proxy*), mirroring `Forget workspace`, because a picker is a chooser
that closes and cannot express per-row deletion. Removing the one in use falls back to `System`,
since a mode that is set but no longer offered has no way back to it.

Still no `PickerDelegate` trait, at eleven consumers, all drawing as label plus dimmed detail.

### The badge is the half that earns it

The switch says what *will* happen; the badge says what *is* happening, and the second is the
whole reason this was worth fixing rather than documenting. `proxy_badge_label` is pure and
**always returns something** — `proxy system`, `proxy off`, `proxy corp:3128`, or
`proxy corp:3128 · env` when the environment is what supplied it, so "the mode is System" and "a
proxy is actually being used" stay distinguishable.

It was conditional at first, shown only when a proxy was in effect, which meant that from a cold
start there was no badge and no hint the feature existed — reachable only by a palette row nobody
would think to search for. That is the discoverability rule in §2 broken by the very thing meant
to satisfy it, and it is the second time in this document that a conditional indicator had to be
made permanent. It takes
the environment's value as an argument rather than reading it, both so it is unit-testable and
because `std::env::set_var` is `unsafe` under edition 2024 and racy across parallel tests. The
env read itself happens **once at boot** and is cached on `Workspace`, the way `globals_active`
is: the status bar asks every frame and env vars cannot change under a running process.

### What is asserted, and the one thing that is not

The routing is proved over a real socket, with the test server standing in as the proxy and never
forwarding: a proxied plain-HTTP request carries an **absolute-form** request line
(`GET http://host/path HTTP/1.1`), so the captured text alone proves where the request went.
`a_request_goes_through_the_configured_proxy` aims at a `.invalid` host for that reason — if the
setting were ignored, it fails to connect rather than quietly passing. Deleting the `Url` arm
fails it.

**`Off`'s `no_proxy()` call is not covered, and break-testing is what showed that.** Removing it
leaves `with_the_proxy_off_a_request_goes_straight_to_the_server` passing — it has to, because no
`HTTP_PROXY` is set in the test process, so `Off` and `System` are indistinguishable there. The
call only matters when the environment has one, and forcing that needs an env var, which is the
wall above. So that one line is held by review; the test holds the two things it can, the request
form and that the proxy port is never touched. Written down rather than papered over, the way §6a
does for `trash`.

`HTTPS` through a proxy is `CONNECT` plus a TLS tunnel, and is not driven at all — a socket test
faking that would be asserting its own fixture. The configuration path is shared with HTTP, so
what is untested is reqwest's tunnelling rather than Zuno's decision.

---

## 6j. Certificates — two halves that are not the same shape

mTLS APIs were simply uncallable, and a private CA could only be reached by turning verification
off entirely. Both are one `ClientBuilder` call, so they shipped together.

**PEM only, and the backend decided that.** Under `rustls`, `Identity::from_pem` is the sole
constructor — `from_pkcs12_der` and `from_pkcs8_pem` are `native-tls` only. One file carrying cert
and key, no password, which conveniently means nothing secret lands in `app.json`.

**The asymmetry is the whole design.** A client identity is *one*: a handshake presents a single
certificate and reqwest takes one per client, so choosing among several would need a rule for
which to use per host — a different feature. Trusted issuers are a *set*, all active, because
`add_root_certificate` is repeatable and trust is additive; a corporate CA and a staging CA at
once is ordinary. `cert_panel` draws two differently-shaped sections for exactly that reason — a
radio and a list — since a flat list would hide the one thing a reader needs to understand.

Identities are still kept as a list you switch between, the same correction the proxy needed:
choosing remembers the path so switching back costs no second trip through the file dialog.

App-level in `app.json`, for `ProxyMode`'s reason — these name files on this machine, so a
`RequestSettings` field would write an absolute path into every committed collection file. In
`ClientKey`, so a changed certificate misses the cache instead of leaving pooled clients
presenting the old one. **Known limitation:** the paths key the cache and the files are read when
a client is built, so editing a certificate in place without changing its path keeps the old one
until the setting is re-applied.

**A failure is reported, never swallowed** — a certificate silently dropped means a handshake
failing for a reason nothing on screen explains. Both a missing file and a malformed one name the
path.

### The panel, and what it replaced

`cert_panel` is a plain state struct with a render function rather than an `Entity`, the shape
`close_panel` uses: it owns no text input, so its whole state is which row is selected, and the
certificates are read from `app_state` at render time rather than mirrored. Rows carry a *path*
rather than an index, so a list that changed under the selection cannot activate the wrong file.
Removing the active identity also stops presenting it.

The entry point is a permanent `file-badge` in the titlebar, tinted `accent` when anything is in
force. It replaced a status chip that appeared **only when a certificate was already configured**
— which is the cold-start gap this document has now recorded three times (§2's audit, the proxy
badge above, and here), and the third time it was my repeating a mistake the section directly
above it describes. A padlock was the first glyph and said "secure" rather than "certificate".

The cost of the colour-only signal, stated: a colour-blind reader gets no "in force" cue from the
icon, and the panel is the fallback that spells it out.

### What is asserted

A real generated PEM builds a client; a missing one errors with the path named; a changed
certificate misses the cache. The **selection** cannot be driven headlessly —
`prompt_for_paths` is `unimplemented!()` in the test platform, the same hole `reveal_path` and
`open_with_system` sit in — so what is tested is everything below it. An actual mTLS handshake is
not exercised: that needs a server demanding a client certificate, which is reqwest's behaviour
rather than Zuno's decision.

---

## 6k. The update notice — and why there is no updater

Zuno installs system-wide through `apt`, so installing an update means root. The shape that was
sketched first — the app downloads the package, prompts for a password, installs, restarts — was
dropped, and the reason is worth keeping because it inverts on one fact: **the password problem
exists only because a *window* is asking.** A terminal asking for `sudo` is unremarkable, and
`scripts/install.sh` is already one command for both installing and updating. So the app's whole
job is to say a release exists and hand over that command.

Four things that decided it, beyond the ceremony of a polkit policy and a helper binary:

- **Chicken-and-egg.** The policy would ship *in the .deb*, so nobody on the current release
  could use the first auto-update anyway. The one manual step is unavoidable either way, and
  spending it on the script buys updates forever after.
- **An app that `dpkg -i`s behind apt's back desynchronises dpkg's state**, and it gets worse
  the day there is an apt repository.
- **Root plus an auto-downloaded binary is a supply chain**, defensible only with signature
  verification — real work, for a convenience.
- It is the one path where a failed install leaves someone with no working Zuno.

### The check reads a redirect, not the API

`releases/latest` answers `302` with the tag in `location`. That needs no JSON parser, and —
unlike `api.github.com` — has no 60-per-hour limit shared by everyone behind one NAT. An office
all starting at nine would exhaust that limit, and the failure here is silent by design, so the
feature would stop working for precisely the people most likely to have it. `follow_redirects` is
therefore **off** for this one request: following it lands on an HTML page that would have to be
scraped, while the redirect itself is the answer.

It goes through `Engine::send` rather than a second HTTP client, which is §6b's rule and earns
more here than it does there: the check inherits the proxy (§6i) and the trusted CAs (§6j), and a
corporate network is exactly where a second client fails silently.

**Every failure is nothing.** Offline, firewalled, rate-limited, answered with something
unreadable — all of them leave `Update::Unknown` and put nothing on screen. `version::is_newer`
returns false when *either* side is unparseable, so a malformed answer cannot produce a notice and
a malformed `CARGO_PKG_VERSION` cannot make every check announce one.

**Once per launch, not on a stored timer.** A timestamp in `app.json` was the plan and buys very
little against a redirect with no rate limit — while costing a wall clock in the code path, which
the test dispatcher's simulated clock makes the one half no test could drive. The cost is stated:
a window left open for a week does not re-check. The check is started from `main` rather than from
`Workspace::new`, and that placement *is* the test isolation — the harness builds a `Workspace`
directly, so a check in the constructor would put a real request to GitHub in front of every test
in the suite.

### The chip is the one conditional control in the titlebar

Two rules in this codebase point opposite ways here, and both are right. A **control** must be
permanent or the capability behind it cannot be discovered from a cold start — §2's audit, the
proxy badge, the certificate button, three times. A **state badge** must be conditional, because
"a badge that's always there stops being read", which is why `cookies on` only appears when they
are on.

This is a notice, not a control: with nothing to update to there is nothing to discover. So the
chip is conditional, and the always-available path is the palette's *Copy the Zuno install
command* — which is also how someone sends the command to a colleague.

It sits **first in the titlebar's right-hand cluster**, and that position is what makes it free.
The cluster is `flex_none` inside a `justify_between` row, so it is pinned by its right edge and
grows leftward: a child added at the *start* moves nothing, while one added at the end would shift
the window controls out from under the pointer. That is the layout-jump failure §6a records about
the collection panel, which manufactured a flaky test rather than merely looking bad.

Three smaller decisions:

- **Clicking opens a menu, not an action.** There are two questions — how do I update, and should
  I — and one click can only answer one. `Copy install command` and `What's new in <version>`,
  with `Dismiss until the next release` below a separator. Fourth consumer of `context_menu.rs`.
- **The copy announces itself.** A clipboard write is invisible, so without the status line this
  is indistinguishable from a dead control, and the second half of the message carries as much as
  the first: *run it in a terminal* is what stops someone waiting for Zuno to install it.
- **The dismissal stores the version, not a flag.** It lapses by itself when a later release
  lands, so there is no reset to forget — and the thing that forgets to reset a flag is a user who
  never sees another update. The test asserts both directions, because one asserting only that the
  dismissed version stays hidden passes against a plain boolean.

`ZUNO_NO_UPDATE_CHECK=1` turns the request off entirely. An env var rather than a setting: the
people who want it are already launching from a shell, and a toggle would need a home in a panel.

---

## 6l. The header-name dropdown — a combobox, not the picker

Authoring a header meant typing its name from memory, including `Authorization`. The picker is
the wrong instrument: it is a *centred modal* that owns the screen, which is right for a palette
and absurd for filling a table cell. So this is the first **inline** overlay — anchored under the
cell, with focus staying in the box behind it.

**A combobox, not an autocomplete.** An empty cell offers the whole table; typing filters it. A
list that only appears once you start typing is useless to the person who does not know what
headers exist, who is the entire audience for the feature — the discoverability rule one level
below `affordances()`, which proves every *action* has a mouse path and says nothing about a user
who cannot name the thing they must type.

`core/src/headers.rs` holds the table and the match, pure, for `fuzzy` and `version`'s reason.
Three decisions in it:

- **Not the IANA registry.** Hundreds of names, most response-only or protocol extensions; a list
  you scroll past is worse than typing.
- **Prefix before substring, and never fuzzy.** `fuzzy.rs` scores subsequences, which is right
  for half-remembering a command name and wrong here: `cte` would match `Content-Type` and a
  dozen others, and a list that reorders unpredictably as you type is one you stop reading.
- **An unknown name offers nothing**, rather than falling back to the full list. `X-Trace-Id` is
  an ordinary thing to type, and a list reappearing under it is noise at the exact moment you are
  doing the thing the list cannot help with. A *finished* name offers nothing either.

### Three mechanics, each of which decided the design

- **Owned by `Workspace`, not by the row.** `request_pane` has ten `overflow_hidden` ancestors,
  and an absolutely-positioned child is still masked by one — so an inline `anchored()` under the
  cell is clipped. Rendered from the root there is nothing to escape. Same move `context_menu`
  makes, and the same reason.
- **No scrim and no focus transfer**, which is what separates it from that menu. A scrim swallows
  the next click; focusing the list stops the typing it exists to accompany. It carries
  `.occlude()` only so the wheel does not scroll the pane behind it, since scroll handlers
  consult the hit test rather than propagation.
- **The position comes from `TextInput::last_bounds`**, already recorded for hit-testing and the
  IME rectangle and therefore already in window coordinates. It is written during *paint*, so on
  the frame a brand-new row first appears the position is not yet known and the list is absent —
  measured, `false` on frame one and `true` on frame two. That is why the list's **contents** are
  derived from the focused row alone and only its *placement* reads bounds: folding the two
  together made what the list would offer unobservable for a frame, which two tests then could
  not assert.

### The rule the feature turns on

**Typing never highlights anything.** `Enter` accepts only an entry `up`/`down` explicitly moved
to, so a half-typed custom header is never replaced by whatever ranked first. Break-tested: with
the guard removed, `accept-c` silently becomes `Accept-Charset`, which is a wrong header sent to
a real server and nothing on screen saying so.

The highlight is *not* reset by typing, deliberately. It is an index into the list as currently
derived and is rendered from that same list, so it always points at the row you can see — where
clearing it on every keystroke would make `down`-then-type-one-more-character lose your place.

**`escape` needed a fallback, and this is the part with no visible symptom.** The four keys are
bound in `Some("HeaderCell")`, and a leaf-matching predicate *ties* with a context-less one, with
the tie going to later registration — so this `escape` wins whenever a header name has focus.
Without forwarding to `cancel_request` when no list is open, putting the cursor in a header cell
would quietly disarm cancelling an in-flight request. Seventh time that ordering rule has decided
behaviour here, and the first where the cost was a *different* feature silently stopping.

Accepting writes through `select_all_text` plus the ordinary edit path rather than assigning the
content, so `Ctrl+Z` undoes it and `Changed` still fires — body prettify's reasoning, applied to a
single-line input.

*Deliberately absent:* header **values**. `Content-Type` has a known short set and is the obvious
second consumer, but doing both at once means debugging the anchoring and the data at the same
time. Query parameter names get nothing at all, and never will: they are the API's vocabulary,
not HTTP's.

---

## 6m. The inline body diff — normalize, then borrow

`ResponseDiff` (§6, the diff bar) answers *whether* the body changed. `BodyDiff` answers *what*
changed, and lives on a fourth response tab beside Body, Headers and Timing.

**The normalization is the feature; the diff algorithm is a dependency.** A JSON API answers on
one line — `{"id":1,"name":"ada"}` — so a line diff over the raw bytes has exactly one line to
report and concludes "it changed", which is what the diff bar already said. Both sides go
through `json::format::pretty` first, which puts one field per line, so the comparison lands on
the field that moved. That reuses the formatter §6g already built and already trusts: it copies
tokens from their spans rather than re-serializing, so key order and number formatting survive
and two responses cannot come out different here because of how they were *printed*.

Pretty-printing is **all-or-nothing across the pair**. Formatting one side and not the other
makes every line differ. An endpoint that starts returning an HTML error page instead of JSON is
a rewrite either way, but reformatting the JSON side buries the one line worth reading under a
wall of re-indented ones.

**Patience, not Myers — and Myers is the crate's default.** Pretty-printed JSON is full of
interchangeable `},` and `],` lines, and Myers will happily pair a closing brace in one document
with an unrelated one in the other to shorten the edit script, producing hunks that straddle
object boundaries. Patience anchors only on lines *unique to both sides* — which for JSON means
the keys. Same reason `git diff --patience` exists.

**Refinement is by character, and that was measured rather than chosen.** `similar`'s
`InlineChangeMode::Auto` resolves to *whitespace-separated words* without the `unicode` feature,
and JSON has almost no whitespace: `"https://example.com/v1/users"` is a single token, so
bumping a path segment marked the entire URL as changed — precisely the coarse answer refinement
exists to improve on. The test `a_changed_line_marks_only_the_part_that_moved` was written
against `Auto`, failed, and is what pins the mode. Character tokens fragment, so
`semantic_cleanup` shifts the boundaries back out. The `unicode` feature would buy grapheme
awareness and a `unicode-segmentation` dependency; the point of choosing this crate over
`imara-diff` was that it brings none.

**A flat line list, not nested hunks.** The renderer is a `uniform_list`, which addresses items
by a single index, so hunks are flattened and the gap between two of them becomes a `Skipped`
line carrying its own count. Every list mechanic — virtualization, the one-sampled-row
horizontal sizing, the scroll indicator — is then the body viewer's, unchanged. The sampled row
matters here in a way it does not there: `with_width_from_item` defaults to row 0, which in a
diff is as likely as not to be the narrowest row in the list.

**Three bounds, because two of them do not constrain the case that bites.** `MAX_DIFF_BYTES`
(4MB) refuses the pair outright; `MAX_REFINE_BYTES` (2000) skips refinement on a line too long
to read across, which is what stops a minified HTML body running Patience over half a million
character tokens. Neither bounds two documents that *share nothing*: those produce a single hunk
holding every line of both, so `MAX_DIFF_LINES` (5000) caps the output and the pane says it did.
A diff that stops early while claiming to be complete is a lie about the one thing the reader
came to establish.

**Computed eagerly, beside the summary diff and in the same background task.** Lazily would save
work whenever the tab is never opened, and costs the thing that decides whether opening it is
worth a keystroke: the tab's label reads `Diff +3 -1`. The cost is bounded on both sides that
matter — identical bodies settle on a byte compare, and anything over the cap returns without
diffing. One task rather than two so the summary and the detail land in the same frame; split,
there is a window where the bar says the body changed and the tab still shows nothing.

**The guard against an older run is real state, not a render-time check.** `body_diff` compares
the live response with the one before it, so beside a run from history it labels lines as added
that the run on screen never contained — a wrong answer presented as a right one, which is worse
than none. The diff bar solves this by hiding; a tab cannot hide without shifting the three
beside it, so `RequestView::diff_to_show` returns `None` and the tab renders a note. A method
rather than an `if` in the renderer because **nothing headless can observe that a region was not
painted** — `cx.debug_bounds` reports the last frame drawn, so `is_none()` would read as coverage
whether or not the guard existed.

*Deliberately absent:* a side-by-side view. The response pane is a column roughly 500px wide;
two of them side by side leave ~35 characters each, which is narrower than one line of indented
JSON. Unified is not a compromise at this width, it is the only readable option.

---

## 6n. HTML bodies — read the page, not the markup

An API client gets HTML for one reason above all others: something broke and a framework
answered with a debug page. The traceback is in there, under forty kilobytes of markup. So the
body view grows a second half — the text pulled out of the page — and `body_header` grows one
control to swap between them.

**"Preview" would be the wrong word and was deliberately not used.** gpui 0.2.2 has no webview,
and rendering HTML means a CSS cascade, box layout, floats and images — a second product, not a
feature. What is built is *extraction*: what the page **says**. What it **looks like** is a
different question, and the honest answer to it is the user's own browser (see below).

**The crate choice inverted on evidence, and that is the point of recording it.** `nanohtml2text`
was chosen first and is better on every axis that is easy to measure: one crate against twelve,
and ~7ms/MB against ~100ms/MB. It also passes `<pre>` contents through **raw** — entities
undecoded, nested tags left as literal markup, and no separation from the block before it. Django,
Flask and Rails all put the traceback in a `<pre>`, so the cheap crate was broken in exactly the
case it was being bought for, and the benchmark that made it look fine did not contain one.
`a_traceback_in_a_pre_block_keeps_its_text_and_its_indentation` is what settles it, and swapping
the crate back fails it on the first assertion.

`html2text` is configured with `TrivialDecorator` and `no_table_borders`, because its default
output is shaped for a terminal: `#` before a heading, backticks around `<code>`, box-drawing
rules around tables. **Decoration invented by the viewer is indistinguishable from decoration
that was in the response**, which is the one thing a debugging tool must not do.
`nothing_is_decorated_with_terminal_markup` holds that.

**Both halves are indexed up front, and `kind` points at one of them.** `HtmlBody` holds the raw
`LineIndex` and the extracted one; the toggle swaps which `Arc` sits inside `BodyKind::Text`. The
tidier model is a `BodyKind::Html` variant, and it was rejected: ten accessors match on `kind` —
`row_count`, `searchable_source`, `rows_for_offsets`, `select_visible` and the rest — and every
one would have to re-ask which half is showing. Swapping the `Arc` means none of them change, and
the text view inherits the raw view's search, selection and horizontal scrolling for free. The
selection *is* cleared on a swap, because the two halves share no line numbering.

**The preference lives on `RequestView`, not on `BodyView`.** `BodyView` is rebuilt on every
response, so a preference held there resets on each send — silently, at the one moment the reader
is least likely to notice and most likely to be re-reading the same endpoint. Threaded into
`BodyView::build` exactly as `force_parse` is.

**Text is the default**, and this was the one call put to the owner. In an API client, HTML
arriving is overwhelmingly a framework saying something went wrong; the alternative considered
was auto-switching on a 4xx/5xx, which is nicer in that exact moment and makes a 200 behave
differently for reasons nobody asked for. Sticky beats clever: anyone here to read markup flips it
once per buffer and never thinks about it again.

*Deliberately absent, and designed for:* **Open in browser.** Writing the body to a temp file and
calling `cx.open_with_system` — already in the tree for *Reveal in file manager* — is exactly what
Postman's preview does, which is why relative assets break there and a styled page comes up naked.
Injecting `<base href="{scheme}://{host}/">` from the request URL fixes that for one string
concat, and is a genuine improvement on the thing being matched. Its limit has to be stated rather
than discovered: `<base>` resolves public static assets, but the page opens from a `file://`
origin, so no cookies go, the auth header Zuno sent is not replayed, and any `fetch()` back to the
API is cross-origin. A server-rendered template comes up right; an SPA shell stays blank.

---

## 6o. Binary bodies — a hex dump instead of a dead end

`BodyKind::Binary` used to render one sentence: *"184 KB of binary data"*. No rows, no search, no
selection, no way to tell a JPEG from an HTML error page served with the wrong content type. It is
now a `hexdump -C`-shaped view, and the variant that stood for "nothing to show" is **gone** —
nothing constructs it, so it was deleted rather than left as a branch nobody reaches.

**The dump is text, and that decision is the whole slice.** The viewer already has a virtualized
text path carrying search, selection, copy and horizontal scroll. Rendering hex rows by hand would
have meant reimplementing four mechanisms to draw something that is, in the end, monospaced lines.
So `hex::dump` produces a `String`, `LineIndex` indexes it, and `text_list` draws it — the same
three steps any text body takes.

**`BodyKind::Hex` is nonetheless its own variant, not `Text`.** It holds the same type and behaves
identically in six accessors, which argues for reuse — but `raw_is_json` is `Text(_)` *plus a
notice*, meaning "this was meant to be JSON and fell back to raw", and a truncated dump carries a
notice. Reusing `Text` would therefore have syntax-highlighted a hex dump as JSON, colouring byte
pairs that happen to look like numbers. The cost of keeping them apart is one extra pattern in the
arms that treat them alike: `BodyKind::Text(lines) | BodyKind::Hex(lines) =>`.

**Truncation keeps the front, and this is the one cap in the codebase that does.** `MAX_AUTO_PARSE`
and `MAX_EXTRACT_BYTES` both *refuse* above their limit, because half a JSON tree or half a page of
prose is worse than none. A hex dump is the opposite: it is read for magic numbers, headers and
framing, all of which are in the first few rows, and nobody inspects the middle of a JPEG. So a
large body shows its first megabyte and the notice says so.

**The ASCII gutter is one cell per byte, and stays that way deliberately.** Anything outside
printable ASCII is a `.`, including valid UTF-8 — widening it would let a two-byte character
occupy one cell while consuming two bytes, so the gutter would stop corresponding to the hex
columns beside it. A short final row pads its hex columns for the same reason: without the
padding the last gutter slides left and no longer lines up with the rows above.

*Deliberately absent:* a toggle to hex for bodies that **are** text. Occasionally useful — checking
a BOM, a trailing `\r` — and it is the obvious generalisation of `HtmlView` into a three-way
choice. Not built, because the gap being closed was binary responses having nothing at all.

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
to stderr, plus a perf floor on `zuno-core` for parse/flatten. A budget you don't measure is a
budget you've already blown.

That floor shipped as ordinary `#[test]`s asserting wall-clock bounds (`core/tests/json_perf.rs`)
rather than the criterion benches this line originally named — criterion was never added. Run
them in **release**, as CLAUDE.md says: in a debug build they are slow enough to fail under load,
which is noise rather than signal.

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
serde_json   = "1"          # promoted from dev-dep when the collection format landed
# Moving a request to the desktop trash. Default features off: they pull `chrono`, and every
# `coinit_*` flag is Windows COM configuration. Hand-rolling the XDG spec was rejected — the
# same-filesystem case is easy and the cases that decide whether a restore works are not.
trash        = { version = "5", default-features = false }

# The two tower traits, and only those two, for the connector layer that times a connection
# (§6h). `tower` itself would do — it is already in the tree — but these are what it re-exports
# and they carry no features to choose. Both are already in `Cargo.lock` at 0.3.3 via reqwest,
# so declaring them adds **no** crate to the graph. Checked with `cargo info`, per the rule
# above, rather than written from memory.
tower-layer   = "0.3.3"
tower-service = "0.3.3"

# The inline body diff (§6m). Chosen on **measured** dependency weight rather than reputation:
# `cargo tree` on a scratch crate gives `similar` 1 crate, `imara-diff` 4, `html2text` 30 and
# `syntect` 46. `imara-diff` is the faster engine and is what helix and gitoxide use, but it
# stops at the edit script — the word-level refinement inside a changed line would be ours to
# write, which is most of what was being borrowed. `inline` is **not** a default feature and is
# what `iter_inline_changes` lives behind; it pulls nothing extra.
similar       = { version = "3.2.0", features = ["inline"] }

# HTML -> readable text (§6n). The one place in this tree where the *heavier* crate won, and it
# won on correctness rather than features: `nanohtml2text` is one crate to this one's twelve (as
# resolved here — a bare `cargo tree` says thirty, but `thiserror`, `unicode-width` and friends
# are already present) and twelve times faster, and it passes `<pre>` contents through **raw**,
# entities undecoded and nested tags literal. `<pre>` is where every framework puts its
# traceback. `default = []` keeps `css`, `xml` and tracing off — they would add `nom`,
# `xml5ever`, `log` and `backtrace`.
html2text     = { version = "0.17.1", default-features = false }

# `ropey` and `criterion` were listed here for a long time and neither is a dependency.
# The rope was dropped deliberately (§7); the perf floor is an ordinary `#[test]` asserting
# wall-clock bounds (`core/tests/json_perf.rs`), which needs no bench harness.

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

In the editor the clamp used the *cursor's* line width at first, on the reasoning that the widest
*visible* line would make the limit jitter as you scroll vertically. The first half of that
survived and the conclusion did not: §6's horizontal-scrolling slice found that bounding by the
cursor's line is two opposite bugs — a caret on `{` gives a maximum of zero, so any scroll snaps
home, and once that line scrolls out of view there is no bound at all. It clamps against the
**document's** widest line now, which is stable in the way the visible one is not.

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

**Nothing remains** *of the items this table ever listed.* `Ctrl+,` closed five, the method picker
a sixth, `Ctrl+H` a seventh, and body authoring took form, binary, and multipart — the last of
which was the only item here that ever needed engine work rather than UI.

> **And then a §11-shaped item turned up that §11 had never listed: the proxy.** reqwest 0.13
> builds every client with `auto_sys_proxy: true`, so Zuno had honoured `HTTP_PROXY` on every
> request since M1.2 — honoured on every request, invisible, unreachable, which is this
> section's definition exactly. It is closed now (§6i).
>
> Worth recording because of *how* it was missed rather than that it was. This table was written
> by looking at what Zuno had built and not surfaced; a behaviour inherited from a dependency's
> default was built by nobody, so it cast no shadow here. That is the same blind spot ROADMAP
> names about its own audit — the one that hid OpenAPI import and the collection runner — one
> layer further out: **§11 can only see capability someone chose to add.** The way it was found
> was reading the vendored `reqwest` source, not reading Zuno.

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
- **A clicked setting did not reach the request, and only the mouse path was wrong.** The row's
  click handler called `panel.confirm` directly and dropped the `bool` it returns — and that `bool`
  is the whole mechanism: only `Workspace::setting_confirm` reads it, and only it calls
  `commit_settings`. So a click updated the panel's own copy, redrew the new value, and left the
  request unchanged, while `Enter` on the same row worked.

  Fourth occurrence of "actions, not direct calls" after the body-kind chip, the fold-all buttons
  and the fold chevron — and the first where the direct call was **not** a visible no-op, which is
  why it survived. The panel showed exactly what you asked for. The keyboard test asserts against
  `spec(cx)` precisely because "a panel that edits a copy nothing reads would look identical on
  screen", and then no equivalent test was written for the click. That asymmetry is the lesson:
  **a convention checked only on the path that already worked proves nothing about the other one.**
- ~~**Settings are per-request, and stay that way for now.**~~ **Two scopes now.** The claim
  above was that a defaults layer needed a global → environment → request scope model, "the same
  one environments has to build". It needed **two of those three**: `app.json` holds one
  `RequestSettings` that a new request starts from, and the request holds its own. There is no
  middle layer, because nothing anyone has asked for varies a timeout *by environment* while
  varying it by request — environments carry values, not policy. Dropping the layer that was
  never wanted is what turned this from a blocked design into one panel row.

  **Two triggers, not a scope row.** `Ctrl+,` and the request pane's gear edit the buffer in
  front of you; `Ctrl+Shift+,` and a gear in the titlebar edit the defaults. The first build put
  a scope row inside one panel, and it needed the header *and* that row to both spell out which
  set was live — a design arguing with itself. Where a gear lives says what it changes, so the
  titlebar's sits in the app's own furniture and the pane's stays with the request. The panel is
  one entity either way; it holds one `RequestSettings` and a `Scope` saying where to write it.

  Applied wherever the app makes a request rather than loading one: a new tab, the buffer that
  replaces the last closed one, the OpenAPI spec **fetch** — which is the case that earns it most,
  since a spec served from the same self-signed box as the API cannot be downloaded without it —
  and the requests an import writes, because fixing TLS on forty files one at a time is the same
  papercut multiplied. Never applied to a request read from a file: its settings are in it.

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
- **Workspaces: a registry in `app/src/app_state.rs`.** `app.json` under `XDG_CONFIG_HOME` holds
  `{id, path}` per workspace, the last one opened, and the theme. The collection root and the
  session file are the *resolved answer* rather than sources of truth — `resolve` installs both
  from the active entry, which is what let the test harness's `install_at` seams stay exactly as
  they were. Three decisions: an entry has **no name** (it is the directory's, so a `mv` cannot
  leave one lying — the `label_for` argument); the session path is **derived** from the id rather
  than stored, so an entry cannot point at another workspace's session; and the pre-registry
  `session.json` is **copied** into `sessions/default.json` once, never moved, so a downgrade
  still finds its session.

  Four rules the verbs follow. **Switching writes the current session before the globals move** —
  `session::save` writes to whatever `SessionFile` holds, so re-resolving first files this
  workspace's buffers under the next one's id. It also means switching needs no unsaved-changes
  prompt: every buffer's live spec goes into the session and comes back. **Forget never touches
  the directory**, only the entry and `sessions/<id>.json`, and **refuses the last entry**, since
  an empty registry leaves the window with no collection at all. And an **id is a filename**, so
  it is `slug`ged *and lowercased* — `Payments-API` and `payments-api` would otherwise be two
  entries fighting over one session file on a case-insensitive filesystem.
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

~~Still open: nothing about the format.~~ **Nothing is open here any more, and this paragraph
outlived its subject by several slices.** It said the collection was "read-mostly from Zuno's
side: no delete, no rename, no folder authoring beyond `mkdir`", and that the root was a single
XDG path with no runtime setter — so the git argument the format is built on was unreachable from
inside the app. Every clause of that is now false: §6a has delete, trash, rename, duplicate, move,
New folder and New request, and workspaces (below) let a collection live in the repo it
describes. Struck rather than deleted because it is the direction CLAUDE.md calls the most
expensive — a doc asserting a gap the code no longer has sends the next reader hunting for
something that is not there.

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
- **Tabs are a fixed width, and the label is shortened in Rust rather than by the layout.**
  `Ctrl+W` and middle-click were the only ways to close a tab; there is a `×` now, and with it the
  label had to stop being hard-cut. A fixed tab width means a tab doesn't move under the cursor
  when a URL is edited — but it is *not* what produces the ellipsis, and two attempts shipped
  believing some arrangement of widths would. gpui's `truncate()` caches its first measurement and
  only ellipsizes text handed a definite width, so whether it fires turns on layout several
  elements away; neither `flex_1().min_w(0)` nor an explicit `.w()` made it fire here.
  `zuno_core::request::elide` now shortens the label before it ever reaches an element, and
  `truncate()` stays underneath purely as a backstop for pathologically wide glyphs.

- **The strip scrolls, and now says so and drives itself.** `overflow_x_scroll` was the whole of
  it for several milestones: the tabs scrolled by wheel with nothing on screen indicating it, so
  a tab past the right edge had no mouse path at all. Two halves fixed that.

  A **chevron at each end**, drawn only when `tabs_overflow` says the tabs want more room than
  the strip has — computed from the window less the panel rather than read off the scroll
  handle, whose extent is written during prepaint and is therefore a frame behind. They are
  *pinned siblings* of the scrolling box, not children of it: inside it they would slide away
  with the content, which is the one thing a scroll control must not do. Deliberately not
  `ui::icon_button`, which titles itself with its action's keystroke — scrolling a viewport is
  not a verb, and inventing an action would need a palette row nobody would search for. They are
  never dimmed at the ends, for the frame-behind reason again: a chevron greyed out one tab early
  reads as broken, while a click that cannot move simply does nothing.

  And **`activate` reveals the active tab**, eased through the same animation the chevrons use.
  Without it a new buffer appended past the right edge and `Ctrl+T` produced no visible change —
  holding the key looked like a dead shortcut. On every activation rather than on creation, since
  `Ctrl+Tab` onto an off-screen tab has the same problem. `reveal_offset` is pure and returns
  `None` for "already visible", which is what stops an ordinary tab click firing an animation.

  Two things the animation forced. It interpolates from the captured offset toward an **absolute
  target** rather than adding a delta per tick, because gpui re-clamps `offset.x` to `[-max, 0]`
  in its own prepaint — incremental steps get eaten at either end and never reach a fixed point.
  And the running `Task` is held so a second click replaces it, since dropping a `Task` cancels
  it and two animations must never fight over one offset; the target accumulates so three quick
  presses travel three tabs.

  **Testing it needed a fact about the harness** now in CLAUDE.md: the test dispatcher's clock is
  simulated, so a `timer()`-driven animation does not advance under `run_until_parked` and must
  be stepped with `advance_clock`. The arithmetic is unit-tested on `reveal_offset` and the
  *wiring* separately, through `active_tab_in_view` — neither catches the other's failure, which
  is why both exist.

  **The deciding argument was testability, not elegance.** Shaped text has no width the headless
  platform can read: a block wrapper stretches to its parent, and a flex wrapper hands the text
  `MaxContent` and thereby breaks the very truncation it was added to measure. Two tests written
  to catch the bug passed against it. A pure function over a string either shortened the label or
  did not, and `elide`'s unit tests plus
  `a_long_tab_label_is_ellipsised_before_it_reaches_the_strip` say which — the latter reaching
  `Workspace::tab_labels`, which exists as a method for exactly that reason.

- **The character budget and the label width are one decision in two constants.**
  `TAB_LABEL_CHARS` is tuned to `TAB_LABEL_WIDTH` in the UI font at `text_xs`, and counting
  characters rather than measuring pixels is deliberate: real widths need the shipping font, which
  the test platform does not have. Erring short is invisible; overshooting puts the ellipsis back
  under the clip where it cannot be seen at all.

- **The close button closes the tab it is drawn on, not the active one** — so it activates first,
  the same two steps middle-click already took. The test asserts which buffers *survive*, by
  clicking the two remaining tabs, and it has to: after closing index 2 of three, `close_tab`
  activates `min(2, len - 1)` and lands on the same buffer either way, so an `active_view`
  assertion passes against a handler that forgot to activate. Verified by deleting the `activate`
  and watching it fail.

  The `cx.stop_propagation()` beside it is the opposite case and is commented as such: the suite
  passes with it deleted, because `activate` early-returns on `views.get(ix)`. It stays as
  convention, and a test for it would assert nothing while reading as coverage.

Curl import now opens a **new** buffer. Replacing was only defensible while there was nowhere else
to put the result; an import over unsaved work destroyed it with no undo. `RequestView::load`
remains for genuine in-place replacement.

**Dirty buffers — answered.** `RequestView::baseline` holds the request as its *file* has it,
set in `load` (the one funnel every buffer fill goes through) and again on save; `is_dirty`
compares against it. Three decisions:

- **The baseline is not an `Option`.** A buffer with no file keeps what it was created with, so a
  fresh tab is clean and one you typed into is not — no second "untitled" case to special-case.
- **Restored buffers re-read their file rather than persisting the baseline.** The session stores
  each buffer's *live* spec and never what the file said. A stored baseline would record what the
  file held when you quit, so a `git pull` while Zuno was closed would leave a buffer reading
  clean against a file it no longer matches. Reading is also clean-until-corrected, which is the
  right way round — the alternative marks every tab dirty on launch until the reads land.
- **`is_dirty` compares field by field instead of building `spec()`.** The strip asks every buffer
  every frame and `spec()` clones every row and the whole body. The cost is `body_matches`
  mirroring `body()` by hand, including its two collapses to `Empty`;
  `a_freshly_loaded_request_is_clean_for_every_body_type` is what stops that drifting.

What this bought is not the dot. **`Ctrl+W` was the one path in Zuno that could destroy work** —
quitting preserves every buffer through the session envelope, and closing a tab preserved none,
with no prompt and no undo. `close_tab` now only asks; `force_close_tab` is the only thing that
closes, the same split `DeleteRequest`/`ConfirmDeleteRequest` uses. Saving from the prompt closes
only if the save actually succeeded, since a failed write plus a close discards the work the
person just asked to keep.

The dot shares the close button's slot and the tab's hover trades one for the other, the editor
convention. Stacked rather than chosen in Rust, since hover is a paint-time style: both are always
painted and only their colours move, which is also what keeps the label from shifting. It reuses
`ICON_GROUP` — `GroupBounds::get` takes the innermost open group of a name and sibling tabs push
and pop separately, so one constant is still per-tab and hovering one does not light up the rest.

**The strip has a context menu**, the primitive's second consumer: Close, Close others, Close to
the right, Close all, and Copy as curl. Right-click activates the tab first, so every row acts on
the active buffer and `Close` needed no action of its own — the two steps the `×` and middle-click
already take. Rows hide rather than grey when they would do nothing.

Two things it forced. Targets are **entity ids resolved fresh**, not indices: every close
renumbers `views`, so a stored list would aim at whatever slid into each slot. And `CloseConfirm`
grew from one index to a set, because closing ten tabs with four unsaved would otherwise stack
four modals with no way to see how many were coming — it asks **once**, naming the request when
one is unsaved and counting them otherwise. A failed save keeps that buffer open, which is the
single-buffer rule and matters more in a batch. `Ctrl+W` routes through the same path, so the
prompt cannot behave differently depending on how many tabs you asked to close.

*Still absent:* tab reordering and renaming. And the strip hides itself at one buffer, so a lone
dirty buffer shows no dot — the prompt is what covers that case.

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

A fifth, and it is the mouse half of the same idea: **a modal occludes.** `modal_open` and the
`Tab` guard make a modal own the *keyboard*; nothing made it own the *wheel*, so scrolling over an
open picker scrolled the response body behind it. A scrim that catches clicks does not help,
because scroll handlers gate on `hitbox.should_handle_scroll` — the hit test, not propagation.
`.occlude()` marks the scrim `BlockMouse`, and `hit_test` stops there. All three overlays carry it
now. Deliberately untested: nothing behind a modal moves in the headless platform either way, so
an assertion would pass against the bug — see the note on `picker.rs`.

*Rejected: scoping the `tab` binding with a context predicate.* GPUI matches only the **leaf**
context, so "not inside a modal" cannot be written once — it has to be restated for every modal
context that ever exists, and the failure mode when someone forgets is a dead keymap with nothing on
screen explaining it. A guard on the handler is one place and cannot be forgotten by a *new* modal,
only by a new focus-moving action.

**A row's two columns elide in opposite directions.** Imported collections put paths like
`TheGameYou-Misc-API-Notification-Blob/notification-controller/getPreSignedUrlInternal` beside
URLs like `http://localhost:8080/api/notification/v1/blob/internal/presigned-url`, and two faults
surfaced together: neither column set `whitespace_nowrap`, so a long one *wrapped* and the fixed
`ROW_HEIGHT` sliced the second line; and the label was `flex_none`, so it could not shrink and
pushed the detail out of the row. Neither shows in a short label, which is every label this
picker had until the OpenAPI importer.

The direction follows where each string keeps its information. A path's head names the
collection and its tail is one more request, so the **label keeps its head** (`elide`). A URL's
head is the `http://host:port` every row repeats and its tail is the endpoint that tells them
apart, so the **detail keeps its tail** (`elide_front`) — trimmed the other way, every row in a
collection reads `http://localhost:8080/api/notif…`.

**Neither column reserves half.** They are sized to their content — the default `flex: 0 1 auto`
with `min_w(0)`, *not* `flex_1`, which is `flex: 1 1 0%` and therefore a fixed half each whatever
they hold. And `split_budget` hands the spare characters to whichever column wants them: a short
label takes only what it needs and the URL beside it gets the rest, with half-and-half reserved
for the case where both want more than half. That rule is a pure function with a unit test, so it
is checked rather than eyeballed.

> **Two earlier versions, both worth keeping.** *Middle*-elision came first and was the wrong
> answer to the wrong question — it shortened the label alone, keeping both of *its* ends, which
> is sensible for one string and not what a two-column row needs. Then the budget was a fixed 62
> characters while the column was whatever flex left it, so the text was elided **and then
> clipped**: an ellipsis in the middle and a hard cut at the end of the same string. **A character
> budget only means something if it matches the width the column actually gets** — which is why
> `ROW_CHARS` is derived from the modal's real width and then split, rather than picked per
> column.

**Elided at render, never in `Item::label`.** `refilter` ranks the stored string, so shortening at
construction would mean typing the part that was dropped stops finding the row — searching against
an ellipsis. `a_long_label_is_shortened_on_screen_but_still_matches_in_full` types the middle of a
path, which both elisions remove.

The tooltip carries **both** strings, one per line, and appears when *either* was cut — whichever
one lost characters, the row as a whole is what you were trying to read. It has **no maximum
width**: the strings that need a tooltip are the ones too long for their row, and wrapping one
mid-path is precisely what the tooltip exists to undo. A pathological path makes a very wide
tooltip, which is the accepted cost. Lines are separate elements rather than one string with
`\n` in it, because `shape_line` carries a `debug_assert!` against newlines.

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

~~**Global settings defaults** (new, deferred deliberately).~~ **Shipped**, and §11's tail already
recorded it while this paragraph went on describing the gap — two sections of one document
disagreeing, which is worse than either being wrong alone. `app.json` holds one `RequestSettings`
that a new buffer starts from, reached by `Ctrl+Shift+,` or the titlebar gear. What follows is the
reasoning that got it there, kept because the correction is the useful part.

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

~~**The cookie jar's visibility** (new). It's on and invisible.~~ **Answered, and it got the
indicator *and* the toggle** — the `cookies on` badge in the status bar plus the `Ctrl+,` row,
which is what §11's own entry describes. The third option, a jar viewer, stays unbuilt for a
reason worth keeping: reqwest owns the store behind `cookie_store(true)` and exposes no way to
enumerate it, so a viewer needs a lower-level client. The same badge argument was reused whole
for the proxy in §6i — the toggle says what will happen, the badge says what is happening.

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

The clamp *was* `max_offset = cursor_line.width - viewport + caret`, and the retraction above is
still right about the entry being a phantom defect: returning the view to x=0 on a short line is
the only correct thing a cursor-following viewport can do, since the caret would otherwise sit
off-screen to the left.

**But the code has moved on, and this paragraph had to as well.** §6's horizontal-scrolling slice
found the per-line clamp was two bugs from one wrong reference: a caret parked on `{` gives a
maximum of zero, so any trackpad scroll snapped home on the next frame; and when the cursor's line
scrolled out of view the clamp never ran at all, so the text could be pushed arbitrarily into
blank space. It bounds against the **document's** widest line now — the option this paragraph
called "the rejected per-document clamp" — while the caret-following behaviour it defends is
unchanged, because prepaint only overrides the offset when the caret has actually moved.

So the entry has now been wrong in both directions: first asserting a defect the code never had,
then defending a mechanism the code had replaced. The heuristic that resolved it the first time —
trust the section that names a rejected alternative — is what made it *durable enough to go stale*,
which is the failure mode this file's §13 is otherwise about.

Worth recording as its own failure mode, because it is the mirror of the one this project already
tracks. The lessons in `CLAUDE.md` are about **code drifting away from a correct comment**. This was
a **doc asserting a bug the code never had** — and it is the more expensive direction, because a
confident "known defect, it's just wrong" sends the next reader hunting for something that isn't
there, and reads as licence to "fix" working code. When two sections disagree, the one that names a
rejected alternative is usually the one that was written while looking at the problem.

The counts in this section describe M1 as shipped and are deliberately not updated as work
continues; `CLAUDE.md` carries the live test count.
