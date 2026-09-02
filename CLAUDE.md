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

**M1, M2 and M3 are all complete**, and architecture.md §11 — engine capability with no UI path —
is empty. The loop, the navigation thesis, and reuse are all built; response search and the
body/headers tabs landed after. ROADMAP's audit section, not its milestone headings, is where the
remaining work lives.

## Layout

A cargo workspace with two members:

- **`core/`** (`zuno-core`) — request/response model, HTTP engine, JSON flattening, curl import,
  diffing. **Never imports GPUI**, which is compiler-enforced by the split, so it can be tested
  without a window and reused by a future CLI.
- **`app/`** (`zuno`) — the GPUI binary. Views, text editing, theme, window chrome.

## Commands

```bash
cargo check --workspace --all-targets    # the fast loop (~0.5s warm)
cargo test --workspace                   # 591 tests, ~20s
cargo test -p zuno-core                  # core only, no GPUI link
ZUNO_TIMING=1 cargo run                  # boot stages + per-request + body-index timings

# Live HTTPS check — #[ignore]d so CI never depends on the network.
cargo test -p zuno-core --test engine -- --ignored --nocapture

# Perf floor for the response viewer. Release, or the numbers are meaningless.
cargo test --release -p zuno-core --test json_perf -- --nocapture

# Build the Debian package. Inspect before trusting it.
cargo deb -p zuno
dpkg-deb --info target/debian/*.deb && dpkg-deb --contents target/debian/*.deb

# Cut a release: bump, refresh the lock, test, commit, tag. Pushes only with `--push`.
scripts/release.sh minor          # or patch / major / an explicit 0.2.0
```

Debug startup is ~4× slower than release; don't judge feel from a debug build.

## Invariants

Breaking any of these is a bug, not a tradeoff.

1. **Zero warnings.** CI sets `RUSTFLAGS: -D warnings`. Don't leave speculative API behind
   "for later" — delete it and re-add when there's a caller.
2. **`zuno-core` never imports GPUI.**
3. **Nothing parses or formats on the UI thread.** Body indexing, JSON flattening, UTF-8
   validation, the response diff, and the session write all go to `cx.background_executor()`; only
   a finished result crosses back. **The two that don't look like parsing are the two that were
   missed:** `ResponseDiff::between` compares both bodies byte-for-byte *and* counts the newlines
   in each, and `session::save` serializes every open buffer's `RequestSpec`, bodies included.
   Assembling the input often does need the UI thread, because only it can read entities — but
   that part has to be a clone, not a format. Blocking is allowed only where the write must land
   before the next thing happens, and only two places qualify: the quit hook and Ctrl+S.
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
   `parse`'s version dispatch, with a test per migration — there are four now (bare spec, v1, v2,
   v3). Each older version gets its own spelled-out struct rather than a defaulted field, so
   "written by an older Zuno" can't be confused with "written by this one, with everything empty".
9. **Collection files never carry `RequestId`.** It's written as 0 and reassigned on open. A
   session-local handle in a committed file is diff churn and invented merge conflicts.
