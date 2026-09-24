# Zuno — Roadmap

**This document owns *order*.** `architecture.md` owns design and records what was tried;
`CLAUDE.md` owns mechanics. When they disagree with this file, they win — a roadmap is the
document most likely to rot, so treat it as disposable and rewrite sections rather than patching
them.

No dates. Detail decays with distance on purpose: the next phase is concrete, the one after is
directional, and anything beyond that is a name and a reason.

---

## Where we are

**M1, M2 and M3 are all complete.** §11 of `architecture.md` — the list of engine capability
with no way to reach it — was empty and has one entry again: the engine sends any WebSocket frame
type and the composer can only ask for text. This section said "what's left is reuse, see M3" for
a while after M3 was finished; rewritten rather than patched, per the note at the top of this
file.

- **M1 — the loop.** Author a request, send it over real HTTP with streaming progress and
  cancellation, read the response through a virtualized JSON viewer, diff it against the previous
  run, come back to it after a restart. Plus curl import, themes, window chrome.
- **M2 — navigation.** Tabs as editor buffers, collections as one-file-per-request in git,
  `Ctrl+P` over buffers and saved requests, `Ctrl+K` over every command.
- **M3 — reuse.** Environments and `{{variables}}`, per-request settings, the history browser,
  response egress, and all four body types authorable.
- **Since, from the audit below.** Response search; the response pane split into body and headers
  tabs; the request pane split into Headers / Params / Body; the editing set the text inputs were
  missing (word movement and deletion, document ends, double- and triple-click, `PageUp`/`PageDown`,
  undo/redo); row selection in the response viewer with copy-value and copy-path; a right-click
  context menu on response rows, built as a reusable primitive; horizontal scrolling, which had
  been missing from every body surface; and syntax highlighting for JSON in the request editor and
  the raw response view — plus per-character search highlighting in the raw view and the JSON
  outline; and find-and-replace in the request body, which is what made `Ctrl+F` mean something
  everywhere rather than everywhere except the surface you type into; the **collection panel** —
  a browsable tree of what you have saved, which until then nothing in the app could show you.
  The **timing timeline**, a third response tab breaking a request into DNS, connect + TLS,
  waiting and download along one time axis — the first item on that audit where the engine, not
  the UI, was the half that was missing. Then the **proxy** and **certificates**, which between
  them are what makes Zuno usable on a corporate network at all; and a **tab context menu**,
  which took the batch-close prompt with it. Then **distribution**, which is the first item in
  this list that is not about the app at all: a single `curl | sh` that installs *and* updates
  (one command, because `apt-get install ./file.deb` is already both), the checksums and the two
  container smoke tests that keep it honest, and an update **notice** in the titlebar that copies
  that command. Deliberately not an updater — see architecture.md §6k, where the password prompt
  turns out to be a consequence of a *window* asking for root rather than a requirement. And a
  **header-name dropdown** — a combobox under the cell you are typing in, which is the first
  *inline* overlay in the app and the discoverability rule one level below `affordances()`: that
  table proves every action has a mouse path, and says nothing about someone who cannot name the
  header they need to type. See architecture.md §6l.

  Adding to this list rather than leaving it is deliberate: the paragraph below is about this
  exact list going stale, and a slice that updates architecture.md and skips the file owning
  *order* is how that happens.

  This list had gone two slices stale — the request-pane tabs and the editing set were both shipped
  and both absent from it — which is the rot the note at the top of this file predicts. Worth
  noticing *how*: each of those slices updated architecture.md, where its design decisions belong,
  and neither updated the file that owns **order**. A doc nobody has to touch to finish a slice is
  the one that silently stops being true.

Measured (release): **189 ms** cold start, **48 ms** to flatten 10 MB of JSON into 1.31 M rows
off-thread, **6.9 ms** to search that body end to end, 60 fps scrolling at any size.

**So what is the frontier?** Not a milestone, and no longer "a short list of conveniences" —
that framing survived two slices past being true. The loop is excellent and the navigation thesis
is built, but the audit's own item 4 now names two structural gaps that outrank everything else on
it, and **both were invisible to the audit because it only ever looked inward**:

1. **Workspaces — done.** A registry in `app.json`, one session per workspace, and New / Open
   existing / Switch / Forget. The git argument this whole format is built on — commit your
   requests, review them in a pull request — is reachable now, because a workspace can live
   inside the repo it describes rather than only in `~/.local/share`. See architecture.md §12.
2. **Folder verbs — done.** A folder can be made, renamed, trashed and deleted, and its
   right-click menu offers everything that works on a directory. Still absent: duplicating or
   moving a *folder*, and `Ctrl+S` cannot target one. See architecture.md §6a.

What is left after those two: **scripting**, which is the one item that would decide the ceiling
and the one still blocked on a decision rather than on work — it needs a language and a sandbox
chosen before anything else. **GraphQL** was the last of the three capabilities item 4 found by
comparing Zuno against what an API client is expected to do, and it has since shipped along with
the other two. Read the audit, not the milestone headings.

**GraphQL — done, as a request *kind*.** §6f used to argue half of it away: GraphQL over HTTP
is a JSON body, so nothing was missing to *send* one. What was missing was authoring, and that
turned out to need a model change rather than a body variant.

It shipped in two slices, in that order deliberately — **a format change and a feature must
never share a slice.** First the spine/kind split alone: `RequestSpec` holds what is true for
every protocol and `RequestKind` holds what isn't, with no behaviour change and a serde shim
that reads every collection file ever written. Then `RequestKind::GraphQl` on top of it: query
and variables editors, the envelope, `{{var}}` substitution, curl export, and an exact Postman
mapping. A kind chip beside the method switches between them, asking first when that discards
work. See architecture.md §3.1.

Putting the format change in its own slice is what made its one real bug — an older Zuno
overwriting a session it could not read — have exactly one candidate.

**Two exports, for two audiences — done.** *Export…* on any folder asks which: a **Zuno
bundle** (one file, nothing lost) or a **Postman collection** (v2.1, for other tools).

The bundle exists because Postman export is lossy by necessity — it has no home for per-request
settings, captures, assertions or `expect_status`, which is exactly what Zuno adds. Sending a
collection to another *Zuno* should not go through a format that drops them. It is an
**envelope, not a second schema**: each request is its existing `RequestSpec` serialization
embedded verbatim, so a new kind travels for free and there is no second description of a
request to keep in step with the model. Import goes through the same sniff as every other
document, so `Ctrl+Shift+I` reads one with no new verb.

It carries a `version` where collection files deliberately do not (invariant 9): a bundle is
written once and opened *somewhere else, possibly by an older build*, which is the one case
where "written by a newer Zuno" beats a cryptic parse error. Environments travel as the
**committed half only** — `dev.local.json` is gitignored because it holds secrets, and a bundle
is a thing you send someone.

**Postman export — done, and it closes an asymmetry.** Zuno read curl, OpenAPI and Postman and
could write only a single curl command: easy to get in, impossible to get out. Right-click any
folder → *Export as Postman…* writes a v2.1 collection, at any depth, because `collection::scan`
takes a root and the export is the same code with a different one.

**OpenAPI export was considered and rejected.** OpenAPI describes an *API*; a collection holds
*example requests*. Emitting one means inventing parameter types, body schemas and response
definitions a collection does not contain — a document that looks authoritative and is guessed.
Postman's format holds what Zuno holds, so the mapping is real. Recorded so nobody builds it
because the roadmap once implied symmetry with the importers.

