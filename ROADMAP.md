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

**M2 is under way.** Tabs are done — buffers, a strip, a versioned session envelope that migrates
every older format forward. Collections are done as a *format*: one request per file, git-diffable,
written by Ctrl+S. 214 tests across three layers.

**The navigation thesis is still the gap.** `what.md` named `Ctrl+P`, `Ctrl+K`, and fuzzy search
across collections as the defining features, and none of them exist yet. That has a sharp
consequence right now: requests can be *saved* to a collection but not *opened* from one, because
the opener is the picker. Nothing else in the app is waiting on so much.

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

3. **Prefer work that exposes capability already built.** §11 of `architecture.md` lists nine
   engine capabilities with no way to reach them — cookie jar, timeout, redirects, TLS toggle,
   encodings, history. A settings panel and a history browser each surface several at once, which
   makes them unusually cheap for the value.

4. **Defer the expensive and isolated.** Syntax highlighting needs tree-sitter plus a highlight
   cache, touches nothing else, and improves nothing structural. It is the most expensive thing
   left and the least load-bearing, so it goes last regardless of how much it would flatter a
   screenshot.

---

## M2 — Navigation

The thesis milestone. Concrete, because it's next.

**Tabs — done.** What makes the "20–100+ open requests" claim from `idea.md` true.

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

**The picker primitive.** See principle 2. An overlay, a filter input, fuzzy scoring, keyboard
selection, and a `Vec` of candidates it doesn't know the meaning of.

**`Ctrl+P` — find any request.** The picker over collections.

**`Ctrl+K` — command palette.** The same picker over the action registry. Budget more than
"`actions.rs` already lists the verbs" suggests: `actions!` generates *types*, not an iterable
registry. The runtime list is `cx.all_action_names()` (with `cx.build_action` to dispatch by name),
but it returns namespaced strings for *every* registered action — including the ~20
`text_input::`/`editor::` ones that must not appear in a palette — and carries no human labels or
keybinding hints. A curated label table is real work, not a lookup. `actions.rs` also isn't the
single list: its own header points at `input::text_input` for the editing verbs.

> **Done when:** you can hold 50 requests open across collections, reach any of them by name
> without touching the mouse, run any command from the palette, and nothing about it feels slower
> than the single-request loop does today.

---

## M3 — Reuse

Directional. The theme is *stop retyping things*.

**Environments and variables.** The engine already *detects* unresolved `{{var}}` and refuses to
send — so the failure path exists and substitution is the actual work. Needs a scope model
(global → environment → request) and a decision about where secrets live, which is the first
question in this milestone that isn't purely mechanical.

**Auth helpers.** Mostly UI sugar over headers: Basic already works end to end (curl import
proves it). Bearer is trivial. OAuth flows are a genuine project and should be scoped separately
rather than smuggled in here.

**Settings panel.** Principle 3 — surfaces six of §11's capabilities at once. Cheap enough to pull
forward into M2 if it starts getting in the way; the cookie jar in particular is on by default and
invisible, which makes consecutive requests non-independent with nothing on screen saying so.

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
  `idea.md` list, and the one most likely to define the product's ceiling. Needs a language and a
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
- **Beating Postman on feature count.** The bet is feel. `idea.md`: *"the first milestone
  shouldn't be build Postman"* — that stays true at every milestone.

---

## How to use this file

Start a milestone by re-reading the four principles, not the feature lists. If a phase's contents
no longer make sense, the principles are what tell you the new right answer — rewrite the phase.

Before adding anything, check it against the non-goals, and check `architecture.md` §11 to see
whether it's already built and merely unreachable.