10. **Environment secrets never touch the committed file.** `dev.json` is committed,
    `dev.local.json` is gitignored and overrides it, and that split *is* the secret marking —
    there's no per-variable flag to forget to set. Anything that writes environment values back to
    disk must preserve the split, or the collection format starts leaking tokens by design.

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
A `uniform_list` cannot be scrolled programmatically without `.track_scroll(handle)` | The handle is the only way in, and omitting it fails *silently* — `scroll_to_item` sets a deferred request that nothing ever consumes, so jumping to a search match just does nothing. Note the list addresses items by **visible** index, not row index, so anything folded above the target has to be translated through `visible` first. |
A dropped `Subscription` unsubscribes | `cx.subscribe` returns one; store it in a field (or `.detach()`). Let it fall out of scope and the callback silently never fires again. |
`impl Trait` with a generic parameter needs `use<A>`, not `use<>` | The return has to mention every type parameter in scope. `use<>` is only for helpers that are non-generic *and* borrow nothing. |
`truncate()` does **not** clip custom-painted elements | Needs a real `overflow_hidden()`. And clipping alone strands the hidden text — pair it with a scroll offset. |
`truncate()` is **not dependable** — shorten the string yourself | It resolves to `TextOverflow::Truncate(ELLIPSIS)`, but `TextState::layout` reads its width from `known_dimensions.width.or(available_space.width if Definite)` **and caches the first measurement**, so whether it fires depends on layout several elements away. It shipped twice not firing — once from `flex_1().min_w(0)`, once from an explicit `.w()` that should have worked by every reading of the source — and **each failure is silent**: no error, just the hard cut you were removing. Worse, it is **unobservable headlessly**: shaped text has no measurable width, a block wrapper stretches to its parent, and a flex wrapper feeds the text `MaxContent` and so *breaks* truncation. Two tests written to detect it passed against the bug. `zuno_core::request::elide` shortens the label in Rust instead — a pure function a unit test can check — with `truncate()` left underneath only as a backstop for pathologically wide glyphs. The lesson generalises: **when a gpui behaviour can't be observed in a test, don't build on it.**
`WindowOptions::window_decorations` defaults to `None` | Leaves the window client-decorated with nothing drawn: no buttons, no resize. We set `Client` and draw our own in `chrome.rs`. |
`TextSystem::shape_line` has a `debug_assert!` against newlines | Sanitize at the edit boundary, not just on paste. |
`examples/input.rs` ships macOS `cmd-` bindings | Translate every one to `ctrl-`. It also has a latent `assert_eq!` panic when a placeholder is showing. |
`TabStopNode` orders by tab_index path, **then** paint order | Leaving inputs at the default index 0 makes visual order the tab order for free. |
`Context::on_app_quit` is the correct save hook | Not the Quit *action* — that misses window-manager close. `cx.on_window_closed` is also needed, since GPUI doesn't quit on last-window-close. |
`impl Trait` returns capture **every** in-scope lifetime under edition 2024 | A render helper taking `cx: &mut Context<_>` and returning `impl IntoElement` borrows `cx` for the element's life — fine for one, but collecting several into a `Vec` is several simultaneous mutable borrows. Add `+ use<>` when nothing in the element actually needs the borrow (`cx.listener` returns an *owned* closure). See `settings_panel::setting_row`. |
`Window::dispatch_action` **defers** — it captures the focus id, then `cx.defer`s the dispatch | So an action dispatched from a modal still resolves against the frame the modal was in. It also means closing-then-dispatching and dispatching-then-closing behave identically for actions. Focus order *does* matter for anything that calls `window.focus` synchronously, like `activate`. |
A context-less binding does **not** lose to a specific one — it *ties*, and **later registration wins** | `binding_enabled` returns `depth = contexts.len()` for a `None` context, which is the maximum; the tiebreak is `ix_b.cmp(ix_a)`. So `escape` in `Some("Picker")` only beats the global `escape` -> `CancelRequest` because it is registered after it in `register_keymap`. Reordering that list changes behaviour with no compile error. |
Border **widths** are per-side; `border_color` is **one colour for the whole element** | So `.border_r_1().border_color(a).border_t_2().border_color(b)` paints *both* borders `b`, with no warning — `gpui-macros-0.2.2/src/styles.rs:375` sets a single `style().border_color`. The tab strip did exactly this: the active tab drew its right divider in the accent colour, and every inactive tab drew its divider in `bg_panel`, which is to say invisibly, so the tabs ran together. **Two colours on one box means two elements.** Worth pairing with the entry below — the nested element needs no `stop_propagation` here only because it carries no handler of its own. |
`svg()` needs an `AssetSource` and a `text_color` **on the `svg()` element itself** | Rendered to an **alpha mask**, so colours in the file are ignored and `style.text.color` paints it. **A parent's `text_color` does not reach it:** `Interactivity::compute_style_internal` starts from `Style::default()` and refines only with the element's *own* base style — inherited text style is never merged in. So is `hover`; use `.group()` on the parent and `.group_hover()` on the glyph. Every failure here is silent — a missing asset is swallowed by `log_err()`, an uncoloured icon never reaches `paint_svg`, and the button still hovers, still shows its tooltip, and still dispatches. **This shipped**: the rule was written as a comment on `icon_button` and then applied to the wrapping `div` three lines below it. `ui::glyph` takes the colour as an argument now, so it can't recur. |
`tooltip()` is on `StatefulInteractiveElement` | Needs `.id()` first, same as `overflow_*_scroll`. gpui 0.2.2 ships no tooltip *view* — only the hook, which wants an `AnyView`, so you write the view. |
A `uniform_list` row does **not** fill the list's width unless you say `w_full()` | The list hands each item the full width as *definite available space*, which reads like it should stretch. It doesn't: taffy only auto-stretches a root node to its available width when the node is `display: block` — the `style.is_block()` gate in `taffy-0.9.0/src/compute/mod.rs:68`. Any row calling `.flex()` takes the other branch and sizes to its **content**. The picker's rows were **76px inside a 620px list**: the selection highlight stopped at the end of the label, and the remaining 88% of every row ignored clicks. A row is a hitbox, not just a background, so this is a dead-control bug wearing a styling bug's clothes — `a_picker_row_spans_the_full_width_of_the_list` asserts the click, measured against the *list's* width, because the row's own bounds are the narrow box and agree with the bug. |
Border colours are **not** text colours, and `border == bg_hover` in the dark theme | Three sites reached for `theme.border` to mean "dimmer than muted" — the picker's detail column, the settings hint, a titlebar divider. `border` is picked to sit *just* off its own background, so as text it scored **1.26:1**, and against `bg_hover` it was exactly 1.0: the command palette's keybindings and the settings hints were **invisible on the row under the cursor**, the only row anyone reads. `theme.text_faint` is the token for that job. Three tests in `theme.rs` hold the palette to it — a WCAG contrast floor per text token *per surface* (`bg_hover` included, since a colour can pass at rest and vanish on hover), a spacing check so the three steps stay visibly distinct, and `border_is_too_dim_to_read_as_text`, which asserts the *failure* so that brightening `border` to rescue a text site shows up as a decision rather than a quiet tweak. |
Focusing a handle whose element **isn't painted** kills the whole keymap | A `FocusHandle` belongs to the entity that created it and stays focusable whether or not it is on screen — but **action dispatch walks up the focus tree**, so with no element there is no path to `Workspace` and *every* binding silently stops resolving. `Ctrl+B` did this: it focused the body **editor's** handle for all five body types, and the editor is only rendered for a raw body, so on a form `Ctrl+L` did nothing and typing went nowhere until you clicked. This is the same failure as switching `active_ix` without moving focus, from the other direction — there the handle was dropped, here it was never painted. Assert it through *another binding still working*, never by checking which handle has focus: the buggy code focused exactly what it intended to. |
`cx.stop_propagation()` does **not** stop a scroll container from scrolling | Its wheel handler gates on `hitbox.should_handle_scroll`, which only checks hit-testing (`window.rs:514`) and never consults propagation — so it runs whatever a listener does. To stop a sideways swipe dragging the document vertically, declare **both** axes `overflow_scroll`: `allow_concurrent_scroll` defaults to *false*, and with two non-zero deltas gpui then zeroes the smaller axis itself. The x axis need not actually be scrollable for this — declaring it is what makes the handler look at `delta.x` at all. |
An `overflow: scroll` container is a **hard cap** on a child that must grow | gpui 0.2.2 has no `max-content` width (`Length` is `Definite \| Auto`), so a block-level child takes exactly its container's width and can never exceed it — and a scroll container whose content is pinned to its own size scrolls nothing. The response headers tab shipped "scrollable" and wasn't, for exactly this reason. Make the container a **flex row** and the child is sized by its content instead. Cells telling themselves to shrink (`flex_1`, `min_w(0)`, `truncate`) turned out to be irrelevant either way — break-tested. |
A test that calls `RequestView::load` on a focused view **kills the keymap** | `load` rebuilds the URL input and the body editor, so focus is left on a dropped entity, no path up the focus tree reaches `Workspace`, and every binding silently stops resolving — `ctrl-b` and `ctrl-f` both do nothing and typed characters vanish. Recover by **clicking** (`track_focus` focuses on mouse-down); a keystroke cannot, because the keymap is what died. Not a product bug: `RequestView::new` is `load`'s only real caller and nothing is focused yet there. Loading from a spec is still the right way to fill a big buffer — `simulate_input` of 200 lines takes 13 seconds — just re-focus afterwards. |
A leaf-matching context predicate **ties** with a context-less one, so registration order decides | `depth_of` scans `(0..=contexts.len()).rev()` and returns the first depth whose *last* element matches — which for a leaf match is `contexts.len()`, exactly what a `None` context scores. The sort is `depth_b.cmp(depth_a).then(ix_b.cmp(ix_a))`, so equal depth falls through to **later registration wins**. This is how `ctrl-f` means the body in the editor and the response elsewhere, and it is why every scoped binding goes *after* its global twin in `register_keymap`. Fifth time this ordering has decided behaviour with no compile error to catch it. |
`StyledText`'s highlights must be **sorted and disjoint**, and overlapping them underflows | `compute_runs` walks them doing `range.start - ix`, so a gap paints the wrong text and an overlap panics on a `usize`; `shape_line` wants the same exact tiling. A search match landing inside a coloured token therefore has to *split* that token, not layer on top of it — `ui::split_spans` collects every boundary and asks per segment what applies, which needs no case per overlap. `with_highlights` also layers onto the inherited text style, so runs need no font of their own; `with_runs` does. |
A modal scrim catching clicks does **not** stop the wheel reaching what's behind it | Scroll handlers gate on `hitbox.should_handle_scroll`, which consults the *hit test*, not propagation — so a `stop_propagation` scrim is irrelevant to scrolling. `.occlude()` (`HitboxBehavior::BlockMouse`) is the fix: `hit_test` walks topmost-first and **breaks** at such a hitbox, removing everything below. Every full-window overlay needs it — the picker, the settings panel and the context menu all shipped without. **Not headlessly testable:** nothing behind a modal moves in the test platform either way, so an assertion reads as coverage and isn't. |
A `uniform_list` item is measured **before the list's own text style applies** | `measure_item` runs ahead of `interactivity.prepaint`, so the row is shaped in whatever font is ambient rather than the `font_family`/`text_xs` the list sets — 8.47px per character against the 7.29 it draws at, in one measured case. Put the text style on the **row**, and size the scroll region from an advance measured with that same font. Untestable headlessly: the platform's ambient font happened to measure *wider*, so the region was too big rather than too small and every assertion passed. Only a bounded range (`> 2500 && < 3300`) pins which font decided it. |
A `UniformListDecoration` is translated by the scroll offset, on **both** axes | It is a child of the list, so an overlay drifts left as you scroll right *and* up as you scroll down. Cancel both. Place it by arithmetic off the `bounds` handed to `compute` — `justify_end` ignores a top margin on the child (flex end-alignment pins it regardless), and a `relative` root with an `absolute` child loses the horizontal pin. Each tidier version fixed one axis and broke the other. |
`uniform_list` sizes its horizontal scroll region from **one sampled row** | `with_horizontal_sizing_behavior(Unconstrained)` is only half of it: the content width comes from `measure_item`, which measures the single row named by `with_width_from_item` — **default 0**. Row 0 of a JSON document is `{`, the narrowest row there is, so the switch alone appears not to work. Compute the widest row yourself and pass its *visible* index. gpui also re-clamps `scroll_offset.x` to `[-max, 0]` every `interactivity.prepaint`, so callers writing `set_offset` need no clamp of their own — one was written, and no test could tell it apart. |
Anything reading a scroll handle's `max_offset`/`bounds` at render time is **a frame behind** | Both are written during `interactivity.prepaint`, which runs after the surrounding element tree is built — so a sibling `div` drawing a scrollbar from them renders nothing on the frame the content first appears, then waits for an unrelated repaint. `uniform_list`'s `with_decoration` is the fix and is computed inside that same prepaint, laid out at the list's own bounds, which is exactly an overlay. |
`cx.debug_bounds` reads the **last rendered frame**, so `is_none()` proves nothing | An element that has been removed keeps its entry until another frame is drawn, and `run_until_parked` does not reliably force one. `is_some()` is trustworthy; `is_none()` is not. Four context-menu tests asserted "the menu closed" this way and failed against code that was closing it correctly — the chosen action had demonstrably run. Read real state instead (a `#[cfg(test)]` accessor, as `Workspace::menu_open`), or better, assert the *consequence* — that a following keystroke lands where it should. |
`anchored()` is the primitive for anything placed at the cursor | `.position(p)` with `position_mode(AnchoredPositionMode::Window)` takes window coordinates, which is exactly what `MouseDownEvent::position` already gives you — reading it as `Local` would add the parent's origin twice. The default `SwitchAnchor` fit mode flips the corner near a window edge, so a menu near the bottom-right needs no arithmetic. It must be emitted somewhere unclipped: an `overflow_hidden` ancestor still masks an absolutely-positioned child, so a menu opened from inside a `uniform_list` row must be owned further up (ours lives on `Workspace`, beside the picker). |
`track_focus` focuses on click through an ordinary **Bubble-phase mouse listener** | So `cx.stop_propagation()` on any descendant suppresses *the focus transfer too*, not just the ancestor's own handler. `Interactivity::paint` registers it inline (`div.rs`, near the `tracked_focus_handle` block) rather than special-casing focus anywhere. The response body's fold chevron hit this: it stopped propagation so a chevron click wouldn't also move the row selection, and the cost was that clicking a chevron left the pane **unfocused**, so the next arrow key did nothing. Read together with the row below — the usual advice is "a clickable inside a clickable needs `stop_propagation`", and this is the case where following it breaks something invisible. Assert it the same way as any focus bug: through a *later keystroke* still working, never by asking which handle has focus. |
`Window::bindings_for_action` finds **only globally-bound** actions, whatever is focused | It matches against `rendered_frame.dispatch_tree.context_stack`, and that is a **build-time** stack: `push_node` pushes a context, `pop_node` pops it, so a *finished* frame's stack is empty. An empty stack matches only bindings registered with a `None` context, so every scoped one reads as unbound. Both row menus shipped this way — `Copy value` (`ctrl-c` in `ResponsePane`), `Copy path`, `Fold`, `Rename` (`f2` in `CollectionPanel`) all drew a blank keystroke column, in a primitive whose stated purpose is to teach the shortcut, while a comment above it said it read from the live keymap. **`bindings_for_action_in(action, &focus_handle)` is the one that works** — it rebuilds the stack from a handle. The existing `keybinding_label_matches_the_keymap` could never have caught it: every action in `advertised_actions` is bound globally, so it pins the *formatter* and says nothing about the lookup. |
Focusing an unpainted handle does **not** kill globally-bound actions — assert on **typing** | The two rows above are right that focus must move when an element stops being painted, and slightly too strong about the symptom. `window.rs` resolves the focused id to a dispatch node and falls back to `dispatch_tree.root_node_id()` when the frame has none — and every `Workspace` handler is reachable from the root, since `Workspace` *is* the window's root view. So a `None`-context binding keeps working, measured: a test asserting `ctrl-shift-h` still added a header **passed against the bug** and read exactly like coverage. What actually breaks is anything needing a live element — no `TextInput` holds focus, so typing vanishes, and no `key_context` node exists, so every scoped binding stops matching. `hiding_the_panel_leaves_typing_somewhere_to_land` asserts the typing, which is the half a user would notice. |
`on_mouse_down` is a **Bubble**-phase listener, so an **ancestor's** handler runs too | Overlapping *siblings* are resolved by hit-testing — the one painted later occludes the rest, which is why `chrome.rs` emits the resize corners last. **Ancestor/descendant is not**: a click inside a child is inside the parent's hitbox as well, so both fire, child first. A clickable nested in a clickable therefore needs `cx.stop_propagation()`. The window controls sit inside the drag-to-move titlebar and went without it for a while — so closing the window also asked the compositor to start dragging it. `platform/test/window.rs`'s `start_window_move` is `unimplemented!()`, which is what makes this testable at all. |

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
- **A test server must send `Connection: close`, and must never wait forever.** Left keep-alive,
  reqwest pools the socket, so a server that accepts once per response can block on a connection
  the client decided to reuse instead. That hung a CI run for six hours before GitHub killed it,
  and never reproduced locally — whether the client sees the server's FIN before sending again is a
  race that only loses on a slow, loaded runner. `accept_before` bounds accept *and* read, so a
  wrong assumption fails in seconds with a message. Both CI workflows now carry
  `timeout-minutes` for the same reason.

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
- **A weak assertion reads exactly like a strong one.** Five times now a test has passed against
  both the correct code *and* the broken version, because it asserted something true in both:
  `row_count() > 0` after switching runs, "no environment appears in the request picker" (true
  whether or not `scan` skips the directory), containment that two independent layers guaranteed,
  an accessor that reported "nothing held" for the very state it was meant to detect, and
  `assert!(spec.settings.follow_redirects)` after importing `curl -L` — true by default, so it held
  with the `-L` arm deleted from the parser entirely. The fix is always the same — break it
  deliberately and watch the test fail. If it doesn't, the test is decoration.
  **The fifth one is worth a second look**, because the weak assertion was hiding a real bug rather
  than merely failing to catch one: the default that made it vacuous was itself the defect, so
  fixing the *code* turned the existing test load-bearing without touching it.