Settings, captures and assertions are **reported by name**, not dropped in silence — Postman has
no per-request home for them. The save dialog opens in `$HOME` rather than the collection root,
which is not only convenience: `scan` walks every `.json` under the root, so an export saved
beside its own requests is read back as one and fails on every scan afterwards.

**Known and deliberately left, because GraphQL is not yet used much.** Written down rather than
carried in someone's head — the failure this file's own header predicts:

- **A `subscription` gets no guidance.** Against a **graphql-sse** server it works, badly: the
  whole stream is buffered and shown as flat text only once it ends, and an unbounded one dies at
  the request timeout instead. Against a server that does not speak SSE it errors obscurely. A
  leading-keyword check could say "subscriptions need a streaming transport" — see the SSE note
  under *Named, not planned*, which is the cheaper half of the WebSocket work.
- **`graphql_envelope` is built twice on a GET** — once in `build_graphql`, once inside
  `graphql_url`. Threading a prebuilt envelope through a function curl export also calls, to save
  one small JSON build on GET-only requests, was not worth the churn.
- **`RequestTab::for_kind` allocates a `Vec` per render** of the tab strip. A stack-allocated
  vector means a new dependency; an iterator complicates the call site. Five elements in a path
  that already builds an element tree.

What remains for GraphQL, in order: **introspection and assisted editing** — the *builder*, and
the actual differentiator: a query you write with the schema helping, not a checkbox tree. Then a
**schema browser**. That last one is
where the **buffer generalization** comes in — a schema browser is not a request, so it is the
first thing that needs a tab holding something other than one. It is a tab, not a modal: the app
currently has exactly one document type and around ten overlays, and making the browser overlay
eleven is the mistake this note exists to prevent.

**Subscriptions are out of scope and stay out**, not by oversight: a subscription is a session
over WebSocket, which is the transport this file already files under *Named, not planned*. It
needs no new kind — see §3.1 on why lifecycle is a property of a run rather than of a saved
request — so nothing here blocks it later.

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
that hides itself at one buffer, click, middle-click and a per-tab close button, and curl
import opening a new buffer
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

**The editor — done**, and it closes the last place that bet was still being paid. `Ctrl+Alt+E`,
or "Edit environments…" at the foot of the switcher, opens a modal listing the set with each one's
variables; `globals` is in the list, pinned and unrenameable, because it could not be edited from
anywhere either. A per-row lock toggle decides which of the two files a value is written to, which
is the only honest surface for a marking that *is* a file split. Trash rather than delete, for a
sharper reason than the collection panel has: the `.local` half is gitignored, so it is the only
copy of every secret in it anywhere.

The bet it retires was recorded here as *"no in-app environment editor — environments are JSON
files in your collection, the same bet the collection format makes"*. That bet was taken back for
collections when the panel grew folders, rename, delete and import, which left environments as the
last surface where "the files are the interface" still meant leaving Zuno for a text editor.

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

**Request chaining — done**, and it is where OAuth ended up living. A capture rule sits on the
request that *produces* a value: `$.access_token → token`, published into the selected environment
after a successful send, secret by default. Every consumer then needs nothing at all — `{{token}}`
is an ordinary variable the resolver already handled. A client-credentials flow is two requests and
one rule.

Authored from the response itself: select the row, `Alt+Shift+C` or "Capture as variable" in its
menu, and the path comes from `path_to` — the same function behind `Alt+C`, so it is right by
construction rather than typed twice. That path publishes straight away rather than waiting for
another send, since the value is in the outline you just clicked. Listed on a fourth request-pane
tab, because a rule you cannot see is the thing the consumer-side design was rejected for.

The cost, stated plainly: an expired token means re-sending the producer yourself. Re-running it
automatically is a later slice, and one that now has somewhere to live.

**Auth helpers — dropped, not deferred.** Recorded so nobody rebuilds it because the roadmap once
said to. Environments made it redundant, and a dedicated auth tab would now be actively *worse*:

- **Bearer and API keys** are `Authorization: Bearer {{token}}` with the token in `dev.local.json` —
  per-environment and gitignored by construction. A Postman-style auth tab adds a mode with no new
  capability, and it writes the credential into the *committed* request file, which is precisely the
  leak the environment split exists to prevent.
- **Basic** is the one genuine gap, and it points somewhere else. `core/src/curl.rs` has a tested
  `base64` — now shared with the Postman importer, which lowers `basic` auth into the same header —
  so *importing* `-u user:pass` works; authoring it from scratch doesn't, because nothing
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
- ~~**Not a global settings screen.**~~ **It is one now**, and the reasoning above was wrong in a
  useful way. It said global defaults needed the same global → environment → request scope model
  environments has to build. Two of those three were enough: `app.json` holds the set a *new*
  request starts from, the request holds its own, and there is no per-environment layer because
  nothing varies a timeout by environment. Reached by its own trigger — `Ctrl+Shift+,`, or a gear
  in the titlebar beside the theme toggle — rather than a scope row inside `Ctrl+,`: where a gear
  lives is what says what it changes, and mistaking one scope for the other is silent either way.

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
  next to an older run. The Diff *tab* follows the same rule by a different route: a tab cannot
  hide without shifting the three beside it, so it stays and says why it is empty.
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
  extension comes from the content type, ignoring parameters, and falls back to `.bin`.

  **The `.bin` fallback used to be almost the whole behaviour, and that was wrong.** Five media
  types were tabled and everything else — every image, every PDF, every zip — saved as `.bin`, on
  the stated reasoning that an unknown type shouldn't claim to be text. Sound as far as it goes,
  but `image/jpeg` is not unknown: a JPEG saved as `shot.bin` does not open by double-clicking it,
  so "claiming nothing" moved work onto the person instead of avoiding a mistake. The table now
  names the types worth naming and *derives* the extension under `image/`, `audio/`, `video/` and
  `font/`, where the subtype usually is one. `application/*` still gets no derivation — its
  subtypes are mostly not extensions.

  That derivation added a second attacker-controlled string to the filename. `collection::slug`
  guards the label half; a `Content-Type` of `image/../../.ssh/config` would have walked into the
  other, so a derived subtype has to be short and alphanumeric or it is refused.

- **`Content-Disposition` outranks all of it.** A download endpoint that answers
  `attachment; filename="invoices-2026-Q1.xlsx"` has named the file, and inferring
  `api-v1-export.xlsx` from the URL throws that away. The header is used when it names something,
  and the content type only supplies an extension the server omitted — appending unconditionally
  would give `report.csv.csv`.

  **That makes three attacker-controlled strings in one filename**, and this is the most direct of
  them: the server is literally choosing the name. RFC 6266 allows a quoted string, so `filename`
  can legally contain `/`, `..`, NUL or a leading dot. `disposition::filename` reduces it to a
  single path segment — and *degrades* rather than rejects, since a refused header silently falls
  back to the URL label and hides that the server said anything. `../../.ssh/config` becomes
  `config`; a name that is only dots becomes nothing.

*The rest of egress — done, in the slice after search.* Copying a single row's value or its path
needed a selected row, which is why it waited: the pane had focus but no cursor. `up`/`down` and a
click now place one, `Ctrl+C` copies the row's value and `Alt+C` its JSONPath. See
architecture.md §6.

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

