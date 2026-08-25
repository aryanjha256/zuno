# Zuno — Roadmap

**This document owns *order*.** `architecture.md` owns design and records what was tried;
`CLAUDE.md` owns mechanics. When they disagree with this file, they win — a roadmap is the
document most likely to rot, so treat it as disposable and rewrite sections rather than patching
them.

No dates. Detail decays with distance on purpose: the next phase is concrete, the one after is
directional, and anything beyond that is a name and a reason.

---

## Where we are

**M1, M2 and M3 are all complete**, and §11 of `architecture.md` — the list of engine capability
with no way to reach it — is empty. This section said "what's left is reuse, see M3" for a while
after M3 was finished; rewritten rather than patched, per the note at the top of this file.

- **M1 — the loop.** Author a request, send it over real HTTP with streaming progress and
  cancellation, read the response through a virtualized JSON viewer, diff it against the previous
  run, come back to it after a restart. Plus curl import, themes, window chrome.
- **M2 — navigation.** Tabs as editor buffers, collections as one-file-per-request in git,
  `Ctrl+P` over buffers and saved requests, `Ctrl+K` over every command.
- **M3 — reuse.** Environments and `{{variables}}`, per-request settings, the history browser,
  response egress, and all four body types authorable.
- **Since, from the audit below.** Response search, and the response pane split into body and
  headers tabs.

Measured (release): **189 ms** cold start, **48 ms** to flatten 10 MB of JSON into 1.31 M rows
off-thread, **6.9 ms** to search that body end to end, 60 fps scrolling at any size.

**So what is the frontier?** Not a milestone — the audit's item 4. The loop is excellent, the
navigation thesis is built, and nothing about using Zuno for real REST work is *blocked*. What
remains is a short list of asymmetries and conveniences, and then the two named-not-planned items
(scripting, request chaining) that would decide the product's ceiling. Read the audit, not the
milestone headings.

> **Test counts, once and not repeated.** `CLAUDE.md` carries the live total. Where a number appears
> below it describes that milestone as shipped and is deliberately not updated — the same rule
> architecture.md §13 states. Two of them had drifted into reading as current before this note
> existed.

---

## Four sequencing principles

These decide phase order, and they're the durable part of this document.

1. **Navigation needs something to navigate.** `Ctrl+P` over a single scratch request is theatre.
   Collections come before the palette that searches them — which is also why curl import shipped
   first: it's how requests get *into* the app at all.

2. **Build the picker once.** `Ctrl+P`, `Ctrl+K`, the method dropdown, and the environment
   switcher are the same interaction: an anchored overlay, a filter input, a fuzzy-scored list,
   keyboard selection. Built deliberately once, four features become cheap; built ad hoc, it gets
   written four times and feels different each time. **This is the highest-leverage piece of UI
   work remaining.**

3. **Prefer work that exposes capability already built.** §11 of `architecture.md` tracked engine
   capabilities honoured on every request with no way to reach them. `Ctrl+,` closed five of the
   original nine in one modal and the method picker closed a sixth as a side effect, which is the
   ratio this principle is about — one modal for five features.

   **§11 is now empty, and this text said "Three remain" long after all three landed.** The
   principle outlives its list: prefer the work where the engine already does the thing and only
   the UI is missing, because that ratio is unbeatable. §11 is also the wrong place to *look* for
   such work now — by construction it can only name gaps where the engine was involved, and the
   two largest found since (nothing could copy a response; the headers table hid the body) appear
   nowhere in it.

4. **Defer the expensive and isolated.** Syntax highlighting needs tree-sitter plus a highlight
   cache, touches nothing else, and improves nothing structural. It is the most expensive thing
   left and the least load-bearing, so it goes last regardless of how much it would flatter a
   screenshot.

---

## M2 — Navigation — **complete**

The thesis milestone. Kept in full rather than trimmed to a line: the *order* these landed in, and
the two estimates that turned out wrong, are the durable part.

**Tabs — done.** What makes the original brief's "20–100+ open requests without the UI turning
into chaos" true.

Built in two slices, in that order for a reason. First the *session format*: a versioned envelope
in `app/src/session.rs` that still reads M1's single-spec file, because persistence was the only
part of tabs that could silently destroy work — the quit hook saved `active()` alone, so a strip
landing first would have dropped every other open request on exit. Then the verbs and the strip:
`NewTab`/`CloseTab`/`NextTab`/`PrevTab` on `ctrl-t`/`ctrl-w`/`ctrl-tab`/`ctrl-shift-tab`, a strip
that hides itself at one buffer, click and middle-click, and curl import opening a new buffer
instead of replacing the active one.