- **Docs went stale twice while the code was right.** Both times a multi-file edit script aborted
  on a failed anchor assertion, so files listed *after* the failure were silently skipped, and the
  summary claimed work that hadn't happened. `git status` showed the untouched files both times.
  Hence the checklist below.
- **And then the code went stale while the comments were right — five times, found in one audit.**
  The opposite failure, and the harder one: `chrome.rs` explained why the window controls call
  `stop_propagation` and never called it, so closing the window also asked the compositor to drag
  it; the body chip's comment said `Ctrl+Shift+B` "does the same" while the chip still cycled
  `RawKind`; `SizeInfo` documented a compression ratio that reqwest makes unknowable; the in-flight
  pane advertised `Ctrl+C` because an M1 design sketch had said to wire it; `settings_panel` cited a
  `tab_stop(false)` call that was never there — and *that* default is why Tab used to escape the
  modal.

- **And a fourth direction: a comment stating the correct rule, applied to the wrong line.** The
  icon buttons carried a comment explaining that gpui paints an SVG with `style.text.color` and
  draws nothing without one — and then set the colour on the wrapping `div` instead of on the
  `svg()`, which does not inherit it. Every icon in the app was invisible. Tooltips worked, hover
  worked, clicks worked; only the pixels were missing, so no test noticed and the reviewer (me)
  read the comment as evidence the code was right.

  Two things came out of it. `ui::glyph` now takes the colour as a parameter, so the rule is a
  signature rather than a sentence. And the untestable half got separated from the testable half:
  nothing in the headless platform can observe a paint, but `resvg` — pinned to *gpui's* version —
  can prove each icon rasterizes to visible pixels, which catches the malformed-file variant of the
  same silent failure. **When a comment states a rule, check the rule is applied to the thing the
  comment is about**, not merely present nearby.