*And it built half of what egress was waiting for.* Search produced a row *cursor* — a current
match, revealed and scrolled to — but not a *selection* the user drives. Row selection reused the
reveal-and-translate machinery wholesale, which is why the follow-up slice was small; the two
cursors stayed separate, because a match and where you are standing are different questions.

**4. Smaller, but real.** With search done, this is the live list.

- **Copy-as-curl — done.** `Ctrl+Shift+X`. `curl.rs` holds both directions now, so a round-trip
  test can catch the drift that matters: an exported flag the importer drops.

  It needed one decision that wasn't obvious from the outside. A copied command gets pasted into
  issues and chat, so resolving every variable would leak a live token, and resolving none would
  make the command un-runnable. It resolves everything *except* values from the gitignored
  `.local` file, which stay as `{{token}}`, and says so in the status bar. The secret marking being
  a file split rather than a per-variable flag is what made that free — nothing had to be tagged
  for export to get it right, which is a point in favour of that original choice.

  A test caught a real bug on the way, and not in the export: the "withheld a secret" notice
  scanned disabled rows too, so a fresh buffer announced a redaction for a header it never sent.

  **Then copy as code — curl, fetch, Python, HTTPie, Go, Java, Ruby, C#, PHP, and grpcurl for a
  gRPC call.** `Ctrl+Shift+X` opens a picker of the targets that can express the request, curl
  first. Every target renders one wire description (`codegen::Wire`) built from the engine's own
  functions rather than reading the spec, and that choice is what running the output found out:
  **curl had been exporting two different requests from the one Zuno sent.** A JSON body with no
  typed `Content-Type` went out with none, which curl sends as a form, because the engine
  *derives* that header and the exporter never did; and a binary body was labelled a form the
  same way, fixed with curl's own `-H 'Content-Type:'`. Both passed every exact-text test,
  because the tests agreed with the exporter. `core/tests/codegen.rs` now runs each snippet with
  its real toolchain against a local server and compares what arrived with what Zuno sent — 42
  pairs, one known difference named and asserted (Ruby's `Net::HTTP` labels any typeless body a
  form, and nothing public turns that off). The same run corrected curl *import*: `-H 'Name:'`
  removes a header in curl, measured, and `Name;` is the empty value — the reverse of what a test
  had pinned.
- **Row selection and row-level copy — done.** `up`/`down` or a click place a cursor in the
  response body; `Ctrl+C` copies that row's value (a JSON string arrives *decoded*, a container
  arrives as its own source text) and `Alt+C` copies its JSONPath. This was the last item on the
  egress list above, held back through two slices for want of a selection.

  It also closed a latent version of the picker's width bug: both body row builders sized to their
  own text inside a full-width list, which was merely ugly while the fold chevron was the only
  click target and would have made ~90% of every row dead the moment one was added. The fix and
  the feature were one change, which is the argument for having done them together.

- **Row context menu — done, and it repaired the slice above.** Those two verbs first shipped as
  toolbar labels that appeared *only once a row was selected*, so the mouse path was findable only
  by someone who already knew the keyboard path — the discoverability audit's own finding, one
  level down, one slice after the audit. Right-click is a blind reflex, so it answers without
  being taught. Double-click folds a container, the file-tree convention; the labels were removed.

  Built as `ui`-level primitive `app/src/context_menu.rs` rather than a response-pane feature, on
  principle 2 — and the leverage is already visible, because three of its obvious next consumers
  are items still on this list: **delete/rename a saved request**, tab close/rename, and
  toggle/remove on header rows. It is also the first real consumer of `anchored()`, which
  architecture.md §12 had guessed at twice and placed wrongly both times.
- **Horizontal scrolling — done, and it was a capability gap rather than a convenience.** Found
  by using the app, not by reading it. Soft-wrap is off, so a long line runs off the right edge —
  and in the response body there was no horizontal scroll *and* no cursor to fake one, which made
  anything past the pane width unreachable rather than awkward. The request editor only looked
  better: its offset followed the caret, so `End` reached text a trackpad could not.

  Both bodies and the response headers scroll now, with `left`/`right`/`home` in the response
  pane and a thin auto-hiding indicator.

  **It took two attempts, and the first one shipped broken on every surface with a green suite.**
  Worth recording as an ordering lesson rather than a feature note: the tests asserted that
  something overflowed and that an offset changed, neither of which distinguishes working
  scrolling from a region sized to the wrong row, a scrollbar drawn along the top edge, or an
  editor that snapped back to column zero on the next frame. The headers tab had no test at all.
  architecture.md §6 lists the four traps; the transferable one is that **a green assertion about
  a scroll offset says nothing about whether the content can be reached.**

- **The collection panel — done, and it was the largest gap on this list.** `Ctrl+Shift+E`
  shows a tree of the collection: directories fold, a click or `Enter` opens a request,
  `up`/`down`/`left`/`right` walk it. See architecture.md §6a.

  Found by using the app rather than by reading it, like the layout bugs below — and the
  measurement that named it is worth keeping: **`collection::scan` had exactly one caller in
  the whole app**, the picker. So the only way to look at a saved request was to fuzzy-search
  for it, which needs you to already know its name. "What have I got in here" was a question
  Zuno could not answer at all, which is a *browsing* gap that no amount of ranking closes.

  It is the same blind-spot shape §11 has, one level out again. This audit was taken from
  inside the app — what is built but unreachable, what did I trip over — so a capability that
  was never started casts no shadow in it. Three were found by comparing against what an API
  client is expected to do rather than against what Zuno has: **OpenAPI import**, **GraphQL**,
  and a **collection runner with assertions** appeared nowhere in these documents — not in the
  audit, not in "named, not planned", not in the non-goals. The first of them has since landed.

- **The collection runner — done**, and it is the third of the three capabilities the audit
  found missing from these documents entirely.

  Moved here from *Named, not planned*, where its write-up had been pasted into the middle of
  that section's bullet list — splitting it in two and presenting a
  shipped feature as an unplanned one. A formatting slip rather than a stale claim, and the same
  cost: a reader scanning for what is left found it under the heading that means "not
  committed".

  Assertions live on the request beside its captures — `expect_status` plus a table of
  `path · operator · value`, authored from a response row with `Alt+Shift+A` so the path comes
  from `path_to` rather than being typed twice.

  Two producers feed one loop. `Ctrl+R` runs the folder your panel selection sits in, in filename
  order, which is the smoke test over a feature. `Ctrl+Alt+R` runs a **flow**: a named, ordered list
  of requests in a reserved `flows/` directory, which is the case folder order cannot express —
  a collection is organised by resource and a workflow runs across it. The report fills in as the
  run goes, names what each failure was, and clicking a row opens that request.

  The run loop is entirely in `zuno-core` with no GPUI and no async runtime, which is what the crate
  split was for: it is what `zuno run ./collection` would call.

