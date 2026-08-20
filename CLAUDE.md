# Zuno — working notes for agents

A native API client in Rust + GPUI. The thesis: **Postman-level capability, Zed-level feel** —
speed and keyboard navigation are requirements, not polish.

Three docs, three jobs. Read them in this order:

- **`ROADMAP.md`** — what to build next and *why in that order*. Its four sequencing principles
  matter more than its feature lists.
- **`architecture.md`** — design truth. Records not just how things work but what was tried and
  abandoned (`serde_json::Value`, `ropey`, splitting query params), so you don't re-derive a dead
  end. §11 lists capabilities that are already built but unreachable from the UI — check it before
  building anything.
- **`CLAUDE.md`** (this file) — mechanics: commands, invariants, and the traps.

Milestone 1 is complete: the request → response loop works end to end. **M2 is complete too** —
tabs, collections, `Ctrl+P`, and `Ctrl+K`. The navigation thesis is built; M3
(environments and variables) is next.

## Layout

A cargo workspace with two members:

- **`core/`** (`zuno-core`) — request/response model, HTTP engine, JSON flattening, curl import,
  diffing. **Never imports GPUI**, which is compiler-enforced by the split, so it can be tested
  without a window and reused by a future CLI.
- **`app/`** (`zuno`) — the GPUI binary. Views, text editing, theme, window chrome.

## Commands

```bash
cargo check --workspace --all-targets    # the fast loop (~0.5s warm)
cargo test --workspace                   # 260 tests, ~6s
cargo test -p zuno-core                  # core only, no GPUI link
ZUNO_TIMING=1 cargo run                  # boot stages + per-request + body-index timings

# Live HTTPS check — #[ignore]d so CI never depends on the network.
cargo test -p zuno-core --test engine -- --ignored --nocapture

# Perf floor for the response viewer. Release, or the numbers are meaningless.
cargo test --release -p zuno-core --test json_perf -- --nocapture

# Build the Debian package. Inspect before trusting it.
cargo deb -p zuno
dpkg-deb --info target/debian/*.deb && dpkg-deb --contents target/debian/*.deb
```

Debug startup is ~4× slower than release; don't judge feel from a debug build.

## Invariants

Breaking any of these is a bug, not a tradeoff.

1. **Zero warnings.** CI sets `RUSTFLAGS: -D warnings`. Don't leave speculative API behind
   "for later" — delete it and re-add when there's a caller.
2. **`zuno-core` never imports GPUI.**
3. **Nothing parses or formats on the UI thread.** Body indexing, JSON flattening, and UTF-8
   validation go to `cx.background_executor()`. Only a finished index crosses back.
4. **Response bodies are `Bytes`, never `String`.** Binary and invalid UTF-8 are normal.
5. **Check the registry before writing a version string.** `cargo info <crate>`. Never write one
   from memory — see "Lessons" below.
6. **Tests must never write to the developer's own files.** `session::install_at(cx, None)` and
   `collections::install_at(cx, None)` in the test harness — the suite drives `SendRequest` and
   `SaveRequest`, and both are save points. `~/.config/zuno` holds the session,
   `~/.local/share/zuno/collections` the requests.
7. **New `RequestSettings` fields need serde defaults.** The container carries
   `#[serde(default)]`; keep it. `RequestSpec` stays strict on purpose.
8. **`session::Session`'s fields stay required — no `#[serde(default)]`.** A required `version`
   is what lets `parse` tell an envelope apart from M1's bare `RequestSpec`. Default `tabs` and a
   legacy file parses as an envelope with zero tabs, silently discarding the user's request
   instead of migrating it. Change the shape by bumping `CURRENT_VERSION` and adding an arm to
   `parse`'s version dispatch, with a test per migration — there are three now (bare spec, v1, v2).
9. **Collection files never carry `RequestId`.** It's written as 0 and reassigned on open. A
   session-local handle in a committed file is diff churn and invented merge conflicts.

## GPUI 0.2.2 — verify, don't remember

**Read the vendored source before writing GPUI code:**

```
~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-0.2.2/
```

This matters more than usual. Nearly every GPUI example online tracks Zed's `main` branch, which
differs from the published `0.2.2` crate we're pinned to — so recalled APIs are frequently wrong
in ways that compile-fail at best and mislead at worst. `examples/` in that directory is the best
reference available; `src/` settles any question in seconds.