- **A "bug" reported from the harness that the real app did not have.** A probe showed `Ctrl+B`
  failing to reach the body editor while the response find bar was open, and it was written up as
  a pre-existing product bug. It works in the app; the divergence was the harness's. **The
  headless platform is authoritative about state and logic, not about focus and paint** — it has
  already been wrong once about which element owns the keyboard. Before calling a focus or
  rendering observation a product bug, say it is a harness observation and ask, or find the
  mechanism. Reporting one costs the reader a hunt for something that isn't there, which is the
  same expense as a doc asserting a defect the code never had.

- **A horizontal-scrolling slice was reported working and was broken on every surface.** Worth
  keeping because the tests were green the whole time and asserted the wrong things. The response
  headers had *no test at all* while the summary said "shipped". The others asserted
  `max_offset > 0` and "the offset changed" — both true for a region sized to the wrong row, a
  scrollbar drawn along the top edge, a thumb sliding the wrong way, and an editor that snapped
  home on the next frame. **"Something overflows" is not "you can reach the end", and "the value
  moved" is not "it moved where you can see it."** Every assertion in that slice passed against
  the bug it was written for. The replacements assert positions against the container, widths
  against the content they must reveal, and offsets *after* a repaint.

- **And once in the third direction: a doc asserted a bug the code never had.** architecture.md §13
  listed the editor's per-line horizontal scroll clamp as a "known defect … it's just wrong", while
  §7 and two comments in `editor.rs` described the same behaviour as a deliberate choice with a
  named rejected alternative. Reading the clamp settles it for §7 — landing on a short line *must*
  return the view to x=0, or the cursor goes off-screen left. Retracted, with the reasoning, in §13.

  This direction is the most expensive of the three. Code drifting from a correct comment misleads
  a reader; a phantom defect sends them hunting for something that isn't there and reads as licence
  to "fix" working code. **Heuristic when two sections disagree: trust the one that names a rejected
  alternative**, because it was written while looking at the problem.

  The pattern is worth naming, because good docs cause it. A confident comment gets trusted and
  stops being checked, so the code drifts underneath it and every reader inherits the claim. All
  five were found by reading the vendored `gpui`/`reqwest`/`tower-http` source instead of the
  sentence describing it. **"Verify, don't remember" applies to our own comments, not just to
  recalled GPUI APIs** — and when a comment explains why a call is load-bearing, that is precisely
  the moment to check the call is there.