- **The proxy — done, and it was not a feature request but a correctness problem.** reqwest 0.13
  builds every client with `auto_sys_proxy: true`, so Zuno has routed every request through
  `HTTP_PROXY` since M1.2 — invisibly, with no way to override it. `Ctrl+K` → *Set proxy* now
  picks System / Off / a URL you type, and a status-bar badge names it whenever one is in effect.

  **It is the first item found by reading a dependency's source rather than Zuno's.** That is why
  architecture.md §11 never listed it despite it matching §11's definition exactly: that table
  records capability *Zuno* built and did not surface, and this was inherited from a default
  nobody chose. Same blind spot this audit already admits to, one layer further out.

  App-level in `app.json` rather than per request, which decided three things at once: a URL
  carrying `user:pass@` cannot reach a committed collection file, the settings panel was ruled
  out (it holds no text input, so the picker took it as its ninth consumer), and curl gets no
  `-x` in either direction — for the reason the cookie jar gets no flag either. See
  architecture.md §6i.

- **Certificates — done.** mTLS APIs were uncallable and a private CA could only be reached by
  turning verification off entirely; both are now real, through a panel reached from a permanent
  lock-shaped button in the titlebar. The design note worth keeping is that the two halves are
  *not* the same shape — one identity at a time because a handshake presents one certificate,
  but any number of trusted issuers at once because trust is additive. See architecture.md §6j.

- **Cookie viewer — done, and it fixed the jar underneath it.** Click the `cookies on` badge, or
  *Show stored cookies* in the palette: every live cookie, grouped by domain, with its path,
  expiry and flags; `delete` forgets one, `shift-delete` clears the jar. It could not be built on
  what was there — reqwest's `cookie_store(true)` keeps a private jar per client, so nothing could
  list it, "clear" had to drop every cached client and its connection pool, and **clients are
  cached per settings, so requests with different settings did not share cookies at all**: a login
  at the defaults followed by a request with its own timeout was sent logged out. One engine-owned
  jar (`engine::cookies`, through `cookie_provider`) is readable, clears in place, and is shared by
  every request that stores cookies. Cookies still live only as long as Zuno runs.

- **Tab context menu — done.** Right-click a tab for Close / Close others / Close to the right /
  Close all / Copy as curl. It also closed the last data-loss shape in `Ctrl+W`'s family: closing
  a batch now asks **once** about every unsaved buffer rather than per tab. See §12.

- **The timing timeline — done, and it is the first item here where the *engine* was the
  missing half.** `Alt+R` reaches a third response tab showing where a request's time went: DNS,
  connect + TLS, waiting and download, as contiguous segments on one time axis with a marker at
  first byte.

  **It took two attempts at the drawing**, and the first was rejected on sight for a structural
  reason rather than a cosmetic one: it had no axis. Four bars on four grey tracks, no ticks and
  no elapsed labels, so there was nowhere to read "where did 50 ms fall" — a proportion chart
  wearing a timeline's name. No test could have caught it; every assertion was about arithmetic
  and the arithmetic was correct. Found by opening the window, like §5's layout bugs and the
  picker's dead rows, which makes this the standing category's latest entry rather than a
  surprise.

  Every other entry in this audit is UI work over capability that already existed — that is what
  principle 3 is about and §11 is the record of. This one inverts it. The chart is a few bars and
  some arithmetic; `Timing` had carried `dns`/`connect`/`tls` as `Option`s since M1.2 with
  `run.rs` hardcoding all three to `None`, under a comment saying reqwest could not supply them.
  Half wrong: two `ClientBuilder` hooks are enough, and only splitting TCP from TLS needs the
  custom connector that comment named. So there is no `tls` field now rather than a permanently
  empty one.

  **Found the way three capabilities before it were** — by comparing against what an API client
  is expected to do, not by reading Zuno. "Waterfall", "timeline" and "phase" appeared nowhere in
  any of these three documents, which is the fourth time that comparison has produced something
  this audit could not see from inside. The category is real; it is worth running deliberately
  rather than waiting to trip over the next one.

  The design decision worth keeping is that a **pooled connection is a state and not three
  zeroes**. Clients are cached per `ClientKey` so a resend reuses its socket, which means the
  common case has no lookup and no handshake — and a zero-width DNS bar claims the lookup was
  instant when the truth is it never ran. `Connection` is an enum for that reason, and the pane
  says which. See architecture.md §6h.

- **OpenAPI import — done.** `Ctrl+Shift+I` takes a spec URL or a file path and fills the
  collection: one folder named for the spec, each operation's tag a folder inside it. This is
  the answer to a first run that feels empty, and it is why it came before the project root —
  a project you can choose is worth less than a project with something in it.

  The parser is hand-written over `serde_json::Value` rather than the `openapiv3` crate, which
  covers 3.0 only and says so; the parts Zuno reads did not change in 3.1. See architecture.md
  §6b — that section is also where the first **form modal** is recorded, built concrete for one
  consumer the way the picker was.

  JSON only. Most published specs are YAML and the YAML crate landscape is a graveyard, so that
  is a limitation written down rather than hidden.

- **Postman import — done, and it was the one that mattered most.** `Ctrl+Shift+I` is now
  `ImportDocument` rather than `ImportOpenApi`: the document decides which parser reads it, so
  there is no format to pick. A v2.x export becomes its folder tree, its requests, its auth
  lowered into headers, and an environment named for the collection that is **selected** on
  arrival — an export whose every URL starts `{{baseUrl}}` is otherwise unsendable.

  **Why it jumped the queue.** Everything else in this audit improves the app for someone already
  inside it; this decides who gets inside. Friends of the author agreed to migrate and named the
  migration itself as the obstacle, which is the only kind of feedback that reorders a roadmap.
  It was also cheap for its size: the modal, the fetch-or-read, folder allocation and the
  skipped-notes channel all existed from OpenAPI import, so the slice was one parser and a sniff.

  **Environment and globals exports import too.** A `type: "secret"` value goes to the gitignored
  half — Postman marks its own, so invariant 10's split survives rather than being guessed from a
  name — and a *globals* export lands on Zuno's globals, which is the one place the two models
  agree exactly rather than approximately.

  Postman's `{{var}}` syntax is already Zuno's, which is luck.

  **Test scripts are recovered too**, which is the part that turns an imported collection back
  into a suite: `pm.environment.set` → a capture, a status check → `expect_status`,
  `pm.expect(…).to.eql` → an assertion, following a local variable bound to the response body
  because that is how real scripts are written. A shape with no faithful translation — a numeric
  comparison, truthiness, a computed index, a guarded statement — is **refused and reported
  verbatim**, never approximated: a rule Zuno invented fails a run for a reason that is nowhere
  in the collection. Descriptions have no field to land in and are reported once.

  Still reported rather than recovered, and both deliberately: **collection- and folder-level
  scripts**, which apply to everything beneath them and would otherwise stamp forty requests with
  a rule none of them declared; and **`prerequest` scripts**, which run before a response exists.
  Reversing the first is a small change if it turns out to matter. See architecture.md §6f.

- **Delete — done**, in the slice after the panel. Right-click a request or press `delete`, and
  a second menu names the file before anything is removed. `context_menu.rs` finally has the
  consumer it was built as a primitive for, and it cost only a `Dismiss` row.

  Two things it turned out not to be. Not a file operation with a refresh: `save_request` writes
  to a remembered `path` with no existence check, so a buffer open on the deleted file would
  have recreated it on the next `Ctrl+S` — the panel then showing a request you had just
  deleted. And not one guard but two: core refuses a directory *and* the panel declines to offer
  the verb on one, because core's refusal makes the outcome identical either way and a test
  asserting the outcome passes against a UI offering a control that can only fail.