Style helpers (`rounded_md`, `border_b_1`, `min_w`, `cursor_pointer`) are **macro-generated** —
grep `gpui-macros-0.2.2/src/styles.rs`, not just `gpui-0.2.2/src/styled.rs`, or you'll conclude
they don't exist.

### Traps, all of which cost real time

| Trap | What to do |
|---|---|
`.id()` returns `Stateful<Div>`, not `Div` | Return `impl IntoElement` from render helpers. This bit three separate milestones. |
Key context predicates match **only the leaf** context (`contexts.last()`) | Put both identifiers in one string: `"TextInput UrlBar"`. Nesting a `key_context` div does **not** work. |
A focus handle needs explicit `.tab_stop(true)` | Otherwise `focus_next()` skips it silently. |
No `cx.background_spawn` in 0.2.2 | Use `cx.background_executor().spawn(fut)`. |
`overflow_*_scroll` is on `StatefulInteractiveElement` | Requires `.id()` first. |
`truncate()` does **not** clip custom-painted elements | Needs a real `overflow_hidden()`. And clipping alone strands the hidden text — pair it with a scroll offset. |
`WindowOptions::window_decorations` defaults to `None` | Leaves the window client-decorated with nothing drawn: no buttons, no resize. We set `Client` and draw our own in `chrome.rs`. |
`TextSystem::shape_line` has a `debug_assert!` against newlines | Sanitize at the edit boundary, not just on paste. |
`examples/input.rs` ships macOS `cmd-` bindings | Translate every one to `ctrl-`. It also has a latent `assert_eq!` panic when a placeholder is showing. |
`TabStopNode` orders by tab_index path, **then** paint order | Leaving inputs at the default index 0 makes visual order the tab order for free. |
`Context::on_app_quit` is the correct save hook | Not the Quit *action* — that misses window-manager close. `cx.on_window_closed` is also needed, since GPUI doesn't quit on last-window-close. |
`impl Trait` returns capture **every** in-scope lifetime under edition 2024 | A render helper taking `cx: &mut Context<_>` and returning `impl IntoElement` borrows `cx` for the element's life — fine for one, but collecting several into a `Vec` is several simultaneous mutable borrows. Add `+ use<>` when nothing in the element actually needs the borrow (`cx.listener` returns an *owned* closure). See `settings_panel::setting_row`. |
`Window::dispatch_action` **defers** — it captures the focus id, then `cx.defer`s the dispatch | So an action dispatched from a modal still resolves against the frame the modal was in. It also means closing-then-dispatching and dispatching-then-closing behave identically for actions. Focus order *does* matter for anything that calls `window.focus` synchronously, like `activate`. |
A context-less binding does **not** lose to a specific one — it *ties*, and **later registration wins** | `binding_enabled` returns `depth = contexts.len()` for a `None` context, which is the maximum; the tiebreak is `ix_b.cmp(ix_a)`. So `escape` in `Some("Picker")` only beats the global `escape` -> `CancelRequest` because it is registered after it in `register_keymap`. Reordering that list changes behaviour with no compile error. |

## Packaging

`.github/workflows/release.yml` on a `v*` tag → `.deb` on a GitHub Release.
`workflow_dispatch` runs the same build without publishing. Four things here are
counter-intuitive enough that the workflow asserts each one rather than trusting it:

| Trap | Why |
|---|---|
**`libvulkan1` and `libwayland-client0` must be declared by hand** | gpui `dlopen`s both, so they never appear in `ldd` and `dpkg-shlibdeps` cannot see them. Omit them and the package installs cleanly, then dies at startup on a machine that happens to lack them. |
**Build on `ubuntu-22.04`, never `ubuntu-latest`** | glibc is backward- but not forward-compatible. 22.04 yields `libc6 (>= 2.35)` — installs on Ubuntu 22.04+ and Debian 12+. 24.04 yields `>= 2.39` and silently locks both out. The workflow fails if the floor moves. |
**The `.desktop` filename must equal the `app_id`** | `dev.zuno.Zuno.desktop`, matching `main.rs`. On Wayland the filename is the *only* link between window and icon; `StartupWMClass` covers X11. Rename either and the launcher shows a generic icon with no error anywhere. |
**GPUI 0.2.2 has no window-icon API** | `set_app_id` exists, `set_icon` doesn't — and Wayland has no client-side icon protocol at all. So the icon is purely a packaging concern: an SVG in `hicolor/scalable/apps/`. Nothing in app code can set it. |