## Design tweaks — the fast path

**Read this before "Finishing a slice", because that checklist does not apply here.** Tweaking
design is how the app gets its taste, it is continuous, and it is *supposed* to be cheap. Spending
fifteen minutes on a colour makes the loop so expensive it stops happening, which costs more than
any bug the ceremony would have caught.

A **visual-only** change — colour, spacing, size, alignment, hover, icon, a chip's label:

1. Make it.
2. `cargo check --workspace --all-targets` (~0.5s). **Not the suite.**
3. **Hand it to the human to look at**, naming what changed. That glance is the verification —
   nothing in the headless platform can observe a paint, so a person's eye is the only instrument
   that reads the pixels.

   **An agent must not try to do this itself.** No launching the app, no screenshot tooling, no
   hunting for one. It does not work — GPUI runs natively on Wayland, so X11-era grabbers see
   nothing and other routes have hit permission errors — and a launched window takes over the
   user's desktop. This step ends with you saying which visual states are new, not with an image.

No test, no doc edit, no break-it-on-purpose. Three triggers, and only these, promote a tweak to
something heavier:

| Trigger | What it costs |
|---|---|
The fix repairs something **invisible** — a dead hitbox, a silent no-op, a control that dispatches nothing | One test, asserted at the *consequence*. This is the `w_full` case: the short highlight was obvious, the 88% of each row that swallowed clicks was not. |
It adds a **shared primitive** others will reuse — a theme token, a `ui::` helper | One line where the primitive is defined. Not three documents. |
It **contradicts a comment or doc** that's already there | Fix that claim. A stale confident note is the failure mode this file's Lessons section is mostly about. |

