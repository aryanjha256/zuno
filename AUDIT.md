# Zuno — code audit

A read-only review of the whole tree (~17.8k lines: `core/` 4.9k, `app/` 6.6k, tests 3.3k, docs).
No code was changed, nothing was built or run. Every claim below was traced in the source; where a
finding depends on GPUI behaviour I read the vendored `gpui-0.2.2` source rather than trusting a
comment, and I say so.

**Verdict.** This is unusually disciplined code. The core/app split is real and load-bearing, the
derived-`spec()` design genuinely eliminates the desync class of bug, the tokenizer and line index
are correct on the edge cases that matter, and the test suite drives real keystrokes and real
sockets. The docs are the best part: `architecture.md` records rejected alternatives, which is rare
and made this audit much faster.

The findings cluster in three places, and the pattern is worth naming:

1. **The comments have started to run ahead of the code.** Three places describe a call that isn't
   there or a behaviour that changed (#7, #4, #9, plus cleanup items). This is the same failure the
   repo already documents twice under "Docs went stale twice while the code was right" — except now
   it is the code that's wrong and the comment that's right, which is harder to catch.
2. **Features added late didn't revisit earlier assumptions.** Body authoring became complete in M3
   but variable substitution (#1) and `body_label` (#3) still assume raw-only bodies.
3. **Invariant 3 ("nothing parses or formats on the UI thread") is enforced for the response index
   and nowhere else** — the diff (#8) and the session write (#9) both do real work in the frame.

Nothing here is architectural. The most valuable single fix is #1.

**Status.** All 21 findings and all five cleanup items are fixed. Each behavioural fix has a test
that was confirmed to fail against the old code before the fix landed; where a property wasn't
test-observable (thread placement, a deleted variant, a stale doc claim) that is stated explicitly
rather than papered over with an assertion that looks strong and proves nothing.

---

## High

### 1. Variables are never substituted into form or multipart bodies

> **Fixed.** `apply`'s body match is now exhaustive with no catch-all, so a new `Body` variant fails
> the build until someone decides whether a variable belongs in it. Guarded by
> `apply_substitutes_form_field_values`, `apply_substitutes_a_form_field_name`,
> `apply_substitutes_multipart_text_parts_but_never_file_paths`, and — over a real socket —
> `a_variable_in_a_form_field_reaches_the_socket_substituted`.

`Resolver::apply` substitutes the URL, query names/values, header names/values, and `Body::Raw`
text — and then stops. `Body::Form` and `Body::Multipart` field values pass through untouched.

[core/src/environment.rs:175-180](core/src/environment.rs#L175-L180)

```rust
// Only raw bodies. Form and multipart field values would want this too once the UI
// can author them; `Binary` is a path, and substituting into it would let a variable
// choose which file gets uploaded.
if let Body::Raw { text, kind } = &resolved.body {
```

The comment's precondition ("once the UI can author them") has been true since M3 — `Ctrl+Shift+F`
and `Ctrl+Shift+M` author both, and `ROADMAP.md` records §11 as empty.

**Why it matters.** There is no error path to catch it. `build.rs` deliberately does not scan bodies
for `{{…}}` (correctly — `{{` is legal in JSON), so an unresolved placeholder in a form field is
sent to the server verbatim with a 200-shaped failure at the other end. The motivating case is the
one the ROADMAP itself names as the reason to build request chaining:

```
POST /oauth/token
grant_type=client_credentials&client_id={{id}}&client_secret={{secret}}
```

That is a form body. Today it goes on the wire as the literal string `{{secret}}`. So the one
credential shape the environment split exists to protect — a secret in `dev.local.json`, referenced
by name — is exactly the shape that silently doesn't work.

**Fix.** Extend the match to `Body::Form` (resolve `name` and `value`) and `Body::Multipart`
(resolve `name`, and `MultipartValue::Text`). Keep `MultipartValue::File` and `Body::Binary`
unsubstituted — that reasoning is already right and already tested
(`a_binary_body_path_is_never_substituted`). Then delete the stale half of the comment.

---

### 2. Closing a tab abandons the UI side of a request but not the socket

> **Fixed.** `close_tab` now cancels through the engine before removing the view. Guarded by
> `closing_a_buffer_cancels_its_in_flight_request`, which holds an `Entity` handle so the buffer
> outlives its removal and its `inflight` state can still be read.

`close_tab` drops the `RequestView`, which drops `InFlight` and with it the consuming `Task`. It
never calls `Engine::cancel`.

[app/src/workspace.rs:226](app/src/workspace.rs#L226)

There is no `Drop` impl anywhere in the tree (verified: `grep -rn "impl Drop"` returns nothing), so
nothing recovers this. The engine thread keeps the job in its `jobs` map, keeps draining the socket,
and keeps appending to an in-memory `Vec<u8>` that no longer has a consumer, until the server
finishes or the 30s timeout fires.

This is precisely the failure the design documents twice — `Engine::cancel`'s own doc comment and
`RequestView::cancel`'s both say cancellation has two halves and needs both. `Escape` does it right;
`Ctrl+W` doesn't.

**Failure scenario.** Send a request against a slow endpoint returning a large body, `Ctrl+W` the
tab. The download continues to completion into memory that is then dropped. With several tabs closed
mid-flight this is both bandwidth and a memory spike with nothing on screen to explain it.

**Fix.** Cancel before removing:

```rust
if let (Some(view), Some(engine)) = (self.active(), cx.engine()) {
    view.update(cx, |view, cx| view.cancel(&engine, cx));
}
self.views.remove(self.active_ix);
```

A `Drop` on `RequestView` can't do it — it has no engine handle — so the workspace is the right
place. Worth also considering it for `RequestView::load`, which resets `inflight` to `None` on
[request_view.rs:324](app/src/request_view.rs#L324) with the same effect.

---

### 3. A request with no body advertises "JSON"

> **Fixed.** `BodyType::Empty` now reports "None", which also makes the picker's `current` marker
> correct because it compares against that string. Guarded by
> `a_body_less_request_says_none_rather_than_a_retained_sub_kind` and
> `the_body_type_picker_marks_none_as_current_on_a_fresh_buffer` — the latter deliberately on a
> fresh buffer, since the pre-existing picker test boots the sample request whose raw JSON body made
> the old label accidentally correct.

`body_label` folds `Empty` in with `Raw` and returns the raw sub-kind for both:

[app/src/request_view.rs:864-871](app/src/request_view.rs#L864-L871)

```rust
BodyType::Empty | BodyType::Raw => SharedString::from(self.body_kind.label()),
```

`body_kind` defaults to `RawKind::Json`, so every fresh buffer (`Ctrl+T`, and `close_tab`'s
replacement buffer) reports its body as **JSON** while `body_region` in the same pane renders
"No body — Ctrl+Shift+B to pick a type". Two widgets, one screen, contradicting each other.

It also breaks the body-type picker's "current" marker, because that is computed by comparing
labels:

[app/src/workspace.rs:950](app/src/workspace.rs#L950) → `let current = view.read(cx).body_label();`

So on a fresh buffer `Ctrl+Shift+B` marks **JSON** as `current · application/json` and never marks
`None`. The picker is actively misreporting the state it exists to change.

**Why it survived.** `the_body_type_picker_offers_every_authorable_type` boots the *sample* request
(`BodyType::Raw`, `RawKind::Json`), where the label happens to be correct. No test opens the picker
on a fresh buffer. `a_fresh_buffer_starts_with_no_body` asserts `body_type` but never `body_label`.

**Fix.** `BodyType::Empty => SharedString::from("None")`. That string already matches the picker's
own `None` row label, so the `current` marker starts working with no other change.

---

## Medium

### 4. The body-kind chip bypasses the action system and still cycles

> **Fixed.** The chip dispatches `OpenBodyType`, matching the method chip beside it;
> `cycle_body_kind` is deleted (it had one caller, which is now gone); the comment says what the
> chip actually does. Guarded by `clicking_the_body_chip_opens_the_type_picker`, which drives a real
> click via gpui's `debug_selector`/`simulate_click` — the bug was *in* the click path, so nothing
> short of clicking tests it. It also asserts `body_kind` is unchanged, which is the half that
> distinguishes "opened a picker" from "cycled in place".

[app/src/request_pane.rs:105-111](app/src/request_pane.rs#L105-L111)

```rust
.on_mouse_down(
    MouseButton::Left,
    cx.listener(|view, _: &MouseDownEvent, _, cx| {
        view.cycle_body_kind(cx)
    }),
)
```

Three problems in five lines.

- **The comment above it is wrong.** [request_pane.rs:69](app/src/request_pane.rs#L69) says "with a
  clickable body-kind chip (`Ctrl+Shift+B` does the same)". `Ctrl+Shift+B` opens `OpenBodyType`, a
  picker over eight types. The chip cycles four raw kinds. They are not the same verb.
- **It violates a stated convention.** CLAUDE.md: *"Actions, not direct calls, for anything a button
  and a keybinding share."* The method chip 40 lines above does it correctly — it dispatches
  `OpenMethod`. This one calls into the view.
- **It is visibly broken on three body types.** With `body_type` of `Form`, `Binary`, or
  `Multipart`, `body_label()` returns "Form"/"Binary"/"Multipart", so clicking the chip mutates
  `body_kind` — invisible state that nothing will read until you switch back to `Raw` — and the
  label does not change. The user clicks a control and nothing happens, twice, then it silently
  changes what a later type-switch produces.

`cycle_body_kind` is the leftover of the cycling the ROADMAP says was *replaced* ("Cycling could
only reach JSON/Text/XML/HTML, so form, binary and multipart were unreachable"). It has exactly one
caller — this one — and no test.

**Fix.** Dispatch `OpenBodyType` from the chip and delete `cycle_body_kind` (invariant 1). The
picker already reaches every raw kind by name.

### 5. Tab moves focus out of an open modal and kills the keymap

> **Fixed together with #6**, since they are one defect — focus leaving a modal that stays on screen
> — reached two ways. `Workspace::modal_open` is now the single predicate, consulted by
> `FocusNext`/`FocusPrev` and by all seven openers. Guarded by
> `tab_does_not_move_focus_out_of_the_picker` (which asserts on where keystrokes *land*, because
> `picker_is_open` stays true in the broken case) and `tab_does_not_strand_the_settings_panel`
> (which asserts Escape still works, the harm as the user meets it).
>
> The guard sits on the handler rather than on the key binding: GPUI matches only the leaf context,
> so "not inside a modal" would have to be restated for every modal that ever exists. Recorded in
> architecture.md §12 with the rejected alternative.

`FocusNext`/`FocusPrev` are bound with **no context**:

[app/src/main.rs:149-150](app/src/main.rs#L149-L150)

```rust
KeyBinding::new("tab", FocusNext, None),
KeyBinding::new("shift-tab", FocusPrev, None),
```

and the handlers call `window.focus_next()` unconditionally
([workspace.rs:871-877](app/src/workspace.rs#L871-L877)).

The picker and the settings panel are overlays — the request pane behind them is still painted, so
its `TextInput`s are still tab stops (`TextInput::new` sets `.tab_stop(true)`). Verified against the
vendored source: `TabStopMap::next` walks the painted frame's tab stops, and a fresh `FocusHandle`
defaults to `tab_stop: false` (`gpui-0.2.2/src/window.rs:286`), so the settings panel's own handle is
not even a candidate.

**Failure scenario.** `Ctrl+,` then `Tab`. Focus lands in the URL bar behind the scrim. The panel is
still on screen, but its leaf key context is gone, so `up`/`down`/`left`/`right`/`enter`/`escape`
under `Some("SettingsPanel")` no longer match — and bare `escape` now resolves to the global
`CancelRequest`. The panel is undismissable from the keyboard; only a click on the scrim closes it.
Same shape for the picker.

**Fix.** Either return early in `focus_next`/`focus_prev` while `self.picker.is_some() ||
self.settings.is_some()`, or scope both bindings away from the modal contexts. The early return is
cheaper and matches how every other handler guards.

### 6. `Ctrl+P` and `Ctrl+K` stack a second modal over the settings panel

> **Fixed with #5.** All seven openers now go through `modal_open`, so the drift that produced this
> can't recur in a new opener. Guarded by `a_picker_cannot_open_over_the_settings_panel`, which also
> asserts the panel is still dismissable afterwards.

Four of the six modal openers guard on both modals; two guard on only one.

| Opener | Guard |
|---|---|
| `open_settings` | `settings.is_some() \|\| picker.is_some()` ✅ |
| `show_history`, `switch_environment`, `open_method`, `open_body_type` | both ✅ |
| `open_request` [:271](app/src/workspace.rs#L271) | `picker.is_some()` only ❌ |
| `open_palette` [:337](app/src/workspace.rs#L337) | `picker.is_some()` only ❌ |

With the settings panel open, `Ctrl+P` opens a picker on top of it. Dismissing the picker restores
focus to the URL bar (`show_picker` captured `restore` from the active view, not from the panel),
leaving the panel visible and inert — the same dead-keymap end state as #5, reached a different way.

**Fix.** Add `|| self.settings.is_some()` to both. Consider a single `fn modal_open(&self) -> bool`
so the next opener can't get it wrong.

### 7. The window-control buttons never call `stop_propagation`, though the comment says they do

> **Fixed.** The buttons now stop propagation before acting, and the comment describes the mechanism
> rather than asserting a call. Guarded by
> `clicking_a_window_control_does_not_also_start_a_window_drag`.
>
> **Confirmed empirically, not just by reading the dispatch code.** With the fix reverted, the test
> fails with a panic at `gpui-0.2.2/src/platform/test/window.rs:289` — `start_window_move`'s
> `unimplemented!()` — which is direct proof the titlebar's drag handler was running. The test fires
> a bare mouse-*down* rather than `simulate_click`, because this button removes the window and the
> paired mouse-up would then land on a window that no longer exists and panic inside gpui's own test
> context. Minimize and maximize can't stand in: their platform calls are `unimplemented!()` too, so
> they panic either way.

[app/src/chrome.rs:143-148](app/src/chrome.rs#L143-L148)

```rust
// `stop_propagation` matters: without it the titlebar's drag handler also fires
// and the compositor starts a window move instead of registering the click.
.on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, _| {
    action(window);
})
```

The third closure parameter — the `&mut App` that `cx.stop_propagation()` would be called on — is
bound to `_`. The call described by the comment does not exist. `picker.rs:301` and
`settings_panel.rs:299` both do it properly, so this reads as an omission rather than a decision.

**Verified, not assumed.** `Div::on_mouse_down` registers for `DispatchPhase::Bubble`
(`gpui-0.2.2/src/elements/div.rs:121-135`), and `Window::dispatch_mouse_event` runs every bubble
listener in reverse paint order, stopping only when `propagate_event` is cleared
(`window.rs:3695-3712`). The titlebar is the button's ancestor and its hitbox contains the click, so
its handler runs immediately after the button's.

**Failure scenario.** Click minimize: the window minimizes *and* `start_window_move()` is called on
the way out. Click maximize: it zooms and starts a move. What the user sees depends on the
compositor — a spurious grab, a window that jumps, or nothing at all — which is why this can sit
unnoticed while being wrong.

**Fix.** Take the third parameter and call `cx.stop_propagation()` before `action(window)`.

### 8. `ResponseDiff::between` runs on the UI thread

> **Fixed.** `RequestView::diff_against` computes it on the background executor and hands back only
> the finished `ResponseDiff`, so `diff` is `None` for a frame or two after a response lands — the
> same deal the body index has always had, and `response_pane` already renders a missing diff as no
> diff bar. The task is held in a `diff_task` field so assigning a new one cancels a superseded
> diff, matching `body_task`.
>
> Guarded by the three pre-existing diff tests, which now exercise the async path and **time out**
> if the result never lands (verified by dropping the task: they fail in 10s with "timed out waiting
> for a diff"), plus a new `the_diff_describes_the_two_most_recent_runs`. That one uses *three* runs
> on purpose — with two, "diffed against the previous run" and "diffed against the first run" are
> the same assertion.

Every other piece of response handling is pushed to the background executor with a comment citing
invariant 3. The diff isn't: it runs inline in `apply`'s `Event::Done` arm.

[app/src/request_view.rs:599-601](app/src/request_view.rs#L599-L601)

`ResponseDiff::between` does a full `Bytes` comparison (`previous.body != current.body`) plus
`count_lines` over **both** bodies — a byte-at-a-time `filter().count()`
([core/src/diff.rs:117-128](core/src/diff.rs#L117-L128)). For two 10MB responses that is ~30MB of
scanning inside the same frame that then has to lay out and repaint the pane, three lines before
`index_body` carefully hands 10MB to a background task for the same reason.

**Fix.** Either move the diff into the same background task as the body index (it needs only the two
`Bytes` and the two headers/timings, all cheap to clone), or compute `body_changed` and `line_delta`
there and keep the header/status comparison inline. The first is cleaner and makes the invariant
uniform.

### 9. Every send synchronously serializes and writes every open buffer

> **Fixed.** `session::save_in_background` serializes and writes on the background executor;
> `session::save` is kept for the two places that must block. Assembling the `Session` still happens
> on the UI thread — only it can read the buffers — which is why the new function takes an owned
> `Session` rather than a reference: the UI-thread part is a clone, not a format.
>
> The quit hook now drops any pending background write **before** writing synchronously, or a
> checkpoint queued by a send moments earlier could land after it and put older state back on disk.
>
> Guarded by the two pre-existing persistence tests, which fail with "No such file or directory"
> if the write task is dropped rather than held (verified). No bespoke test for the quit ordering:
> `on_app_quit` fires at test teardown, so asserting after it isn't reachable from a test — the
> ordering is enforced by the code and stated in the comment instead.

[app/src/workspace.rs:1205](app/src/workspace.rs#L1205)

```rust
crate::session::save(&self.session(cx), cx);
```

`session()` calls `view.spec(cx)` for **all** views — each cloning the URL, every header name and
value, every query row, and the entire body string — then `serde_json::to_vec_pretty` over the lot,
then a blocking `fs::write`. All on the UI thread, in the handler for a keystroke.

The checkpoint-on-send policy is right; the placement fights the project's own numbers.
`architecture.md` §8 budgets **< 5 ms** for "Send keypress → bytes on wire", and the M2 goal is
"50 requests open across collections". Fifty buffers with 20 KB bodies is ~1 MB of cloning and
pretty-printing plus a synchronous disk write before the request is even built. `resolver()` adds
two more file reads and JSON parses on the same path
([workspace.rs:412-430](app/src/workspace.rs#L412-L430)) — that one is at least acknowledged in a
comment.

**Fix.** Build the `Session` on the UI thread (it must reflect the current frame) and move
`to_vec_pretty` + `write` to `cx.background_executor()`. The write already tolerates failure, so
nothing depends on it completing before the send. The `on_app_quit` path must stay synchronous —
that one is correct as written and the comment explains why.

### 10. Response bodies have no size cap

> **Fixed.** `run::MAX_BODY_BYTES` (100MB) caps the transfer, checked twice: against a declared
> `Content-Length` before any body moves, and again while streaming, since the declaration is a
> claim and a chunked response makes none. It **fails** rather than truncating — a truncated body
> would let `SaveResponse` write a corrupt file and make the viewer report a parse error at the cut.
> `EngineError::BodyTooLarge` is deliberately not `is_local`: the request went out and the server
> answered.
>
> The limit is a *parameter* of `execute` rather than a constant read inside it, which is what lets
> `a_streamed_body_past_the_limit_fails_instead_of_buffering_without_bound` drive the streaming
> guard with 64KB instead of moving 100MB through a socket. The pre-check has its own test over the
> public API. Both were confirmed independently load-bearing; with the pre-check disabled the same
> request instead fails as `IncompleteBody` after 2.06s rather than 0.16s, which is exactly the
> wasted transfer it exists to avoid.
>
> Left alone: making this a `RequestSettings` field. 100MB is a policy guess, and if a legitimate
> download ever needs more, a setting is the right answer rather than a larger constant.

`run.rs` streams the whole body into a `Vec<u8>` with no ceiling. `MAX_PREALLOC` caps the
*pre-allocation* from a hostile `Content-Length`, not the buffer's growth.

[core/src/engine/run.rs:78-108](core/src/engine/run.rs#L78-L108)

The documentation reasons carefully about memory for the *index* — `body_view::MAX_AUTO_PARSE` is
10 MB with a long comment explaining that 1.3 M rows cost more than the body — and not at all about
the body itself. Then `HISTORY_LIMIT = 10` retains ten more of them per buffer, and `Bytes` being
refcounted means each retained response pins its full allocation
([request_view.rs:61](app/src/request_view.rs#L61)).

**Failure scenario.** One mistyped URL pointing at a release artifact instead of an API endpoint:
a 2 GB `GET` buffers entirely into memory with a progress indicator and no way to say "too big".
Resend it eleven times and the eleven retained copies are ~22 GB. Both are OOM, from a typo, in a
tool whose whole premise is that you point it at arbitrary URLs.

**Fix.** A `MAX_BODY_BYTES` in `run.rs` that emits `Failed` (or a new `BodyTooLarge` carrying the
truncation point) once `buffer.len()` passes it. The existing "cap it, and *say so* in the UI"
pattern from `MAX_AUTO_PARSE` is the right precedent — `architecture.md` §6 already argues that a
silent truncation is a trust bug.

### 11. `SizeInfo.wire` can never show what the docs say it shows

> **Fixed, and the audit's reasoning was verified rather than assumed before doing so.** reqwest
> 0.13 delegates decompression to `tower_http::decompression`, whose response future calls
> `entry.remove()` on `Content-Encoding` and `headers.remove(CONTENT_LENGTH)` — but *only* when it
> actually decodes. So the wire size really is unrecoverable, and the audit's conclusion stands.
>
> Rather than documenting around it, the type now tells the truth: `wire: u64` became
> `declared: Option<u64>`, holding the server's claim rather than a measurement, and dropping the
> `unwrap_or(decoded)` that made "the server said nothing" indistinguishable from "the server said
> exactly what arrived". That is the same admission `Timing`'s connection stages already make. The
> label reads "N received · M declared" and only when they disagree.
>
> Guarded by `a_compressed_response_is_decoded_and_reports_no_declared_length`, which exists to pin
> a *dependency's* behaviour our docs now rest on: if a future reqwest preserves those headers, the
> test fails and the ratio has become showable. The gzip payload is a checked-in literal — and the
> first version of it gzipped a 27-byte string into 47 bytes, so the test caught my own wrong
> assumption about compression before it reached the assertion it was meant to support.

[core/src/engine/run.rs:131-134](core/src/engine/run.rs#L131-L134) — `wire:
declared_length.unwrap_or(decoded)`, where `declared_length` is `Response::content_length()`.

reqwest returns `None` from `content_length()` exactly when it has transparently decompressed the
body (it strips `Content-Length` and `Content-Encoding` from the headers at the same time). So:

- compressed response → `content_length()` is `None` → `wire = decoded`;
- uncompressed response → `content_length()` equals the decoded length → `wire = decoded`.

`wire == decoded` on every normal response, which makes the interesting branch of `size_label`
unreachable in practice ([response_pane.rs:248-258](app/src/response_pane.rs#L248-L258)) and makes
this claim in `architecture.md` §3.2 false:

> `size: SizeInfo, // wire vs decoded — both are interesting` … "the ratio is how you spot whether
> compression actually happened"

Where the two *can* differ — a `HEAD` or `304` that declares a length and returns no body — the
difference isn't compression, so the label reads "1.2 KB on the wire · 0 B decoded" for a response
that was never compressed at all.

**Fix.** This is an inherited reqwest limitation, so the honest fix is documentation, matching how
`Timing::dns`/`connect`/`tls` are handled: record it as a known limitation in §3.2 and note that
recovering the wire size needs a lower-level client. If the number is wanted, it has to be captured
before decompression, which means a custom decoder — a much larger change than the display suggests.

### 12. Imported curl requests silently follow redirects and accept compression

> **Fixed.** The imported spec now starts from *curl's* defaults for the two wire-observable
> settings — `follow_redirects: false`, `accept_encodings: false` — so the flags turn them on and,
> more importantly, their absence means what curl means by it. `timeout` and `max_redirects`
> deliberately keep Zuno's values; the reasoning (local guard vs. wire-observable) is in `parse`
> and in architecture.md's M1.5 notes.
>
> Guarded by `absent_flags_keep_curls_behaviour_rather_than_zunos`, and see #19 — this fix is what
> made the *existing* test able to fail.

[core/src/curl.rs:180-181](core/src/curl.rs#L180-L181)

```rust
"-L" | "--location" => spec.settings.follow_redirects = true,
"--compressed" => spec.settings.accept_encodings = true,
```

`RequestSettings::default()` already sets both to `true`
([core/src/request.rs:217-228](core/src/request.rs#L217-L228)), so both arms are no-ops — and,
more importantly, the *absence* of the flag is never honoured. curl does not follow redirects
without `-L` and does not send `Accept-Encoding` without `--compressed`.

This contradicts the module's own stated principle, four lines from the top of the file:

> 2. **Don't silently change what the request does.**

`-k` works because the polarity happens to line up (default `verify_tls: true`, flag turns it off).
The two flags whose polarity doesn't line up are both wrong.

**Failure scenario.** `curl https://api.test/redirects-to-login` (no `-L`) imports as a request that
follows the redirect, so you see the login page's 200 instead of the 302 you were investigating —
a different answer than the command you pasted gave.

**Fix.** Set the settings from flag *presence*: initialise `follow_redirects = false` and
`accept_encodings = false` on the imported spec, then let `-L`/`--compressed` turn them on. See #16
for why the current tests don't catch this.

---

## Low

### 13. `EngineError::UnresolvedVariable` still says environments are coming in M2

> **Fixed.** Now "add it to an environment, or select one that defines it". Deliberately names no
> keystroke: `core` cannot see the keymap, so a `Ctrl+E` in that string would be the same kind of
> claim that can quietly stop being true.
>
> Guarded by `an_unresolved_variable_reads_back_as_a_placeholder`, which checks the message reads
> back `{{baseUrl}}` — four levels of brace escaping sit in that format string and `{baseUrl}` would
> name something the user never typed. The staleness itself isn't test-catchable; no assertion knows
> that a milestone has passed.

[core/src/engine/error.rs:27](core/src/engine/error.rs#L27)

```rust
#[error("{{{{{name}}}}} is not defined (in {location}) — environments arrive in M2")]
```

Environments shipped in M3. A user with a typo'd `{{baseUrl}}` is told that the feature they are
already using doesn't exist yet. This is the single most user-visible piece of drift in the tree.

**Fix.** Something actionable instead — `"… — define it in an environment, or press Ctrl+E to
select one"`.

### 14. The in-flight pane advertises a keystroke that isn't bound

> **Fixed structurally rather than textually.** `in_flight` takes the keystroke as a parameter and
> `render` reads it from the live keymap through `workspace::keybinding_hint` — the helper the
> command palette already uses for the same reason. There is no longer a string to go stale, and if
> nothing is bound the pane says "waiting for the first byte" rather than inventing an instruction.
>
> Guarded by `the_in_flight_hint_names_a_key_that_actually_cancels`, which reads whatever the pane
> would advertise and *presses it*, then asserts the request was abandoned. Verified by removing the
> `escape` binding: the test fails.
>
> **Honest limit:** the test drives the keymap and the helper, not the pane's rendered text, so it
> would not catch someone hardcoding the string again. What prevents that is that there is no string
> at the call site to hardcode — a structural guard, not a tested one.
>
> architecture.md §4's sketch, which said "Wire `Ctrl+C`", is corrected in place: that sentence is
> the likely origin of the wrong hint.

[app/src/response_pane.rs:101](app/src/response_pane.rs#L101) — `"Ctrl+C or Escape to cancel"`.

`ctrl-c` is bound only to `text_input::Copy` under the `TextInput` context; nothing binds it to
`CancelRequest`. With focus in the URL bar (the default) `Ctrl+C` copies the selection; elsewhere it
does nothing. The `architecture.md` §4 sketch says "Wire `Ctrl+C` / a re-`Send`", which is probably
where the string came from.

**Fix.** Drop "Ctrl+C or" from the hint, or bind it. Binding it is a bad idea — `Ctrl+C` in a text
field must copy — so the string should change.

### 15. `EngineError::UnsupportedBody` is dead

> **Fixed.** Variant and its `is_local` arm deleted; `grep` confirms no references remain and the
> workspace compiles clean under `-D warnings`. No test — there is nothing to assert about a variant
> that no longer exists, and the compiler is the guard. ROADMAP's claim that it "is gone" is now
> true rather than aspirational.

Declared at [error.rs:45-46](core/src/engine/error.rs#L45-L46), matched in `is_local` at
[:127](core/src/engine/error.rs#L127), constructed nowhere (verified by grep). `ROADMAP.md` states
"`UnsupportedBody` is gone" — it isn't. Invariant 1: *"Don't leave speculative API behind 'for
later' — delete it and re-add when there's a caller."*

**Fix.** Delete the variant and its `is_local` arm. (If #10 lands, `BodyTooLarge` is a new variant,
not a reuse of this one.)

### 16. `label_from_url` mistakes a path segment containing `:` for the authority

> **Fixed.** A colon is authority evidence only in the *first* segment, established without
> allocating by taking `next()` and then `next_back()` — `None` from the second call means there was
> only one segment. Guarded by `a_path_segment_may_contain_a_colon`, which covers both directions:
> `/v1/files:batchUpdate` and `/v1/models/x:predict` keep their verb, while `localhost:8080` alone
> is still a host and port.

[core/src/request.rs:328-333](core/src/request.rs#L328-L333)

```rust
if segment.is_empty() || segment.contains(':') {
    // Bare host, or host:port — the segment found was the authority, not a path.
```

The heuristic is right for `localhost:8080` and wrong for a path segment that legitimately contains
a colon. Google-style REST — `POST /v1/files:batchUpdate`, `/v1/models/x:predict` — is the common
case, and gRPC-transcoded APIs use it throughout.

**Failure scenario.** `https://api.test/v1/files:batchUpdate` labels as `api.test`, so the tab reads
`api.test`, the window title reads `api.test — Zuno`, and `Ctrl+S` writes `api.test.json`. Two
different `:verb` endpoints on the same host collide into `api.test.json` and `api.test-2.json`,
which defeats the readable-directory goal the naming scheme exists for.

**Fix.** Only treat the colon as authority evidence when the segment is the *first* one (i.e. the
path had no `/`). The segment is already known to be the last one, so the check is
`path.split('/').filter(non-empty).count() == 1`.

### 17. IME selection mapping is inconsistent between the two editors, and `text_input`'s looks wrong

> **Fixed.** `text_input` now offsets both ends by `range.start`, matching `editor.rs`. Recorded as a
> sixth deliberate divergence from `examples/input.rs` in both the module header and
> architecture.md §7.
>
> Guarded by `a_composed_replacement_leaves_a_copyable_selection`, driven through
> `EntityInputHandler` directly because that is the surface an IME talks to and no keystroke reaches
> it. It replaces a *non-empty* range, which is the only case where the two forms differ, then
> copies — because the real consequence is not a wrong highlight but a panic: with the bug the stored
> selection is `1..6` over five bytes of content, and `copy` slices with it. Reverting gives
> `1..5` against an expected `1..3`.

[app/src/input/text_input.rs:427-434](app/src/input/text_input.rs#L427-L434)

```rust
.map(|new_range| new_range.start + range.start..new_range.end + range.end)
```

versus [editor.rs:520-523](app/src/input/editor.rs#L520-L523), which offsets both ends by
`range.start`:

```rust
.map(|r| r.start + range.start..r.end + range.start)
```

`new_selected_range_utf16` is relative to the inserted text, so both ends should shift by
`range.start`. Adding `range.end` to the end is only harmless when `range.start == range.end` — the
empty-insertion-point case, which is most of them, which is why nobody has hit it. Replacing a
non-empty marked range or a selection during composition gives a selection that is too long by the
replaced range's width. The `text_input` form is inherited from `examples/input.rs`; `editor.rs` is
the corrected copy, so the codebase already contains the fix.

**Fix.** Make `text_input` match `editor`. Worth a comment saying it deliberately diverges from the
upstream example — that list is already in the module header and this would be a sixth entry.

### 18. Non-UTF-8 header values render as a Rust debug array

> **Fixed.** `String::from_utf8_lossy` instead of `format!("{:?}", bytes)`, so an undecodable byte
> becomes U+FFFD and the rest of the value stays readable.
>
> Guarded by `a_non_utf8_header_value_is_readable_rather_than_a_byte_dump`, which serves a latin-1
> filename in `Content-Disposition` — the case that actually turns up. Reverting renders
> `[97, 116, 116, 97, 99, 104, ...]`, which is the finding in one line.

[core/src/engine/run.rs:157](core/src/engine/run.rs#L157) —
`.unwrap_or_else(|_| format!("{:?}", value.as_bytes()))`

The intent (show it rather than drop the header) is right; the rendering produces
`[104, 105, 255]` in the headers table. A latin-1 filename in a `Content-Disposition` — the common
real case — becomes unreadable digits.

**Fix.** `String::from_utf8_lossy(value.as_bytes())`, which keeps the readable prefix and marks the
bad bytes with U+FFFD. Hex would be defensible too; a decimal debug array isn't.

---

## Test quality

The suite is strong — three layers, real keystrokes, real sockets, and assertions on bytes a server
actually received. Two specific problems.

### 19. `settings_flags_are_applied` is a weak assertion of the exact kind CLAUDE.md documents

> **Fixed, and instructively.** The assertion was vacuous *because of the bug in #12* — the default
> it asserted was the defect. So fixing the code turned the existing test load-bearing without
> touching it: with `-L` deleted from the parser, `settings_flags_are_applied` now fails, where
> before the same deletion left it green. Verified both ways.
>
> `output_flags_are_silently_dropped_because_they_mean_nothing_here` also gained an assertion that
> `--compressed` took effect, since `ignored.is_empty()` alone passed whether or not it did.
> Recorded as the fifth instance in CLAUDE.md's weak-assertion lesson, with the note that this one
> was hiding a bug rather than merely failing to catch one.

[core/src/curl.rs:689-694](core/src/curl.rs#L689-L694)

```rust
let spec = import("curl -k -L --max-time 5 https://x.test/a").spec;
assert!(!spec.settings.verify_tls);
assert!(spec.settings.follow_redirects);   // <- true by default
assert_eq!(spec.settings.timeout, Some(Duration::from_secs(5)));
```

`follow_redirects` is `true` in `RequestSettings::default()`, so this line passes whether or not the
`-L` arm exists. Delete `"-L" | "--location" => …` from `parse` and the test still goes green. Same
for `--compressed` in `output_flags_are_silently_dropped_because_they_mean_nothing_here`
([:726-729](core/src/curl.rs#L726-L729)), which only asserts `ignored.is_empty()`.

This is the fifth instance of the pattern the "Lessons" section names — *"A weak assertion reads
exactly like a strong one"* — and it is hiding a real bug (#12), which is what makes it worth
calling out rather than filing under cleanup. Apply the documented remedy: break the `-L` arm on
purpose and watch the test not fail.

**Fix.** Assert the negative case too: `import("curl https://x.test/a")` must yield
`follow_redirects == false` and `accept_encodings == false`. That test fails today, which is the
point.

### 20. Coverage gaps that line up exactly with the findings above

Not a general complaint — these are the specific untested paths:

| Untested | Finding |
|---|---|
| `body_label()` on a fresh (`Empty`) buffer, and the picker's `current` marker for it | #3 |
| ~~The body-kind chip's click path — `cycle_body_kind` has one caller and zero tests~~ — fixed with #4 | #4 |
| ~~`Tab` while any modal is open~~ — fixed with #5 | #5 |
| ~~`Ctrl+P` / `Ctrl+K` while the settings panel is open~~ — fixed with #6 (`a_second_ctrl_p_does_not_nest_a_modal` covered only picker-over-picker) | #6 |
| Whether closing a tab cancels its in-flight job | #2 |
| Variable substitution into a form or multipart body | #1 |
| An imported curl command *without* `-L` / `--compressed` | #12 |

Also: `the_body_kind_cycles` no longer tested cycling — its body drives the picker, and its own
comment said so, leaving the *name* as the last thing claiming the cycling path was covered.
Renamed to `the_body_sub_kind_is_chosen_by_name` alongside #4.

### 21. `folding_a_huge_document_is_cheap` can't fail for the reason it exists

> **Fixed, and the old bound's uselessness was demonstrated rather than argued.** The assertion is
> now `collapsed * 20 < unfolded`, holding the property that actually matters: folding an open row
> *skips* `subtree_len` forward rather than stepping through it.
>
> To prove the new bound earns its place, `visible_rows` was rewritten to produce **identical
> output** with no short-circuit — visiting all 1.3M rows. That gives `collapsed` 6.61ms against
> `unfolded` 19.87ms: the old `collapsed < unfolded * 2` **passes** that (6.61 < 39.7), the new one
> fails it. So the old assertion could not have caught a real regression, which is exactly what the
> finding claimed. Measured ~4000x in a debug build when correct, so 20x is a wide margin.

[core/tests/json_perf.rs:93-96](core/tests/json_perf.rs#L93-L96)

```rust
assert!(collapsed < unfolded.max(Duration::from_millis(1)) * 2, ...);
```

The file's own header says the property under test is that `visible_rows` "is O(rows) and not
O(bytes)". But both measured calls are O(rows) — folding the root shortens the *output*, not the
scan — so the ratio between them can't distinguish the two complexities. The measured numbers in
`architecture.md` (6.7 ms vs 7.3 µs, ~900×) show what the assertion could be: `collapsed` should be
orders of magnitude below `unfolded`, not merely within 2×.

**Fix.** `assert!(collapsed * 10 < unfolded)` or similar. The absolute bound above it
(`unfolded < 500ms`) is the one doing real work today and should stay.

---

## Cleanup

Small, verified, low-risk. Grouped because none of them individually justifies a section.
**All five are done**; each entry records what happened.

> **4 turned out not to be cosmetic.** Hoisting the query collection out of the per-candidate path
> meant `score`'s empty-query shortcut had to move too — and without it the general loop scores an
> empty query as `-trailing`, which sorts an unfiltered picker *by label length* and throws away the
> order the caller assembled (buffers before files, environments in scan order). Caught by a test
> written for the hazard rather than by review:
> `an_empty_query_keeps_the_callers_order_regardless_of_length` fails with
> `["b", "medium-name", "a-very-long-name"]` when the shortcut is removed.

1. **Stale `#[allow(dead_code)]`.** [theme.rs:70-73](app/src/theme.rs#L70-L73) and
   [:80](app/src/theme.rs#L80) suppress dead-code warnings on `Theme::syntax` / `SyntaxTheme` with
   the comment "Unread until the JSON viewer lands in M1.3". M1.3 shipped; `response_pane::json_row`
   reads every field. Both attributes and the comment should go — a stale `allow` is how the *next*
   unused field hides.
2. **`settings_panel` documents a call it doesn't make.** [:131-134](app/src/settings_panel.rs#L131-L134)
   says "`tab_stop(false)` because Tab is not how you move within a modal". `cx.focus_handle()`
   already defaults to `tab_stop: false` (verified in `gpui-0.2.2/src/window.rs:286`) and nothing
   calls it. Describe the default, or make it explicit — as written it reads like a guarantee
   someone can delete. (See also #5: the default is *why* Tab escapes the modal.)
3. **`Resolver::apply` clones every string before resolving it.**
   [environment.rs:163-170](core/src/environment.rs#L163-L170) — `self.resolve(&param.name.clone())`.
   The clone is working around a borrow conflict that a temporary binding solves:
   `let resolved = self.resolve(&param.name).into_owned(); param.name = resolved;`. Four needless
   allocations per row per send.
4. **`fuzzy::score` re-collects the query per candidate.**
   [fuzzy.rs:51-52](core/src/fuzzy.rs#L51-L52) builds `needles: Vec<char>` inside `score`, so
   `rank` over N candidates allocates it N times per keystroke. Hoisting it into `rank` (or taking
   `&[char]`) removes N allocations from every keypress in the picker. The module's "revisit if
   ranking shows up in a profile" note covers the algorithm, not this.
5. **`Ctrl+P` matches only the tab label, not the URL.** `open_request` puts the URL in `detail`, and
   `refilter` ranks against `label` alone ([picker.rs:165-166](app/src/picker.rs#L165-L166)). For
   buffers the label is one path segment, so you can't find an open request by typing its host. For
   saved requests the label is the relative path, which is fine. Probably deliberate; worth a note
   in the module if so, since the row visibly shows text you can't search.

---

## Checked and sound

Recorded so a future pass doesn't re-derive it — these are the places I went looking for bugs and
didn't find them.

- **`flatten.rs`** — iterative, no unchecked indexing, `read_literal`'s `self.src[self.pos..]` is
  safe at `pos == len`, offsets can't overflow `u32` because `JsonOutline::parse` rejects
  `len > u32::MAX` before calling it, and structural errors are rejected where being permissive
  would produce nonsense. The 50,000-deep test earns its place.
- **`LineIndex::line`** truncation backs off correctly to a UTF-8 boundary; `bytes[end]` cannot be
  out of range because the branch is guarded by `len > MAX_DISPLAY_LINE`.
- **`compute_line_starts` vs `LineIndex`** genuinely need opposite trailing-newline behaviour, and
  both comments say why. This is the kind of thing that looks like an inconsistency and isn't.
- **`session::parse`** — the version dispatch is right, `active` is clamped and empty `tabs` rejected
  *before* returning, which is what makes `views[active_ix]` in `Workspace::new` safe. Invariant 8
  is correctly implemented: each old version has its own spelled-out struct.
- **`collection::slug`** is genuinely a containment boundary and holds: separators mapped, `.`/`..`
  and empty handled, truncation on a char boundary. Both layers tested independently, as the comment
  claims.
- **`scan`'s symlink handling** — recursing on `is_dir()` rather than filtering `is_file()` really
  does prevent cycles while still following a symlink to a request file, and `MAX_DEPTH` backs it up.
- **`is_folded_at`** — the inference is sound: an unfolded open row is always followed by `ix + 1`
  (its first child or its own close), and the last-visible-row case correctly returns `true`.
- **Keymap registration order** — the tie-break reasoning in `main.rs` matches
  `Keymap::binding_enabled` in the vendored source: a `None` context scores maximum depth, so later
  registration is what makes `escape` close the picker. The comment is correct and load-bearing.
- **Picker close-before-act ordering** — correct, and correctly explained. `dispatch_action` defers,
  `activate` focuses synchronously, so the order only matters for `Buffer`/`File`.
- **`Event::Done` dropping its own `Task`** — `self.inflight = None` drops the `Task` for the future
  currently executing, and the remaining statements in the arm still run because async-task defers
  cancellation of a running task. It works, but it is load-bearing and undocumented: any future code
  added *after* `this.update(...)` in that closure would silently not run. Worth one line of comment
  at [request_view.rs:595](app/src/request_view.rs#L595).
