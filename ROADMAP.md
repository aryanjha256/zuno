# Zuno — Roadmap

**This document owns *order*.** `architecture.md` owns design and records what was tried;
`CLAUDE.md` owns mechanics. When they disagree with this file, they win — a roadmap is the
document most likely to rot, so treat it as disposable and rewrite sections rather than patching
them.

No dates. Detail decays with distance on purpose: the next phase is concrete, the one after is
directional, and anything beyond that is a name and a reason.

---

## Where we are

**Milestone 1 is complete: the request → response loop.** Author a request, send it over real
HTTP with streaming progress and cancellation, read the response through a virtualized JSON
viewer, diff it against the previous run, come back to it after a restart. Plus curl import,
light/dark themes, window chrome.

Measured: **189 ms** cold start (release), **48 ms** to flatten 10 MB of JSON into 1.31 M rows
off-thread, 60 fps scrolling on any response size.

**M2 is complete.** Tabs as editor buffers, collections as one-file-per-request in git, `Ctrl+P`
over buffers and saved requests, and `Ctrl+K` over every command. 260 tests across three layers,
and a `.deb` on tagged releases.

The navigation thesis is built. What's left is **reuse** — see M3 — and the honest
next question is no longer "can you get around" but "can you stop retyping things".

That gap *is* the roadmap. Everything below is about closing it.

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

3. **Prefer work that exposes capability already built.** §11 of `architecture.md` tracks engine
   capabilities that are honoured on every request with no way to reach them. `Ctrl+,` closed five
   of the original nine in one modal, which is the ratio this principle is about. **Four remain**,
   and none is a toggle: form and binary body authoring, multipart, the response history browser,
   and custom HTTP methods.

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
interpreting. Deliberately *not* a `PickerDelegate` trait yet — one consumer, and invariant 1 says
API waits for a caller; `Ctrl+K` is a new `Target` variant, not a rewrite.

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

**Environments and variables.** The engine already *detects* unresolved `{{var}}` and refuses to
send — so the failure path exists and substitution is the actual work. Needs a scope model
(global → environment → request) and a decision about where secrets live, which is the first
question in this milestone that isn't purely mechanical.

**Auth helpers.** Mostly UI sugar over headers: Basic already works end to end (curl import
proves it). Bearer is trivial. OAuth flows are a genuine project and should be scoped separately
rather than smuggled in here.

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

**History browser.** Ten responses per request are already retained; only the diff surfaces them.

---

## M4 — Editing

**Syntax highlighting** in the request body and the JSON viewer. Principle 4. Theme tokens
(`SyntaxTheme`) were defined back in M1.0 so the palette wouldn't have to be invented under
pressure.

**Method dropdown**, replacing cycling — free once the picker exists.

**Form and multipart authoring.** Note multipart is the one item in §11 that isn't purely UI work:
curl import parses `-F`, but the engine still returns `UnsupportedBody`.

---

## Named, not planned

Reasons recorded so a future session can judge them, not commitments.

- **Scripting** (pre-request / post-response). The largest single feature in the original
  original brief, and the one most likely to define the product's ceiling. Needs a language and a
  sandbox decision before anything else.
- **Request chaining** — extract a value from one response, feed it to the next. Arguably more
  valuable than general scripting and far smaller.
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