**Where this rule came from.** The picker's hover width and its unreadable detail column were a
four-line fix that took fifteen minutes, because "Finishing a slice" got run on a paint change:
a hand-rolled WCAG implementation and three tests for a hex value, break-it-on-purpose three times,
and edits to all three docs for a hover colour. The diagnosis was four minutes; the ritual was
eleven. **The checklist below is priced in behaviour bugs** — session formats discarding requests,
`preserved_body` overwriting real bodies, an icon set that rendered nothing — and a hover colour is
not in that class. Applying its weight to paint is cargo cult, and the reason it happened is that
this file said to.

The one genuine trap in this territory is the opposite of over-testing: a visual bug with an
invisible functional half. Both bugs above had one. So the question to ask on a design fix is not
"does this need a test" but **"is there a part of this I could not have seen?"** — if no, ship it.

## Finishing a slice

**Scope: behaviour changes.** For a visual tweak see the fast path above; running this list on a
colour is how a four-line fix takes fifteen minutes.

Not ceremony — each line here is something that has actually been missed, and the last two are why
this list exists at all.

1. `RUSTFLAGS="-D warnings" cargo test --workspace` — the count in **Commands** above is the current
   total; update it.
2. **Break it on purpose.** Revert the core behaviour by hand and confirm the intended test fails.
   A test that passes both ways is decoration, and this has caught four of them.