- **The rest of the row menu — done**, in the slice after delete. Reveal in file manager, open
  in default app, duplicate, copy path, copy relative path, rename, move to trash, delete.

  The estimate above was wrong and worth correcting rather than deleting: rename was held back
  twice for wanting a "type a new name" modal, and it needs no modal at all. **The tree row is
  the text box** — a `TextInput` drawn in the name's own place with its own key context, which
  is what every file tree does and what this one should have done first. The thing that actually
  cost something was the pair of opposite rules underneath: a renamed request's buffer must
  *follow* the file while a deleted one must *forget* it, both because `save_request` writes to
  a remembered path with no existence check.

  Trash took the `trash` crate rather than ~80 hand-rolled lines of XDG spec — the
  same-filesystem case is the easy one, and the cases that decide whether a restore actually
  works are not. It is also the one verb here with **no end-to-end test**, deliberately:
  driving it would write into the developer's own trash. architecture.md §6a says what that
  leaves uncovered.

  Building it also surfaced a bug both row menus had shipped with: **every keystroke column was
  blank**, because `Window::bindings_for_action` matches an empty context stack on a finished
  frame and so finds only globally-bound actions — and every verb in a row menu is scoped to the
  pane it acts on. A menu whose stated purpose is teaching the shortcut had never shown one.
  architecture.md §6 has the mechanism; the transferable part is that the failure was a *missing*
  string, so it looked deliberate, and the test that should have caught it only ever exercised
  global bindings.

- **And the panel's own layout was wrong for a slice**, found the way §5's layout bugs were —
  by looking at the window, not by reading code or running tests. The tab strip spanned the whole
  width, so a row of open *buffers* was drawn across the top of a tree of saved *files*. The
  panel is a full-height column now with the strip in the editor area beside it.

  Worth recording because it was not only cosmetic: the strip hides itself at one buffer, so
  opening a second one used to push the panel down — and that is precisely what made the
  duplicate-open test read stale bounds between its two clicks and pass against the bug it was
  written for. A sidebar that jumps on an unrelated event *manufactures* flaky tests.

- **New folder and Move — done**, and this is the one that made the panel worth having. Until
  it landed the collection could only be **flat**: `save_request` writes to the collection root,
  nothing created a directory, and nothing put a request in one. The tree was a viewer of a
  structure the app had no way to produce.

  It also settles something this file spent several sessions stuck on. The project/workspace
  naming argument gated **none** of this — new folder, move and save behave identically whatever
  the top level is called — so the decision that felt blocking never was. Worth remembering the
  next time a naming question stalls a slice.

  `Ctrl+S` still writes to the root on purpose: saving into whatever the panel happens to have
  selected depends on state you are not looking at when you press the key. Save, then move.

  **It shipped broken and the two verbs did not compose**, which is worth keeping. The tree took
  its directories from the requests inside them, so a folder you had just created had no row —
  and the move picker, deriving destinations the same way, would not offer it. Creating a folder
  and filling it was impossible, while every test of either verb alone passed. A directory earns
  a row by existing now. The lesson is not about folders: **two features that are each correct
  can still fail to meet**, and nothing in a per-feature test suite looks at the seam.

- ~~**No renaming or deleting a *folder***~~ — **done** in `folder rename, delete, trash and
  reveal`; see item 2 at the top of this audit. `Ctrl+S` still cannot target one, on purpose:
  saving into whatever the panel happens to have selected depends on state you are not looking at
  when you press the key.

  **And it mostly stopped mattering**, because a request can now be *created* in a folder —
  `Ctrl+N` in the panel, a row on both menus, an icon in the header. It is born with a path, so
  `Ctrl+S` overwrites it. Filing a request used to be new-tab, save-to-root, move; the residual
  case is a scratch `Ctrl+T` buffer, which is still Save-then-Move. See architecture.md §6a.
- ~~**The collection root cannot be changed.**~~ **Done** by workspaces — item 1 at the top of
  this audit — so a collection can live in the repo it describes, and the git argument the
  one-file-per-request format is built on is reachable from inside the app.

  Both bullets sat here unstruck for several slices *after* the work that closed them, which is
  the failure direction `CLAUDE.md` calls the most expensive: a doc asserting a gap the code no
  longer has sends a reader hunting for something that isn't there. Found by re-reading this list
  to answer "what's left", which is the only thing that ever catches it.
- ~~**No body prettify.**~~ **Done.** `Alt+Shift+F` formats the request body, `Alt+Shift+M`
  minifies it, and `Ctrl+Z` undoes either because the rewrite goes through the ordinary edit path.

  The interesting part is what it is *not* built on. `serde_json::to_string_pretty` reorders object
  keys alphabetically — `serde_json = "1"` has no `preserve_order` — so it would silently rewrite a
  body whose key order is deliberate. `json/format.rs` walks the outline `flatten` already builds
  and copies each token from its byte span, so only whitespace is its decision. JSON only; XML and
  HTML stay deferred on the same argument as their highlighting. See architecture.md §6g.

  **A planned second half was dropped after reading the docs properly**: making `Ctrl+Shift+C`
  copy the formatted outline. "Copy gives the raw bytes" is listed above under *decisions worth
  keeping*, with a reason and a test, and it was misread here as a gap. See architecture.md §6g.

**6. Discoverability — done, and it should not have taken this long.** Only six of ~40 actions were
reachable by mouse; nine had no affordance at all, including three shipped in the two slices before
this one. Every verb now has an icon button or a clickable label, and each tooltip names its
keystroke read from the live keymap — so the mouse path teaches the keyboard one rather than
replacing it. See architecture.md §2.

The lesson is about how the gap opened, because it is the same shape as §11's. "Keyboard-first"
became keyboard-only one slice at a time: each feature got a binding and a palette row, both of
which satisfied the convention checklist, and neither of which can be *seen*. The palette drift test
proved every action was reachable **by name** and quietly implied it was reachable at all. Worth
remembering that a green test asserting the thing you thought to assert is not the same as coverage
of what a new user can find.

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

**And it recurred, in the picker, found the same way — from a screenshot.** Two more of the same
species, so this is now a standing category rather than a one-off:

- **The picker's rows were 76px wide inside a 620px list.** `uniform_list` gives each item the
  list's width as available space, but taffy only stretches a root node to fill it when the node is
  `display: block`, and a `.flex()` row sizes to its content instead. The visible symptom was a
  selection highlight ending mid-row; the real one was that 88% of every row ignored clicks.
- **`theme.border` was being used as a text colour**, and in the dark theme it equals `bg_hover` —
  so the command palette's keybindings and the settings panel's hints were invisible on precisely
  the row under the cursor. A `text_faint` token and a contrast matrix over every text token ×
  every surface now hold that.

Both are testable *at the consequence* even though the paint isn't: a click in the dead zone, and a
WCAG ratio. That's the transferable lesson — when a rendering bug can't be observed, find the
functional half of it and assert that instead. See architecture.md §12.

**Binary responses — a hex dump, and one dead variant removed.** `BodyKind::Binary` rendered a
single sentence saying how many bytes arrived, which could not answer the only question anyone has
about a binary response: *is this the thing I asked for?* It is now a `hexdump -C` view, so a JPEG
shows `ff d8 ff e0` at offset zero and an HTML error page served as `image/png` is obvious at a
glance. The variant is gone rather than kept — nothing constructs it any more.