Worth correcting an earlier version of this file: "only the strip and the switching are missing"
was wrong. It counted the `Vec<Entity<RequestView>>` field as readiness and missed both that
persistence was single-buffer and that switching needs focus to travel with it — a `FocusHandle`
belongs to its creating entity, so a switch that only moves `active_ix` leaves the keymap dead.

*Left over, deliberately:* no reordering, no rename (tab labels derive from the URL — see
`label_for`), and `dirty` still unanswered until collections give it a baseline.

**Collections — the format is done.** §12's persistence decision is settled: a directory of
one-request-per-file JSON (`core/src/collection.rs`), because a collection you can commit and
review in a pull request is a real differentiator and that needs one file per request. Ctrl+S
writes the active buffer; filenames derive from the URL, collisions get a suffix rather than
overwriting, and `RequestView::path` — persisted through a v2 session envelope — is what makes a
second save overwrite its own file instead of breeding `posts-2.json`.

*What's missing is reach, not format:* **nothing opens a saved request back into a buffer.** That's
the picker's job by principle 2, so it waits rather than getting a throwaway list UI. Until then a
saved request is only reachable while its tab is open — worth knowing, since it makes the picker
the next thing that has to land. Folder authoring is also absent; `mkdir` works.

**The picker primitive — done.** Principle 2's one build: `app/src/picker.rs` is a centred modal
with a filter input, a fuzzy-ranked `uniform_list`, and a `Target` it hands back without
interpreting. Deliberately *not* a `PickerDelegate` trait — a new consumer is a new `Target`
variant, not a rewrite. (This said "one consumer" for a long time after there were seven. The
count was never the trigger: the trait earns its keep at a consumer wanting different *rendering*,
and all seven render identically. See architecture.md §12.)

Matching is hand-rolled in `core/src/fuzzy.rs` rather than taking `nucleo`: hundreds of requests and
a couple of dozen actions is not a scale where a real matcher earns its complexity, and pure code in
core unit-tests without a window. It's greedy, so it doesn't always find the tightest alignment —
documented, and it never fails to match something a human would call a match.

**`Ctrl+P` — done.** Open buffers first, then saved requests from `collection::scan`. Buffers first
because for a handful of tabs it makes Ctrl+P a tab switcher, so it's useful from the first press
rather than only once a collection has grown. A request already open is listed once, as the buffer.
Choosing a file sets its `path`, so Ctrl+S afterwards overwrites instead of duplicating.

This also closed the one-way door the collections slice left behind: Ctrl+S wrote files nothing
could read back.

**`Ctrl+K` — done.** The same picker over `commands::palette()`, each row showing its keybinding
read live from the keymap so a rebinding can't leave the palette advertising a dead shortcut.

The estimate in an earlier version of this file was wrong, and it's worth recording why:
"`actions.rs` already lists the verbs" treated a palette as a loop over `all_action_names()`. It
isn't. That returns namespaced strings for *every* registered action — the twenty-odd
`text_input::`/`editor::` ones included — with no labels. "Backspace" is a keystroke, not a command.
So `commands.rs` is a curated table holding real action *values* rather than name strings, which
makes renaming an action a compile error instead of a silently dead row, and a drift test requires
every `zuno::` action to be either offered or explicitly excluded with a reason.

> **Done when:** you can hold 50 requests open across collections, reach any of them by name
> without touching the mouse, run any command from the palette, and nothing about it feels slower
> than the single-request loop does today.

---

## M3 — Reuse

The theme is *stop retyping things*. Started out of order on purpose: the settings panel came first
because principle 3 outranked the listed sequence — it exposed five already-built capabilities for
one modal, and the cookie jar was silently making consecutive requests non-independent.

**Environments and variables — done.** `Ctrl+E` selects one; `{{name}}` is substituted into the
URL, query rows, headers, and every body a variable can appear in — raw text, form field names and
values, and multipart text parts — on the way to the socket, while the stored request keeps its
placeholders. Two layers: `globals.json` always active, one selected environment on top.
Request-level variables were considered and dropped — an editable table per request, for the layer
least likely to be used.

Four decisions worth keeping:

- **Environments live in the collection**, in a reserved `environments/` directory, so they travel
  with the requests they describe and are reviewable in a PR — the same argument as the collection
  format itself.