3. **Update the docs that the change invalidated**, then **grep for the new text to prove the edit
   landed.** Prefer one edit per file over a batched script: a script that asserts its anchors
   aborts on the first miss and silently skips everything after it, which is how §11 spent a slice
   claiming multipart was unimplemented.
4. `git status` before writing any summary. A doc you meant to change and didn't will be sitting
   there unmodified.

Which doc owns what: **ROADMAP** order and what's next; **architecture.md** design decisions and
what was tried and rejected; **CLAUDE.md** commands, invariants, traps.

## Conventions

- Comments explain **why**, not what. If a decision has a rejected alternative, name it.
- Derive state rather than mirroring it. `RequestView` has no stored `RequestSpec` —
  `spec(cx)` assembles one from the inputs, so what's sent can't disagree with what's on screen.
  **The corollary bites:** anything the inputs can't represent is *destroyed* by the derivation.
  That's how form, multipart, and binary bodies were once silently emptied on load and then written
  over on save. The durable fix was giving every `Body` variant an editor, so `RequestView::load`
  matches **exhaustively with no catch-all** — adding a variant is now a compile error until someone
  decides how to edit it. An interim `preserved_body` field held the unknown instead; it's gone,
  because a catch-all that quietly preserves is weaker than a match that refuses to build.