The reusable idea is the same one the HTML text view had: **a dump is text**, so indexing it as
lines gives it the viewer's search, selection, copy and horizontal scrolling for nothing. It is
also the one cap in the codebase that **truncates rather than refuses** — the JSON and HTML caps
decline above their limit because half an answer is worse than none, while a hex dump is read for
magic numbers and framing, which are at the front.

Saving got the fix that prompted it: the extension table was five entries wide and everything else
saved as `.bin`. See the egress section above.

See architecture.md §6o.

**HTML bodies — done, and the cheap dependency lost.** An API client gets HTML when a framework
blew up, so the body view gained a text half and a toggle. See architecture.md §6n.

Worth keeping for the crate decision, which inverted under a test. `nanohtml2text` is one crate
to `html2text`'s twelve and twelve times faster, and it was chosen on exactly that. It passes
`<pre>` contents through raw — undecoded entities, literal nested tags — and `<pre>` is where
Django, Flask and Rails each put the traceback. **The benchmark that made it look fine had no
`<pre>` in it.** The lesson is not "prefer the big crate"; it is that a sample which omits the
one element the feature exists for measures nothing.

Also recorded: XML and HTML *highlighting* stay deferred, now by decision rather than by cost.
`quick-xml` would make XML cheap — a pull parser whose byte spans map straight onto the existing
`Token` shape, and the response viewer needs none of the lexer tolerance the editor does — but
the audience is thin enough that it is not worth the surface.

**Inline body diff — done, and the structural-diff instinct was wrong.** This sat in "Named, not
planned" reading: *"A structural diff over `Row` spans is probably better than a text diff, now
that the JSON outline exists."* It is not, and the reason is worth keeping.

A structural diff answers "which field changed" and cannot answer anything about a body that is
not JSON — an HTML error page, a plain-text 500, a CSV export. A *line* diff answers both, but
only once the lines mean something, and for a minified JSON body they do not: the whole document
is one line. The outline earns its keep either way — not as the thing being compared, but through
`json::format::pretty`, which turns one line into one field per line. **Normalize, then run an
ordinary text diff** does the structural job on JSON and keeps working on everything else.