- **Secrets are a file split, not a flag.** `dev.json` is committed, `dev.local.json` is gitignored
  and overrides it. The split *is* the marking, so there's no per-variable flag to forget. Zuno
  writes the `.gitignore` rule itself, but only when a selected environment actually has secrets,
  and it says so — the collection format exists to be committed, so "document it and hope" leaks
  tokens by default.
- **Substitution is single-pass and replaces only known names.** An unknown `{{foo}}` is left
  verbatim: in a URL or header that trips the pre-existing `UnresolvedVariable` check by name
  before DNS, and in a JSON body it passes straight through — which is why no escape syntax was
  needed. No recursion, so cycles are impossible by construction rather than detected.
- **Values are re-read per send, not cached at switch**, so editing `dev.json` in an editor takes
  effect on the next request. The files are the interface, so they stay authoritative.

*Deliberately absent:* no in-app environment editor. Environments are JSON files in your
collection — the same bet the collection format makes; an editing UI is its own slice.

A gap turned up on the way: `build.rs` validated the URL and headers but **not query rows**, so an
unsubstituted `{{var}}` in a query parameter reached the wire literally. Fixed, with a test.

**The same gap, found again by audit, in bodies.** `Resolver::apply` substituted `Body::Raw` and
nothing else, so once form and multipart became authorable (2b and 2d below) their field values went
out verbatim. Worse than the query-row case: `build.rs` deliberately never scans a body for `{{…}}`
because `{{` is legal in JSON, so there was no error either — and a client-credentials token
request, the motivating case for request chaining, is a *form* body, so it sent the literal string
`{{secret}}`. `apply`'s body match is now exhaustive with no catch-all, the same discipline
`RequestView::load` already uses: a new `Body` variant fails the build until someone decides whether
a variable belongs in it.

**Auth helpers — dropped, not deferred.** Recorded so nobody rebuilds it because the roadmap once
said to. Environments made it redundant, and a dedicated auth tab would now be actively *worse*:

- **Bearer and API keys** are `Authorization: Bearer {{token}}` with the token in `dev.local.json` —
  per-environment and gitignored by construction. A Postman-style auth tab adds a mode with no new
  capability, and it writes the credential into the *committed* request file, which is precisely the
  leak the environment split exists to prevent.
- **Basic** is the one genuine gap, and it points somewhere else. `core/src/curl.rs` has a tested
  `base64`, so *importing* `-u user:pass` works; authoring it from scratch doesn't, because nothing
  in the UI can encode. But the encoded value belongs in a `.local` file, not in a request header —
  so the useful thing is "hand me the credential to paste", not an auth tab. ~30 lines as a palette
  command over the picker's fallback row, whenever it's wanted.
- **OAuth is not an auth helper.** Client-credentials flow is: send a token request, extract a value
  from the response, use it in the next request. That's **request chaining** below, and it's the
  motivating case for it.