`assets/icons/zuno.svg` is a **placeholder**; replacing that one file is the whole job, since
the `.desktop` key and the cargo-deb asset entry both point at it by name. Only the scalable
SVG is shipped — no PNG rasters — so there are no generated icon artifacts to go stale when
the source changes.

`mesa-vulkan-drivers` is a `Recommends`, not a `Depends`: a machine on the proprietary NVIDIA
driver already has an ICD and must not be forced to pull in Mesa.

## Testing

**GPUI has a headless test platform, and it is the main reason this codebase is trustworthy.**
`app/src/tests.rs` drives real keystrokes through the real keymap:

```rust
#[gpui::test]
async fn something(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);   // real keymap, theme, engine
    cx.simulate_keystrokes("ctrl-shift-h");    // keymap → context predicate → dispatch
    cx.simulate_input("X-Trace-Id");           // platform input handler → EntityInputHandler
}
```

Enabled by `gpui = { features = ["test-support"] }` as a **dev**-dependency, so `cargo build`
keeps the default feature set (`cargo test` recompiles GPUI once, ~35s).

Two patterns worth reusing:

- **`wait_for(cx, what, probe)`** — the engine runs on its own OS thread, so `run_until_parked()`
  returns while the consuming task is still awaiting its channel. Poll against a deadline.
- **Real sockets over mocks** — `core/tests/engine.rs` spins a throwaway `TcpListener` and
  asserts on *the request text the server actually received*. ~60 lines, no test dependencies.

Three test layers, and bugs have been caught at each: pure units (`core/src/**/tests`),
end-to-end over sockets (`core/tests/`), full-stack through keystrokes (`app/src/tests.rs`).

## Lessons that cost real time

- **A version written from memory was wrong.** `reqwest = "0.12"` while 0.13.4 was current — and
  for `0.x` crates the minor *is* the major, so `"0.12"` can never reach 0.13. Every other
  dependency was fine because `"1"`/`"2"` already meant the newest major. Run `cargo info`.
- **Adding a struct field broke every saved session.** `cookie_store` made existing
  `session.json` files fail with `missing field`, silently falling back to the sample request.
  Hence invariant 7.
- **The test suite overwrote a real session file** before `session::install_at` existed. Hence
  invariant 6.
- **`Url::parse("https://{{baseUrl}}/users")` succeeds** — it reads the placeholder as a
  hostname. Unresolved `{{…}}` is caught before parsing, in URLs and header values.
- **Similar-looking types can need opposite behavior.** `LineIndex` (display) drops a trailing
  newline's empty line; the editor's `compute_line_starts` must *keep* it, or the cursor has no
  row to sit on after pressing Enter. Both are commented.
- **Write the test that's awkward to write.** The `tab_stop`, `{{baseUrl}}`, and line-index bugs
  were all found by tests that felt like a chore.

## Conventions

- Comments explain **why**, not what. If a decision has a rejected alternative, name it.
- Derive state rather than mirroring it. `RequestView` has no stored `RequestSpec` —
  `spec(cx)` assembles one from the inputs, so what's sent can't disagree with what's on screen.
- Actions, not direct calls, for anything a button and a keybinding share. A command palette row
  dispatches the action too, so palette and keystroke can't drift.
- **Every new action needs a `commands::palette()` row or an `EXCLUDED` entry with a reason.** The
  drift test fails otherwise, on purpose: an action reachable only by a keystroke nobody remembers
  is the thing a palette exists to prevent.
- Every action handler lives on `Workspace`, because dispatch travels up the focus tree and
  `Workspace` is always on it.
- **Anything that changes which buffer is active goes through `Workspace::activate`.** A
  `FocusHandle` belongs to the entity that created it, so switching `active_ix` without moving
  focus leaves it inside the old view — and after a close, inside a dropped one, where no key
  context matches and the whole keymap goes dead with nothing on screen explaining why. Removing
  the focus move from `activate` fails five tests; that's deliberate.
- Errors are typed and renderable. `EngineError::is_local()` separates "nothing left the
  machine" from a network failure, which is a real distinction to a person debugging.