- **Every verb needs a mouse path, not just a keybinding.** Keyboard-first is not keyboard-only: an
  audit found only six of ~40 actions were reachable by mouse, and nine had none at all — find,
  copy-as-curl, copy response, settings, import, new tab. A shortcut nobody can discover is a
  feature nobody has. `ui::icon_button` / `ui::text_action` are the way to add one, and their
  tooltip reads the keystroke from the live keymap so the mouse path *teaches* the keyboard one.
  `affordances()` in the tests is the table that keeps it honest.
- Actions, not direct calls, for anything a button and a keybinding share. A command palette row
  dispatches the action too, so palette and keystroke can't drift.
- **`TextInput` emits `Changed`; subscribe to it rather than polling in `render`.** The picker
  used to notice typing by storing the query it last ranked and comparing it every frame,
  because the input emitted nothing. It does now — from the two methods that mutate content,
  which between them are every edit path — so a mirror field is no longer the answer. Hold the
  `Subscription`: dropping one unsubscribes, silently.
- **Every new action needs a `commands::palette()` row or an `EXCLUDED` entry with a reason.** The
  drift test fails otherwise, on purpose: an action reachable only by a keystroke nobody remembers
  is the thing a palette exists to prevent.
- Every action handler lives on `Workspace`, because dispatch travels up the focus tree and
  `Workspace` is always on it.
- **Anything that opens a modal, or moves focus, asks `Workspace::modal_open` first.** Written out
  at each site it drifts: `open_request` and `open_palette` checked only `picker` while four other
  openers checked both, so `Ctrl+P` over the settings panel stacked two modals. And because the
  panes behind a modal are still painted, their `TextInput`s are still tab stops — so `Tab` walked
  focus past the scrim, the modal's leaf key context stopped matching, and every binding it owned
  including `Escape` silently died. Both are the same bug: focus left the modal while the modal
  stayed on screen.
- **Anything that changes which buffer is active goes through `Workspace::activate`.** A
  `FocusHandle` belongs to the entity that created it, so switching `active_ix` without moving
  focus leaves it inside the old view — and after a close, inside a dropped one, where no key
  context matches and the whole keymap goes dead with nothing on screen explaining why. Removing
  the focus move from `activate` fails five tests; that's deliberate.
- Errors are typed and renderable. `EngineError::is_local()` separates "nothing left the
  machine" from a network failure, which is a real distinction to a person debugging.