**Settings panel — done**, pulled forward ahead of environments. `Ctrl+,` surfaces the five §11
capabilities that were honoured on every request with no way to see them: cookie jar, timeout,
redirects and hop limit, TLS verification, encodings. (This file used to say six. Counting them,
it's five — the rest need their own UI, not a toggle.)

Two things it turned out not to be:

- **Not pure UI.** The cookie jar is shared per client-config across the whole process, so toggling
  cookies off routes through a different cached client rather than emptying a jar — and toggling
  back restores it. A toggle alone would have shipped the confusion it was meant to remove, so
  `Engine::clear_cookies` landed with it.
- **Not a global settings screen.** These are per-request, because `RequestSettings` already lives
  on `RequestSpec` and persists per collection file. Global defaults need the same
  global → environment → request scope model that environments has to build below; building a
  second one here would mean discarding one.

The status bar now carries a `cookies on` badge. That's the half that actually saves the hour: the
toggle says what will happen, the badge says what *is* happening.

**History browser — done.** `Ctrl+H` lists every retained run — live first, then "1 send ago" and
back — with each row carrying its status, size and duration, because "which run was the 500?" is the
question you open it to answer. Choosing one shows it and re-indexes its body off-thread.

It closed the last non-body item in §11, and it was more than a feature: `history` was written and
read by nothing at all, so ten response bodies per buffer were being retained where nothing could
reach them. Surfacing it is what makes that memory worth spending.

Three details that keep it from misleading:

- The pane says **"Showing the run from N sends ago"** when you aren't on the live response.
  Without it the pane is indistinguishable from the current run.
- The **diff is hidden** while browsing, because it describes live-vs-previous and is simply wrong
  next to an older run.
- **Sending returns you to live.** A response arriving while you read an old one must not leave you
  parked in the past with no sign anything happened.

---

## M4 — Editing

**Syntax highlighting** in the request body and the JSON viewer. Principle 4. Theme tokens
(`SyntaxTheme`) were defined back in M1.0 so the palette wouldn't have to be invented under
pressure.

**Method dropdown — done**, pulled forward from M4 because it really was cheap once the picker
existed. `Ctrl+M` opens the picker over the seven common verbs with the active one marked, replacing
cycling (which needed seven presses to reach OPTIONS).

Two things worth recording:

- **It closed a §11 item nobody costed.** Because the picker has a filter input, typing an unknown
  verb offers it as `Method::Other` — so custom HTTP methods went from "sendable but unreachable" to
  reachable, for about twenty lines. Validated against RFC 9110's `tchar` set so a verb that the
  engine would reject with `InvalidMethod` is never offered in the first place.
- **"Free once the picker exists" was right, but a note in `picker.rs` was wrong.** That note said
  the method dropdown would want `anchored()` positioning. It doesn't: anchoring needs the button's
  screen bounds, and a centred picker is better here anyway — one interaction idiom, keyboard-first.
  Corrected in place.

---

## What's actually missing — an audit

Taken after environments and history landed, when the remaining §11 items looked like the whole
story. They weren't. Ordered by how much each one blocks *using Zuno for real REST work*, which is
not the same as how much code each needs.

**1. Response egress — done.** `Ctrl+Shift+C` copies the displayed body; `Ctrl+Shift+S` saves it to
a file through the native picker. Both read `displayed()`, so they follow the history browser rather
than always grabbing the live run.

Three decisions worth keeping:

- **Copy gives the raw bytes, not the pretty-printed outline on screen.** What you paste into a
  fixture or a bug report has to be what came back; reformatting would quietly change the thing
  you're reporting.
- **Copy is text-only and says so.** A body that isn't valid UTF-8 is normal (invariant 4) and the
  clipboard needs a `String`, so a binary response points at Save rather than copying mojibake.
  That's *why* Save is a separate verb and not a duplicate of Copy — it's also how a multi-megabyte
  body gets out.
- **The suggested filename runs through `collection::slug`**, for the same reason saving a request
  does: it derives from the URL, so `https://x.test/../../.ssh/config` must not become a path. The
  extension comes from the content type, ignoring parameters, and defaults to `.bin` because an
  unknown type shouldn't claim to be text.

*Still missing from egress:* copying a single JSON row's value or its path. That needs a selected
row in the response viewer, which doesn't exist — the pane has focus but no cursor — so it's row
selection first, then copy. Worth doing alongside response search, which wants the same thing.

**2. Body authoring — done** (2a–2d below). As found, this was the real capability blocker: no
workaround for multipart or binary, and file upload is bread-and-butter REST. Multipart was the only
part needing *engine* work — `UnsupportedBody` plus reqwest's `multipart` feature. Form turned out
**not** to be blocked at all: an explicit `Content-Type` header beats the derived one, so `a=1&b=2`
as a raw body already worked, which is why it ranked as convenience rather than capability.

`cx.prompt_for_paths` in gpui 0.2.2 de-risked the file selection, so neither binary nor multipart
needed hand-typed paths.

**2a. Non-raw bodies are no longer destroyed — done, and it was a bug rather than a gap.** `spec()`
derives the body from the editor, the editor only holds raw text, so loading a request with a form
body produced an *empty* editor and the next Ctrl+S wrote that emptiness over the real body.
Reachable since M1, because curl import has always parsed `-F` and `--data-binary @file` into
exactly those variants. `RequestView::preserved_body` now holds what the editor can't express, the
pane says what it's holding instead of showing a misleading empty editor, and `Ctrl+Shift+B` explains
itself rather than being a dead keystroke.

Two things this settles for the authoring work: **where** non-raw state lives (one field, disjoint
from the editor), and that the round trip is already covered by tests — so form, binary, and
multipart become "add an editor" rather than "add an editor and fix persistence at the same time".

**2b. Form authoring — done.** `Ctrl+Shift+B` opens a body-type picker (None / JSON / Form / Text /
XML / HTML), replacing the cycling that walked `RawKind` and so could never reach a form at all.
`Ctrl+Shift+F` adds a field, switching the body to a form first if it isn't one — which is what the
keystroke plainly means. Fields reuse `KeyValueRow`, so `enabled` toggling and row removal came free.

Multipart and binary are deliberately **absent from the picker** until their editors exist: offering
a type nothing can author is worse than not offering it.

> **"Replacing the cycling" was only half true until an audit caught it.** The *keystroke* moved to
> the picker; the body-kind **chip in the pane kept cycling `RawKind`** by calling the view directly.
> So the click and the keybinding were different verbs — which is what "actions, not direct calls" is
> there to prevent — and on a Form, Binary, or Multipart body the chip mutated `body_kind` under a
> label that couldn't show it, making a real control look dead. The chip now dispatches
> `OpenBodyType`, `cycle_body_kind` is gone, and a test clicks the chip rather than trusting it.

Two bugs surfaced while building it, both found by a test asserting on bytes a server actually
received rather than on the spec:

- **A stale `Content-Type` header silently outranked the body.** `build.rs` derives a Content-Type
  only when no explicit header is set, so switching the sample request to a form sent a urlencoded
  body *declaring itself JSON* — which a server rejects or misparses. Now reported at the moment the
  type is chosen, naming both the header and what was expected. Reported rather than rewritten:
  editing someone's headers behind their back is worse than telling them.
- **Choosing "None" still sent the editor's text.** `body()` fell through to the editor for both
  `Empty` and `Raw`, so the setting looked applied and wasn't. `Empty` is now unconditional, and the
  pane shows "No body" instead of an editor whose contents don't get sent.

Switching type turned out to be **lossless for anything still visible** — the editor's text and the
form rows are both kept, so JSON → Form → JSON round-trips. Only `preserved_body` is dropped, because
it can't be rendered or re-derived and holding it alongside a chosen type would be invisible state.

**2c. Binary authoring — done.** `Ctrl+Shift+O` opens the native file dialog and switches the body
type to match, the same "the keystroke plainly means this" shape as `Ctrl+Shift+F`. Clicking the path
in the pane reopens the dialog, since there's nothing else in that region to click.

**Only the path is held, never the bytes.** `build.rs` reads the file at the send boundary, so a file
edited between sends goes out in its new state and a 2GB upload never enters this process's memory. A
file that has since disappeared surfaces as `BodyFileUnreadable` — checked at send rather than at
selection, because checking in the pane would mean a filesystem call on every frame.

The pane also says **"no Content-Type is sent unless you add the header"**, because `build.rs`
deliberately guesses nothing for binary uploads — that's correct, and invisible otherwise.

**2d. Multipart authoring — done, and §11 is now empty.** `Ctrl+Shift+M` adds a part;
`Ctrl+Shift+O` attaches a file to the *focused* part, or sets the whole binary body when no part has
focus — one verb, two meanings decided by where you are, rather than two keystrokes for the same
intent. Doing it last paid off as predicted: the UI is form's field table plus binary's picker, so it
was composition rather than invention.

On the engine side, reqwest's `multipart` feature is enabled and `UnsupportedBody` is gone.
`build_body` reduces parts to plain `PreparedPart` values rather than building a
`reqwest::multipart::Form`: that type is neither `Debug`, `Clone`, nor `PartialEq`, so holding one in
`PreparedBody` would have cost the enum its derives and made multipart the only body untestable in a
unit test. Reqwest stays confined to `build`, and file reading stays beside every other body's.

**Unlike every other body, an explicit `Content-Type` cannot win here** — `multipart` generates the
boundary and writes the header itself, and a user-supplied `multipart/form-data` without that
boundary is unparseable. Verified over a real socket: boundary in the header *and* delimiting the
parts, a text part, and a file part carrying its filename (without which many frameworks read an
upload as a plain text field).

**And it let `preserved_body` go.** Every `Body` variant now has an editor, so `load` matches
exhaustively with no catch-all: adding a variant is a compile error until someone decides how to edit
it, which is stronger than silently holding the unknown. The compiler forced it — once multipart was
authorable the catch-all became unreachable and `-D warnings` rejected it.

**3. Search in a response — done.** `Ctrl+F`, `Enter`/`Shift+Enter` to step, `Escape` to close.
This was where the "huge JSON" claim got tested, and it held: a full-body miss over 10MB scans in
**6.9 ms**, and the offset-to-row mapping in **148 µs**. Both are asserted in `json_perf`, so a
regression fails rather than merely feeling slow.

"Map hits back to rows" was the right guess about where the work would be, and it was harder than
the sentence implies — a row's source position isn't stored anywhere, and the first reconstruction
was wrong by exactly the nesting depth. See architecture.md §6.

Two things it turned out to need that weren't on this list:

- **`TextInput` had to start emitting a `Changed` event.** Incremental search means re-scanning per
  edit, and the picker's trick of comparing the query in `render` doesn't extend to spawning a
  background task. The picker moved onto the event too, so there's one mechanism.
- **The find bar's `Escape` had to be registered after the global one.** Third time this ordering
  rule has decided behaviour with no compile error to catch it — now with a test that fails when
  the block moves.

*Still missing from egress, and unchanged:* copying a single row's value or its path. Search built
the row *cursor* (a current match, revealed and scrolled to) but not a *selection* the user drives,
so that remains row selection first, then copy.

**4. Smaller, but real.** With search done, this is the live list.

- **No copy-as-curl.** Import exists and export doesn't, which is asymmetric — and "here's the
  repro" is a constant need. Nearly pure `zuno-core`, so it's the cheapest real thing left.
- **No delete or rename of a saved request** from inside the app. Mild, because files are the
  interface and `rm` works, but it means the collection is read-mostly from Zuno's side.
- **No body prettify.** Paste minified JSON and you live with it.

**5. Layout, and it took a screenshot to find — done.** Unplanned, and worth recording because
of *how* it was found: two screenshots of the running app, not a test and not a read of the code.

- **The response pane hid its own body.** Headers were rendered inline above it, unbounded, in a
  pane that clips and never scrolls, so a Cloudflare-fronted response's two dozen headers pushed
  the body off the bottom edge with no way to reach it. `Body` and `Headers` are now tabs
  (`Alt+R`), Body default. See architecture.md §6.
- **The tab strip painted the wrong borders.** A div carries one `border_color` for all four
  sides while widths are per-side, so the active tab's accent overwrote the neutral divider: the
  active tab drew a stray accent edge on the right, and every inactive tab drew its divider in
  the panel's own colour — invisibly. The tabs ran together for several milestones. Now a nested
  element, since two colours need two boxes.

The pattern is the one this file's audit section already warns about from the other direction:
§11 tracks *unreachable engine capability*, and neither of these is that. Both were plainly
visible to anyone who opened the window, and invisible to a test suite that asserts on state
rather than on pixels. Worth remembering the next time the counts and the green suite feel like
coverage.

**Syntax highlighting stays last** regardless (principle 4): expensive, isolated, and it improves
nothing structural — it would only flatter a screenshot.

---

## Named, not planned

Reasons recorded so a future session can judge them, not commitments.

- **Scripting** (pre-request / post-response). The largest single feature in the original
  original brief, and the one most likely to define the product's ceiling. Needs a language and a
  sandbox decision before anything else.
- **Request chaining** — extract a value from one response, feed it to the next. Arguably more
  valuable than general scripting and far smaller. **This is where OAuth lives**: a
  client-credentials flow is exactly "POST for a token, then use it", and building it here rather
  than as an auth feature means every other token-then-call API gets it too.
- **Client certificates.** `RequestSettings` has room; reqwest supports it.
- **Inline body diff.** The summary diff answers "did my change do anything?". A structural diff
  over `Row` spans is probably better than a text diff, now that the JSON outline exists.
- **gRPC / WebSocket / SSE.** Each is a different transport and a different response viewer. Not
  extensions of the HTTP loop — separate products wearing the same coat.
- **macOS and Windows builds.** Keybindings assume `ctrl`; `session.rs` assumes XDG paths. Both
  are marked in code.

---

## Non-goals

Saying no is what keeps the thesis from being eaten. None of these are ruled out forever; all of
them would change what Zuno *is*.

- **Team collaboration and cloud sync.** Local-first is a stated principle, not a limitation.
  This is also where every competitor's business model lives, and following them there means
  competing on the wrong axis.
- **Mock servers, load testing, contract testing, API documentation.** Adjacent products.
  Postman's decline into a platform is the cautionary tale.
- **A plugin ecosystem.** Not before the core loop is something people prefer.
- **Beating Postman on feature count.** The bet is feel. From the original brief: *"the first milestone
  shouldn't be build Postman"* — that stays true at every milestone.

---

## How to use this file

Start a milestone by re-reading the four principles, not the feature lists. If a phase's contents
no longer make sense, the principles are what tell you the new right answer — rewrite the phase.

Before adding anything, check it against the non-goals, and check `architecture.md` §11 to see
whether it's already built and merely unreachable.