The second correction is that the algorithm was never the expensive part. `similar` is one crate
with no transitive dependencies (measured with `cargo tree`, against `imara-diff`'s four and
`syntect`'s forty-six), and it brings Patience, hunk grouping and word-level refinement. What was
actually ours to get right was the normalization, the three caps, and refusing to show the diff
beside a run it does not describe.

See architecture.md §6m.

**Syntax highlighting — done, and principle 4 mispriced it.** Kept rather than deleted, because
the correction is the useful part.

The principle read it as tree-sitter plus a highlight cache: expensive, isolated, improving nothing
structural. Three of those four were wrong for JSON. It needs a **lexer, not a parser** — an
editor's text is invalid on most keystrokes, and colour survives that where a parser cannot. It
needs **no cache**, because JSON has no multi-line tokens, so each visible line is lexed
independently. And it was **not isolated**: response search highlighted a whole row rather than the
matched characters *precisely because* splitting a shaped run is the syntax-highlighting problem,
so the two closed together.

What stands up is the shape of the reasoning, not the verdict: it really is the most expensive
thing left *for a general language*. The mistake was pricing the general case when only JSON was
wanted. XML and HTML are still deferred on exactly the original argument — see the HTML entry above for
where that landed: the *text* of an HTML body is now readable, which is the half that mattered,
and colouring its markup is not.

See architecture.md §6.

---

## Named, not planned

Reasons recorded so a future session can judge them, not commitments.

- **Scripting** (pre-request / post-response). The largest single feature in the original
  original brief, and the one most likely to define the product's ceiling. Needs a language and a
  sandbox decision before anything else.
- **gRPC — all four call shapes, metadata and trailers, and reflection.** The design and its
  rejected alternatives are architecture.md §6q; this is the order it was built in. A `.proto` is compiled by `protox` (no
  `protoc` binary, which is what makes it shippable in the `.deb`), `prost-reflect` encodes JSON
  into a `DynamicMessage`, and the call goes out over the existing reqwest client. Choose a
  schema, pick a method, type JSON, send.

  **The transport was spiked before anything was built on it**, because two of its assumptions
  were load-bearing. gRPC reports its status in HTTP/2 *trailers*, which reqwest names nowhere —
  reachable through `From<Response> for http::Response<Body>` and `http_body::Body`, proven in
  `core/tests/grpc_trailers.rs`. And plaintext gRPC needs h2 with **prior knowledge**: there is
  no ALPN on a cleartext socket, so a client that merely prefers h2 sends HTTP/1.1 and the
  server hangs up. That is the exact mirror of the `wss://` bug, and it is why `ClientKey` now
  carries an `Alpn` enum rather than an `http1_only` bool.

  **`is_session()` had been standing in for two different questions**, which gRPC exposed. It was
  passed as "offer only HTTP/1.1" — true of every session that existed when it was written — so a
  server-streaming gRPC call would have been pinned to the one version gRPC cannot use; and it
  routed dispatch, so the same call would have opened a WebSocket handshake at a gRPC server.
  Both silent.

  A `.proto` lives in the collection's reserved `protos/`, named rather than pathed, for the
  reason invariant 10 exists: an absolute path into one person's home directory is broken for
  everyone who clones the repo.

  **Server streaming landed with it**, and cost almost nothing: past `Opened` the app cannot tell
  a gRPC stream from a socket or an SSE stream, which is the fourth time keeping lifecycle off
  the kind has paid for itself. The one piece that needed writing is `grpc::Reader`, because a
  length-prefixed message arrives split across HTTP/2 DATA frames however the network felt like
  splitting it — and it is capped for the reason the SSE parser is.

  It also sharpened why trailers matter: a stream can deliver three messages and *then* fail, all
  under one 200. A client judging by the status line, or treating the end of the body as success,
  shows partial data as if it were the whole answer.

  **All four call shapes now work.** Client-streaming needed the one mechanism nothing else in
  Zuno has: a request body that stays open, fed by the same `Outbound` channel a socket sends
  down, with a half-close — dropping the body sender — as the thing that lets the server answer
  at all.

  Three bugs on the way, all one family: **"is this a session" and "does this stream" kept being
  the same question in the code and different questions in fact.** It picked the wrong ALPN, it
  routed a gRPC call into the WebSocket handshake, and it read a client-streaming call — which
  answers exactly once — as a transcript. Each was silent. The fourth of the family is still
  worth watching for.

  And one that only a repeated run found: a bidirectional call read zero frames about half the
  time, because **dropping a `oneshot` sender resolves its receiver**, so the feeder task ending
  after half-close was indistinguishable from someone pressing Disconnect. It presented as a
  clean close with no data, which is the worst shape — nothing on screen said anything was wrong.

  **Reflection landed last, as planned, and the ordering paid off exactly as argued.** Asking a
  server to describe itself *is* a bidirectional streaming call, so by the time it was built the
  transport was proven and what remained was the conversation: `list_services`, then
  `file_containing_symbol` per service — by *symbol*, which is what makes the answer a complete
  schema without knowing how the server lays its files out.

  Only the reflection envelope is hand-decoded, about sixty lines. What comes back inside it is a
  `FileDescriptorProto`, which goes straight to prost-reflect — so the hard parsing stays in a
  library and the hand-written part is small enough to check against the spec by eye.

  **It is an import, not a live lookup.** The descriptor set is written into the collection's
  `protos/` and the request points at it like any hand-written schema, which is why reflection
  needed no new field on `GrpcRequest` at all. See architecture.md §11 for the trade.

  **And then a real server found three bugs the whole test suite had missed**, which is the part
  worth keeping. Reflection was written, tested nine ways and reported done; the first live
  attempt failed with a rustls error about a corrupt message.

  - `list_services` sent an **empty** value. The spec says it is ignored; a real server reads it
    for truthiness, sees "unset", and answers nothing — with `grpc-status: 0`, so silently.
    `"*"` works everywhere.
  - The conversation was **one held-open bidi stream**, which is what the service is declared as.
    A server that buffers the whole request before replying deadlocks against it: measured, first
    byte at 9.99s of a 10s hold. One call per question, half-closing immediately, works against
    both kinds.
  - A scheme-less address defaulted to **TLS**, and gRPC servers are routinely cleartext —
    including on port 443. Now plaintext on loopback only, because widening it would send
    credentials out readable; everything else keeps the secure default and gets an error that
    names the fix.

  All three were invisible to nine passing tests, because every one ran against a fixture written
  to agree with the client. CLAUDE.md says the live `wss://` check is not optional for exactly
  this reason and the same test was missing here. It exists now.

  **A line-by-line audit before commit found four more wrong answers**, each proven with a
  throwaway test before it was fixed. A *Trailers-Only* error — the status in the response head,
  which grpc-go and grpc-java send for an unknown method — was reported as "not a gRPC endpoint".
  A typed `Content-Type` metadata entry went out *beside* ours, because reqwest's `header`
  appends. A bidirectional call against a buffering server deadlocked, because the early-`Opened`
  fix had been applied to client-streaming alone. And the engine read the shape from the stored
  copy in one place and the schema in another, so a stale copy hung the call. The same lesson as
  the reflection bugs, one layer further: every fixture here answered the way the client already
  expected, and the bidirectional test had been written to send before `Opened` — encoding the
  deadlock as the test's own ordering requirement.

  That closes gRPC. What is named but not planned: per-call reflection, a hang-up verb for a
  bidirectional call, copying a chosen `.proto` into the collection rather than storing an
  unportable absolute path, reflection without a collection open, and the `.proto` route's
  remaining rough edge — a schema whose imports live outside the file's own directory.

- **gRPC, the original note.** Shipped WebSocket first, deliberately, and the ordering is the point: both need a
  request that stays open and a transcript instead of a response, and WebSocket forces that to
  be built with the simplest possible payload story — you type text and send it. gRPC would have
  meant building the session model *and* a schema pipeline at once, with no way to tell which
  half was wrong when something broke. Its real cost is schema, not transport: you cannot compose
  a single call without knowing the service and message types, and both routes are expensive —
  server reflection is itself a *bidirectional streaming* call, so learning the schema means
  implementing the hardest of gRPC's four shapes first, and the `.proto` route needs a protobuf
  compiler plus a dynamic encoder (`protox`, `prost-reflect`) in `zuno-core`. Transport is the
  easy part and is already proven: gRPC needs HTTP/2 and reports its status in **trailers**,
  which reqwest names nowhere — but `reqwest::Body` implements `http_body::Body` and forwards
  hyper's `Frame` verbatim, and a `Frame` carries trailers.

- **WebSocket, the parts deliberately left.** The loop is complete and used — handshake through
  the real client, `wss://`, frames both ways, a transcript with the response body's own viewer,
  saved messages, clean close. What is not built, and why each is a fair deferral rather than an
  oversight:

  - **Sending a binary, ping or pong frame.** The engine sends whichever `Frame` variant it is
    handed and the transcript labels all four on the way in; only the *composer* is text-only.
    Recorded in architecture.md §11, because it is engine capability with no UI path rather than
    something unbuilt. A manual ping is the one worth reaching first — it answers "is this quiet
    socket still alive", which nothing else on screen can.
  - **A close code and reason.** Disconnect always sends `close(None)`. Servers that care about
    *why* a client left — and some log it — get no answer. Needs a control on the disconnect
    path rather than any engine work.
  - **Reconnect.** No button, no automatic retry, no backoff. Reconnecting today means pressing
    Connect again, which works and loses the transcript. Auto-retry in particular wants a
    decision first: a client that silently reconnects is a client that hides a server problem.
  - **Loading a saved message by keyboard.** Click only. Every other verb has a shortcut.
  - **The subprotocol field overrides a hand-typed `Sec-WebSocket-Protocol` header**, silently —
    `build_websocket` uses `insert`, not `append`. Defensible (the field is the specific answer,
    the header is the general one) but undocumented in the UI, so someone will hit it once.

  - **The transcript cap — done.** A ring bounded by 16 MiB of retained payload and 10,000
    frames, whichever binds first, dropping oldest and saying how many it dropped. A `VecDeque`,
    because evicting from the front of a `Vec` is a memmove of everything behind it — the same
    quiet O(n²) the render already paid for once. `WebSocketConfig::max_message_size` is 8 MiB
    rather than tungstenite's 64, so no single frame can be four times the whole budget.

    **The part worth remembering is not the arithmetic.** `session_selected` is a position in
    `frames`, so every eviction moves it: without the reindex, the detail pane keeps its
    highlight and silently shows a *different* frame, and when the selected one falls off the
    front there is nothing honest left to show. `push` returns the number evicted for exactly
    that reason. Break-tested by removing the reindex.

    It pairs with the lazy render from the same slice: only visible rows are formatted, so a long
    transcript costs nothing to draw and the cap is about memory alone.

  **Four of these were later closed as a slice of their own**, chosen because they shared one
  shape: the app said something false. A close the peer ignores now gives up after a grace rather
  than leaving the task alive and the strip reading `open`; a graphql-transport-ws handshake that
  is never acknowledged times out against `settings.timeout`, which is what a protocol-layer auth
  rejection looks like from the outside; a frame the engine swallowed is named in the transcript
  instead of vanishing; and an unresolved `{{var}}` in a frame is *announced* rather than blocked
  — refusing would contradict `build.rs`'s deliberate choice not to check bodies, where `{{` is
  legal JSON. §11's ping came with them, since "is this quiet socket alive" is the same question
  the first two are about.

  What is left is below, and the rest of the list is unchanged.

  From the audit of the slice, kept rather than fixed. Each is real; none is a lie on screen,
  which is where the line was drawn:

  - **The transcript never shows the Pong we send.** tungstenite answers a Ping itself
    (`protocol/mod.rs:672`) outside our `send` path, so the record shows an inbound Ping and no
    reply — which reads as ignoring it. A transcript's job is being a faithful record.
  - **The resolver is re-read from disk on every frame send.** Sized for once per request, not
    once per message.
  - **A scheme-less URL becomes `wss://`.** Matches `resolve_url`'s https default, but local
    socket development is overwhelmingly plaintext, so `localhost:8080/ws` fails a TLS handshake
    against a plaintext server.

  From the line-by-line audit of the streaming work, kept rather than fixed. The fourteen that
  *were* fixed are not listed — they are in the code. These are the rest, in full, so that
  "deferred" means written down rather than remembered:

  - **`uses_websocket(cx)` clones the whole document per repaint.** `graphql_query_header` calls
    it to label the transport; it goes through `to_spec`, which copies the query text, then runs
    the sniffer. Every frame the Query tab is visible.
  - **`Parser::push` drains the pending buffer inside its line loop**, so a chunk holding many
    lines is quadratic in lines. Bounded by chunk size, which is why it is here and not fixed.
  - **The kind is asked about its socket twice per send** in `drive` — `Alpn::for_kind` and
    `opens_a_websocket` — and for GraphQL that is two document scans for one answer.
  - **`disconnecting_a_stream_actually_stops_it`'s doc still overstates its own assertion.** The
    load-bearing check is the `Closed` event, not the server-side one the comment names. The
    assertion itself was tightened when the unbounded loop was fixed; the comment was not.
  - **The same subscription yields different frame types on different transports** —
    `Frame::Event` over SSE, carrying the event name, and `Frame::Text` over WebSocket. Compare
    the two transports of one operation and the transcript looks different for no visible reason.
    `a_graphql_subscription_rides_a_socket_and_unwraps_its_payloads` asserts the current shape,
    so fixing this means changing that test too.
  - **`step()` ignores the envelope `id`.** One subscription per socket today, so a `complete`
    for an id we never opened would still close the session. It matters the moment anything
    multiplexes.
  - **Notice rows carry `cursor_pointer`, hover and a click handler** that `select_frame`
    rejects. Clicking one does nothing.
  - **A reconnect notice does not scroll into view.** The follow logic lives in the frame arm
    only, so the most important row in the transcript is the one that does not scroll to.
  - **The SSE parser does not strip a leading BOM**, which the spec asks for. A server sending
    one turns the first field name into `\u{feff}event` and it is silently ignored.
  - **`stream_events` carries `#[allow(clippy::too_many_arguments)]`** rather than grouping the
    four handshake parameters that travel together.
  - **`Save the composed message` and `Choose the GraphQL transport` are unconditional palette
    rows.** On an HTTP request both appear; the second sets a status, the first returns in
    silence. `active_http`'s convention says the guard is the fix, and one guard says nothing.
  - **`build::build` runs outside the head deadline.** For a binary body it reads the file from
    disk, so a request whose body sits on a stalled mount blocks with no timeout and no event
    after `Started`. Not a regression — nothing covered it before either.
  - **`Transcript::frame_count`'s decrement has no test.** The increment is exercised by the ring
    test; nothing checks the count after an eviction. A drift shows a wrong number on the strip.

- **SSE — done**, and it cost a content-type check plus one change of meaning.

  A response whose head says `text/event-stream` becomes a session instead of a body: the same
  `Opened` / `Frame` / `Closed` events a socket emits, so the transcript, the frame detail with
  its JSON viewer, copy and find all work with nothing added. `Frame::Event` carries the name and
  id, because an SSE stream routes on `event:` and flattening it to the payload would throw away
  the half you filter on. This is the payoff for **lifecycle not being part of the kind** — a
  plain GET, a GraphQL subscription and a socket now all arrive at the same surface, and only the
  socket could have promised it in advance.

  **The expensive half was the timeout, and it changed meaning.** It used to be reqwest's
  deadline on the whole exchange, body included, which a stream cannot meet by definition — an
  event stream answered by an HTTP request died at whatever the setting said. (Only `build_http`
  ever set one; `build_graphql` never has, so graphql-sse was never killed this way.) It now
  means *answer within N, and do not go
  silent for N*: `run::execute` deadlines the response head, and `ClientKey::read_timeout`
  deadlines each read. Two consequences worth knowing. A slow but progressing download is no
  longer aborted — it is still capped by `MAX_BODY_BYTES`. And `timeout` now **fragments the
  client cache**, because `read_timeout` is a `ClientBuilder` setting rather than a per-request
  one. `a_timeout_is_a_client_property_and_fragments_the_cache` asserts that, and was renamed
  from `client_keys_ignore_settings_that_are_per_request` — which had been reversed in place and
  left asserting the opposite of its own name.

  The transcript names its transport and keeps the handshake. `Event::Opened` carries a
  `Transport`, reported by the engine rather than guessed from the kind — the guess is right
  today only because nothing but a socket opens a session, and the whole point of the lifecycle
  split is that it will not stay that way. The status line and the response headers now live on
  the `Transcript` rather than on `inflight`, which is cleared at the close: a finished stream
  used to keep its frames and silently lose everything about the response that carried them.
  A session's tab strip offers Frames and Headers and not Timing or Diff, which have nothing to
  draw for a stream.

  **graphql-transport-ws — done**, which is what makes GraphQL subscriptions work against real
  servers rather than only against the one demo endpoint that also speaks graphql-sse. Apollo
  Server, Hasura and graphql-yoga all default to the socket; before this, a subscription was
  POSTed and the failure said nothing about the transport being the problem.

  `GraphQlRequest::transport` is `Auto` / `Http` / `WebSocket`, and **`Auto` reads the document**
  — `core/src/graphql.rs` finds the operation `operationName` selects, or the only one there is,
  and a `subscription` opens a socket. It is a sniff, not a parser: anything it cannot make sense
  of answers `None` and the request goes the way it always did, because a guess must never be the
  reason a working request stops working. The query header shows what Auto resolved to (`auto ·
  websocket`), since a route decided for you and never shown is a route you cannot debug.

  This is the **one place the "lifecycle is the server's decision" rule bends**, and deliberately:
  a socket handshake and a POST are different requests, so the client cannot wait to be told. It
  is also the first time a *subprotocol* gets first-class treatment, which is the precedent MQTT
  over WebSocket and STOMP would inherit.

  Two details worth keeping. The subprotocol offered is `graphql-transport-ws`; the string
  `graphql-ws` belongs to the **deprecated** `subscriptions-transport-ws`, and the modern library
  called `graphql-ws` announces itself as the former — one name, two things. And the transcript
  gets `next` payloads unwrapped, not envelopes: the envelope is addressed to the client, the
  data is addressed to the person.

  **Reconnection — done, and only where it can be lossless.** SSE picks itself up: the stream
  drops, the transcript shows a notice where the gap is, and the retry goes out with
  `Last-Event-ID` so a server that honours it replays what was missed. The server's own `retry:`
  sets the delay, backed off by attempt and capped at 30s, giving up after six consecutive
  failures — a connection that delivers anything resets the count, so a long feed that blips
  never exhausts it. `204 No Content` ends it for good, which is the only way SSE can say "do
  not come back"; without honouring it, a client retries forever against a server politely
  saying stop.

  **Sockets get a Reconnect control instead, deliberately.** Neither WebSocket nor
  graphql-transport-ws can resume, so an automatic retry there loses every message in the gap
  while looking like it worked. It dispatches `SendRequest`, because a reconnect with no resume
  *is* a new conversation.

  The notice lives in the transcript rather than in a counter on the strip: a stream that dropped
  at 12.4s and came back at 15.9s is missing whatever happened between, and a count says that it
  happened without saying when — which is the only part you need in order to read what is gone.
  `TranscriptRow` carries a `TranscriptKind` for that reason; not everything in a conversation is
  a message. SSE defines `retry:` and replay from `Last-Event-ID`, the parser reads
  both, and nothing acts on them — a dropped stream stays dropped until you send again. And the
  transcript cap above applies here too, more sharply: a subscription is the likeliest thing in
  the app to run for hours.
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
