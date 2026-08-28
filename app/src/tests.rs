//! End-to-end tests driven through GPUI's headless test platform.
//!
//! These exist because the M1.1 acceptance criterion — "type a real request and the
//! spec that comes back is correct" — cannot be checked by a compile or by staring at
//! a window. `simulate_input` and `simulate_keystrokes` push through the same paths a
//! real keyboard does: platform input handler → `EntityInputHandler` for text, and the
//! keymap → context predicates → action dispatch for commands.
//!
//! That makes them a genuine regression net for the two things most likely to break
//! silently: key context scoping and the derived-spec wiring.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{TestAppContext, VisualTestContext};
use zuno_core::{Body, EngineError, Method, MultipartValue, RawKind, RequestSpec, ResponseData};

use crate::body_view::BodyView;
use crate::request_view::{RequestTab, RequestView, ResponseView};
use crate::theme::{Appearance, Theme};
use crate::workspace::Workspace;

/// Boot a window the same way `main` does, so the keymap, theme, and engine under test
/// are the real ones rather than a test-only arrangement.
fn open_workspace(cx: &mut TestAppContext) -> (gpui::Entity<RequestView>, VisualTestContext) {
    // Persistence must not touch the developer's real session file — the tests drive
    // SendRequest, and a send is a save point.
    let (_, view, vcx) = boot(cx, None, None);
    (view, vcx)
}

/// `open_workspace`, but keeping the window handle and taking the two persistence paths,
/// for the tests that care about restore or saving. Most don't, which is why the common
/// case hides all three.
///
/// `None` for either disables that half. Both default to `None` in `open_workspace`
/// because invariant 6 applies to collections exactly as it does to the session file: the
/// suite must never write into the developer's own data.
fn boot(
    cx: &mut TestAppContext,
    session: Option<PathBuf>,
    collections: Option<PathBuf>,
) -> (
    gpui::WindowHandle<Workspace>,
    gpui::Entity<RequestView>,
    VisualTestContext,
) {
    cx.update(|cx| {
        cx.set_global(Theme::new(Appearance::Dark, "monospace".into()));
        crate::register_keymap(cx);
        crate::engine::install(cx).expect("engine");
        crate::session::install_at(cx, session);
        crate::collections::install_at(cx, collections);
    });

    let window = cx.add_window(|window, cx| Workspace::new(window, cx));
    let view = window
        .update(cx, |workspace, _, _| workspace.active().unwrap())
        .unwrap();
    let vcx = VisualTestContext::from_window(window.into(), cx);

    (window, view, vcx)
}

/// A scratch directory that is never `~/.config/zuno` (CLAUDE.md invariant 6). Named per
/// test and per process so a parallel run can't collide.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zuno-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Tear down a test's scratch directory.
///
/// **Persistence has to be switched off before deleting**, or the delete doesn't stick:
/// dropping the app at the end of a test fires `on_app_quit`, which saves the session and
/// recreates the directory that was just removed. Every scratch dir in `/tmp` came back with
/// a `session.json` in it until this pointed the globals at `None` first.
///
/// That the hook fires on teardown at all is a small free proof it's wired to every exit
/// path, which is what it was written for.
///
/// Takes the whole directory rather than the one file, so a failing test doesn't leave an
/// empty one behind for every run.
fn remove_scratch(cx: &mut VisualTestContext, session: &PathBuf) {
    cx.update(|_, cx| {
        crate::session::install_at(cx, None);
        crate::collections::install_at(cx, None);
    });

    if let Some(dir) = session.parent() {
        std::fs::remove_dir_all(dir).ok();
    }
}

/// The buffer currently in front.
///
/// Needed whenever an action *opens* a buffer — `ctrl-t`, curl import, and opening from the
/// picker all do — so the handle returned by `boot` is the *previous* buffer, and reading it
/// silently asserts against the wrong request. That has caught four tests now.
fn active_view(
    window: &gpui::WindowHandle<Workspace>,
    cx: &mut VisualTestContext,
) -> gpui::Entity<RequestView> {
    window
        .update(cx, |workspace, _, _| {
            workspace.active().expect("an active buffer")
        })
        .expect("window")
}

fn spec_of(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext) -> RequestSpec {
    cx.update(|_, cx| view.read(cx).spec(cx))
}

/// The engine runs on its own OS thread, so its events arrive asynchronously from
/// gpui's point of view. `run_until_parked` alone returns while the consuming task is
/// still awaiting the channel, so poll it against a deadline.
fn wait_for<T>(
    cx: &mut VisualTestContext,
    what: &str,
    mut probe: impl FnMut(&mut VisualTestContext) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if let Some(value) = probe(cx) {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

const OK_JSON: &str = "HTTP/1.1 200 OK\r\n\
     Content-Type: application/json\r\n\
     Content-Length: 11\r\n\
     \r\n\
     {\"ok\":true}";

/// A one-shot HTTP server on an ephemeral port. Returns its base URL.
fn serve_once(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

/// Accept one connection, capture the raw request text, reply with `response`.
///
/// The app-level `serve_once` discards what it received; asserting on the bytes a server
/// actually got is the only way to check an encoding rather than the intent behind it.
fn serve_capturing(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return String::new();
        };
        let mut buffer = vec![0u8; 8192];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        String::from_utf8_lossy(&buffer[..read]).to_string()
    });

    (format!("http://{addr}"), handle)
}

/// Accept a connection and never reply, so a request stays in flight.
fn serve_never() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(Duration::from_secs(30));
            drop(stream);
        }
    });

    format!("http://{addr}")
}

/// A port that was bound and released, so connecting to it is refused.
fn closed_port() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    format!("http://{addr}")
}

fn type_url(cx: &mut VisualTestContext, url: &str) {
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input(url);
}

/// Focus the body editor and empty it. The sample request ships a body and the editor
/// opens with the cursor at offset 0, so typing without this prepends.
fn clear_body(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("ctrl-b ctrl-a backspace");
}

#[gpui::test]
async fn url_bar_starts_focused_and_accepts_typing(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    // The sample request seeds a URL; select-all then type replaces it, which is the
    // real editing path (SelectAll action, then a text replacement over a selection).
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("https://api.zuno.dev/v1/users");

    let spec = spec_of(&view, &mut cx);
    assert_eq!(spec.url, "https://api.zuno.dev/v1/users");
}

#[gpui::test]
async fn typed_text_reaches_the_derived_spec(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("https://example.test/api");

    // Adding a header focuses its name cell, so typing lands there without a click.
    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("X-Trace-Id");

    // Tab must reach the value cell. This is the assertion that would have caught
    // TextInput's focus handles missing `tab_stop(true)`.
    cx.simulate_keystrokes("tab");
    cx.simulate_input("abc-123");

    let spec = spec_of(&view, &mut cx);
    assert_eq!(spec.url, "https://example.test/api");

    let added = spec.headers.last().expect("a header row was added");
    assert_eq!(added.name, "X-Trace-Id");
    assert_eq!(added.value, "abc-123");
    assert!(added.enabled);
}

#[gpui::test]
async fn ctrl_m_opens_the_method_picker_with_the_current_one_marked(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-m");
    assert!(picker_is_open(&window, &mut cx));

    let rows = picker_rows(&window, &mut cx);
    assert_eq!(rows.len(), 7, "the seven common verbs: {rows:?}");
    // The sample request is a POST, and the list should say so rather than only offering
    // choices.
    assert!(
        rows.iter().any(|row| row.starts_with("POST") && row.contains("current")),
        "the active method should be marked: {rows:?}"
    );
}

#[gpui::test]
async fn choosing_a_method_sets_it_on_the_request(cx: &mut TestAppContext) {
    let (window, view, mut cx) = boot(cx, None, None);
    assert_eq!(spec_of(&view, &mut cx).method, Method::Post);

    cx.simulate_keystrokes("ctrl-m");
    cx.simulate_input("del");
    cx.simulate_keystrokes("enter");

    assert!(!picker_is_open(&window, &mut cx));
    assert_eq!(spec_of(&view, &mut cx).method, Method::Delete);
}

#[gpui::test]
async fn typing_an_unknown_verb_offers_it_as_a_custom_method(cx: &mut TestAppContext) {
    // Closes the last of §11's non-body gaps. `Method::Other` was always sendable — core
    // has tests for it — but nothing in the UI could produce one.
    let (window, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-m");
    cx.simulate_input("purge");

    let rows = picker_rows(&window, &mut cx);
    let derived = rows.last().expect("a derived row");
    assert!(derived.contains("PURGE"), "should offer the typed verb: {rows:?}");
    assert!(derived.contains("custom method"), "{derived:?}");

    // The derived row is last, so this walks to it rather than assuming it's selected.
    cx.simulate_keystrokes("up enter");
    assert_eq!(
        spec_of(&view, &mut cx).method,
        Method::Other("PURGE".to_string()),
        "and uppercase it, since nobody typing `purge` means a lowercase verb"
    );
    let _ = window;
}

#[gpui::test]
async fn a_known_verb_typed_in_full_is_not_offered_twice(cx: &mut TestAppContext) {
    // A second row would set `Other("GET")` rather than `Get` — the same request, but a
    // different value everything downstream compares.
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-m");
    cx.simulate_input("get");

    let rows = picker_rows(&window, &mut cx);
    assert_eq!(rows.len(), 1, "exactly the real GET row: {rows:?}");
    assert!(rows[0].starts_with("GET"), "{rows:?}");
}

/// Put the active buffer's body into a raw text editor with `text` in it.
fn author_body(cx: &mut VisualTestContext, text: &str) {
    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("text");
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("ctrl-b ctrl-a");
    cx.simulate_input(text);
}

fn body_text(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext) -> String {
    match &spec_of(view, cx).body {
        Body::Raw { text, .. } => text.clone(),
        other => panic!("expected a raw body, got {other:?}"),
    }
}

#[gpui::test]
async fn undo_collapses_a_typed_run_and_redo_replays_it(cx: &mut TestAppContext) {
    // Undo was absent entirely. `input::history_tests` pins the coalescing rule; this pins that
    // the keystroke reaches a real surface and that the whole edit path routes through the
    // history — every insertion funnels through `replace_text_in_range`, so a missed call site
    // there is the way this silently only half-works.
    let (_, view, mut cx) = boot(cx, None, None);

    let original = spec_of(&view, &mut cx).url.clone();
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://example.test/a");
    assert_eq!(spec_of(&view, &mut cx).url, "https://example.test/a");

    // One press, and the whole thing goes — including the character that replaced the
    // selection, which is one edit to a person even though it is a replace then 21 inserts.
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(
        spec_of(&view, &mut cx).url, original,
        "select-all then type collapses into a single entry"
    );

    cx.simulate_keystrokes("ctrl-y");
    assert_eq!(spec_of(&view, &mut cx).url, "https://example.test/a", "redo replays it");

    // Ctrl+Shift+Z is the other redo spelling, and undo past the start must be a no-op rather
    // than a panic.
    cx.simulate_keystrokes("ctrl-z ctrl-shift-z");
    assert_eq!(spec_of(&view, &mut cx).url, "https://example.test/a");
    for _ in 0..10 {
        cx.simulate_keystrokes("ctrl-z");
    }
    cx.simulate_input("ok");
    assert_eq!(spec_of(&view, &mut cx).url, "ok", "undo bottoms out without breaking the input");
}

#[gpui::test]
async fn moving_the_caret_starts_a_new_undo_entry(cx: &mut TestAppContext) {
    // The run has to close when the caret moves: typing, arrowing away, then typing again is two
    // edits to a person, and one entry would undo both at once.
    //
    // **This test exists because deleting `history.break_run()` from `move_to` broke nothing.**
    // The unit test for the rule calls `break_run` directly, so it covered the History type and
    // not the call site — the exact shape of gap this repo keeps getting caught by.
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("abc");

    // Away and **back**, which is the case the contiguity check alone cannot catch: the caret
    // ends up exactly where the run left it, so without `break_run` the next character would
    // silently rejoin the previous entry.
    cx.simulate_keystrokes("left right");
    cx.simulate_input("d");
    assert_eq!(spec_of(&view, &mut cx).url, "abcd");

    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(
        spec_of(&view, &mut cx).url, "abc",
        "the caret having moved makes `d` its own entry, so undo leaves `abc` standing"
    );
}

#[gpui::test]
async fn each_text_surface_has_its_own_undo_history(cx: &mut TestAppContext) {
    // One history per entity. Sharing one would make Ctrl+Z in the URL bar reach into the body,
    // which destroys work silently — the worst kind of bug for an undo stack to have.
    let (_, view, mut cx) = boot(cx, None, None);

    // Switch to a text body, then note what the editor holds before we overwrite it.
    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("text");
    cx.simulate_keystrokes("enter");
    let body_before = body_text(&view, &mut cx);

    cx.simulate_keystrokes("ctrl-b ctrl-a");
    cx.simulate_input("body text");
    assert_eq!(body_text(&view, &mut cx), "body text");

    let url_before = spec_of(&view, &mut cx).url.clone();
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://example.test/x");

    // Undo with focus in the URL bar rolls back the URL and leaves the body alone.
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(spec_of(&view, &mut cx).url, url_before, "the URL bar's own edit undoes");
    assert_eq!(body_text(&view, &mut cx), "body text", "the body is untouched");

    // And the reverse: undo in the body does not disturb the URL.
    cx.simulate_keystrokes("ctrl-b ctrl-z");
    assert_eq!(body_text(&view, &mut cx), body_before, "the body's own run undoes");
    assert_eq!(spec_of(&view, &mut cx).url, url_before, "the URL bar stays where it was");
}

#[gpui::test]
async fn ctrl_backspace_and_ctrl_delete_remove_a_word(cx: &mut TestAppContext) {
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.example.com/posts");
    cx.simulate_keystrokes("ctrl-backspace");
    assert_eq!(
        spec_of(&view, &mut cx).url, "https://api.example.com/",
        "ctrl-backspace takes the word behind the caret, not the whole line"
    );

    // Forward deletion from the start eats `https`, leaving its punctuation.
    cx.simulate_keystrokes("home ctrl-delete");
    assert_eq!(spec_of(&view, &mut cx).url, "://api.example.com/");
}

#[gpui::test]
async fn ctrl_home_and_end_span_the_document_in_the_editor(cx: &mut TestAppContext) {
    // Home and End are deliberately per-line in the editor, so before this there was no way to
    // reach the document's ends by keyboard at all.
    let (_, view, mut cx) = boot(cx, None, None);
    author_body(&mut cx, "alpha\nbeta\ngamma");

    // The caret is on the last line; plain Home only reaches that line's start.
    cx.simulate_keystrokes("home");
    cx.simulate_input(">");
    assert_eq!(body_text(&view, &mut cx), "alpha\nbeta\n>gamma");

    cx.simulate_keystrokes("ctrl-home");
    cx.simulate_input("^");
    assert_eq!(body_text(&view, &mut cx), "^alpha\nbeta\n>gamma", "ctrl-home reaches line one");

    cx.simulate_keystrokes("ctrl-end");
    cx.simulate_input("$");
    assert_eq!(body_text(&view, &mut cx), "^alpha\nbeta\n>gamma$", "ctrl-end reaches the last line");
}

#[gpui::test]
async fn ctrl_arrow_moves_by_word_in_the_url_bar_and_the_body_editor(cx: &mut TestAppContext) {
    // Word movement was simply absent from the hand-rolled input — `Ctrl+Left`/`Right` did
    // nothing anywhere in the app. `input::word_tests` pins what a word *is*; this pins that the
    // keystroke reaches both text surfaces, which is the part a unit test cannot see: the
    // bindings are scoped to `Some("TextInput")` and the body editor only receives them because
    // its leaf context string carries that identifier too. A wrong context compiles fine and
    // does nothing.
    //
    // Asserted by typing at the caret rather than by reading the selection, since the offset is
    // private — where the character lands *is* the caret position.
    let (_, view, mut cx) = boot(cx, None, None);

    // ---- the URL bar -------------------------------------------------------
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.example.com/posts");

    cx.simulate_keystrokes("ctrl-left");
    cx.simulate_input("X");
    assert_eq!(
        spec_of(&view, &mut cx).url,
        "https://api.example.com/Xposts",
        "ctrl-left must land at the start of the last word, not at the start of the line"
    );

    // Two more hops: back over `X`, then over the `/` — punctuation is its own run, so the
    // caret stops between `com` and `/` rather than skipping to the start of `com`.
    cx.simulate_keystrokes("ctrl-left ctrl-left");
    cx.simulate_input("Y");
    assert_eq!(spec_of(&view, &mut cx).url, "https://api.example.comY/Xposts");

    // Selection, and the pair that makes it useful: select a word and replace it.
    cx.simulate_keystrokes("end");
    cx.simulate_keystrokes("ctrl-shift-left");
    cx.simulate_input("Z");
    assert_eq!(
        spec_of(&view, &mut cx).url,
        "https://api.example.comY/Z",
        "ctrl-shift-left selects the trailing word so typing replaces it"
    );

    // ---- the body editor, a different entity with its own handlers ---------
    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("text");
    cx.simulate_keystrokes("enter");

    cx.simulate_keystrokes("ctrl-b ctrl-a");
    cx.simulate_input("alpha beta gamma");

    cx.simulate_keystrokes("ctrl-left");
    cx.simulate_input("Q");
    let Body::Raw { text, .. } = &spec_of(&view, &mut cx).body else {
        panic!("expected a raw body")
    };
    assert_eq!(
        text, "alpha beta Qgamma",
        "the editor gets word movement through the same action, not a second implementation"
    );

    cx.simulate_keystrokes("ctrl-shift-right");
    cx.simulate_input("!");
    let Body::Raw { text, .. } = &spec_of(&view, &mut cx).body else {
        panic!("expected a raw body")
    };
    assert_eq!(text, "alpha beta Q!", "ctrl-shift-right selects to the word end in the editor");
}

#[gpui::test]
async fn focus_body_never_lands_on_an_unpainted_handle(cx: &mut TestAppContext) {
    // `Ctrl+B` used to focus `body_focus` — the *editor's* handle — for every body type, and the
    // editor is only painted for a raw body. A handle belongs to the entity that made it whether
    // or not it is on screen, so on a form this focused an element that did not exist. Action
    // dispatch walks up the focus tree, so that severed the path to `Workspace` and **every**
    // binding stopped resolving.
    //
    // Asserted through `Ctrl+L` rather than by inspecting focus, because "the keymap still works"
    // is the thing that broke. Checking which handle is focused would pass against the bug: the
    // old code *did* focus the editor, that was precisely the problem.
    let (_, view, mut cx) = boot(cx, None, None);

    let url_reachable = |view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext| {
        cx.simulate_keystrokes("ctrl-l");
        cx.update(|window, cx| view.read(cx).url_focus(cx).is_focused(window))
    };

    // Raw: the editor is painted, so this always worked.
    cx.simulate_keystrokes("ctrl-b");
    assert!(url_reachable(&view, &mut cx), "raw body: ctrl-l must still reach the URL bar");

    // Form: the case that killed the keyboard.
    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_keystrokes("ctrl-b");
    assert!(
        url_reachable(&view, &mut cx),
        "form body: ctrl-b must not strand focus on the unpainted editor"
    );

    // Multipart, which has the same shape.
    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_keystrokes("ctrl-b");
    assert!(url_reachable(&view, &mut cx), "multipart body: same");

    // And typing after ctrl-b has to land in the body, not vanish.
    cx.simulate_keystrokes("ctrl-b");
    cx.simulate_input("part-name");
    let spec = spec_of(&view, &mut cx);
    let Body::Multipart(parts) = &spec.body else {
        panic!("expected a multipart body, got {:?}", spec.body)
    };
    assert!(
        parts.iter().any(|p| p.name == "part-name"),
        "typing after ctrl-b must reach the focused part: {parts:?}"
    );
}

#[gpui::test]
async fn the_body_region_reports_focus_for_every_body_type(cx: &mut TestAppContext) {
    // The focus ring around the body used to ask `body_focus`, which is the *editor's* handle —
    // and the editor is only painted for a raw body. So on a form it stayed grey while you were
    // plainly typing into a field, because focus was on that row's own `TextInput` instead.
    //
    // **What this proves, precisely.** It pins the predicate: reverting the Form arm to ask the
    // editor's handle fails the assertion below. It does *not* prove `request_pane::render` keys
    // the border off it — reverting that call site leaves this test green, because the test calls
    // the method directly and nothing in the headless platform can observe a paint. Same
    // admission as `clicking_the_headers_tab_switches_the_response_view`: the wiring is held by
    // review, and saying so is the point, since a test that looks like it covers the border
    // would stop anyone checking.
    let (_, view, mut cx) = boot(cx, None, None);

    let focused = |view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext| {
        cx.update(|window, cx| view.read(cx).body_region_focused(window, cx))
    };

    // Raw: focus the editor and the region reports it.
    cx.simulate_keystrokes("ctrl-b");
    assert!(focused(&view, &mut cx), "a raw body is focused through the editor");

    // Form: Ctrl+Shift+F switches the body to a form and focuses the new field's name cell.
    // That cell belongs to the row, not to the editor.
    cx.simulate_keystrokes("ctrl-shift-f");
    assert!(
        matches!(spec_of(&view, &mut cx).body, Body::Form(_)),
        "ctrl-shift-f switches the body to a form"
    );
    assert!(
        focused(&view, &mut cx),
        "a form field holds focus, so the body region has it — this is the case that was broken"
    );

    // Moving focus out of the body clears it, or the ring would never turn off.
    cx.simulate_keystrokes("ctrl-l");
    assert!(!focused(&view, &mut cx), "focus in the URL bar is not focus in the body");
}

#[gpui::test]
async fn a_picker_row_spans_the_full_width_of_the_list(cx: &mut TestAppContext) {
    // A layout bug with a functional half, which is the only half a headless platform can see.
    //
    // `uniform_list` lays each item out as a taffy **root**, handing it the list's width as
    // definite available space — but taffy stretches a root to fill that space only when the
    // node is `display: block`, a gate inside `compute_root_layout` (taffy 0.9's
    // `style.is_block()`). Every picker row calls `.flex()`, so it took the other branch and
    // sized to its own content. Visibly: the selection highlight stopped at the end of the
    // label. Functionally: most of a 620px-wide list was not clickable.
    //
    // **Asserted against the *list's* width, never the row's own.** Before the fix the row's
    // bounds *are* the narrow box, so a click at `row.right()` lands inside it either way and
    // the test would pass against the bug — the exact shape of weak assertion this codebase
    // has been caught by five times. The container is the only honest reference.
    let (window, view, mut cx) = boot(cx, None, None);
    assert_eq!(spec_of(&view, &mut cx).method, Method::Post);

    cx.simulate_keystrokes("ctrl-m");
    cx.simulate_input("del");
    cx.run_until_parked();

    let list = cx.debug_bounds("picker").expect("the picker should be painted");
    let row = cx.debug_bounds("picker-row-0").expect("the DELETE row should be painted");
    // Minus the container's 1px border on each side.
    assert!(
        row.size.width >= list.size.width - gpui::px(4.),
        "a row must span the list, not its label: row {:?} inside list {:?}",
        row.size.width,
        list.size.width
    );

    // And the consequence, at a point that was dead space: far right of the list, vertically
    // on the row. `on_mouse_down` is Bubble-phase, so the row's handler runs before the
    // container's `stop_propagation` — a hit here really does choose the row.
    cx.simulate_click(
        gpui::point(list.right() - gpui::px(8.), row.center().y),
        gpui::Modifiers::default(),
    );

    assert!(!picker_is_open(&window, &mut cx), "choosing a row closes the picker");
    assert_eq!(
        spec_of(&view, &mut cx).method,
        Method::Delete,
        "clicking the empty right-hand side of a row must choose that row"
    );
}

#[gpui::test]
async fn a_verb_that_could_not_be_sent_is_not_offered(cx: &mut TestAppContext) {
    // The engine rejects anything outside RFC 9110's `tchar` with `InvalidMethod`. Offering
    // a row that fails at send is worse than not offering it.
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-m");
    cx.simulate_input("foo bar");

    let rows = picker_rows(&window, &mut cx);
    assert!(rows.is_empty(), "should offer nothing at all: {rows:?}");

    // A slash would land in the request line too.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("GET/../x");
    assert!(picker_rows(&window, &mut cx).is_empty());
}

#[gpui::test]
async fn muting_a_row_keeps_its_text_but_drops_it_from_the_wire(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("X-Muted");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("still-here");

    let before = spec_of(&view, &mut cx);
    let total_before = before.headers.len();
    let enabled_before = before.enabled_headers().count();

    // Alt+T mutes whichever row holds focus.
    cx.simulate_keystrokes("alt-t");

    let after = spec_of(&view, &mut cx);
    assert_eq!(after.headers.len(), total_before, "the row must not be deleted");
    assert_eq!(
        after.enabled_headers().count(),
        enabled_before - 1,
        "a muted row must not go on the wire"
    );

    let muted = after.headers.last().unwrap();
    assert!(!muted.enabled);
    assert_eq!(muted.name, "X-Muted", "muting must not disturb typed text");
    assert_eq!(muted.value, "still-here");
}

#[gpui::test]
async fn rows_can_be_added_and_removed(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    let baseline = spec_of(&view, &mut cx).headers.len();

    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("X-Temporary");
    assert_eq!(spec_of(&view, &mut cx).headers.len(), baseline + 1);

    cx.simulate_keystrokes("ctrl-shift-k");
    assert_eq!(
        spec_of(&view, &mut cx).headers.len(),
        baseline,
        "Ctrl+Shift+K removes the focused row"
    );
}

#[gpui::test]
async fn query_params_are_editable_independently_of_headers(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    let headers_before = spec_of(&view, &mut cx).headers.len();

    cx.simulate_keystrokes("ctrl-shift-y");
    cx.simulate_input("page");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("2");

    let spec = spec_of(&view, &mut cx);
    let added = spec.query.last().expect("a query row was added");
    assert_eq!(added.name, "page");
    assert_eq!(added.value, "2");
    assert_eq!(
        spec.headers.len(),
        headers_before,
        "adding a query param must not touch headers"
    );
}

#[gpui::test]
async fn text_editing_keys_are_scoped_to_focused_inputs(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("https://a.test");
    cx.simulate_keystrokes("backspace backspace backspace backspace");
    assert_eq!(spec_of(&view, &mut cx).url, "https://a.");

    // Move focus off every text input, then press the same key. It must not edit
    // anything — this is what the `TextInput` context predicate buys us.
    cx.simulate_keystrokes("ctrl-shift-r");
    cx.simulate_keystrokes("backspace backspace");
    assert_eq!(
        spec_of(&view, &mut cx).url,
        "https://a.",
        "backspace outside a TextInput must not reach one"
    );
}

// ---------------------------------------------------------------------------
// The send loop (M1.2) — keystroke through the engine and back into view state
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ctrl_enter_sends_and_the_response_lands_in_the_view(cx: &mut TestAppContext) {
    let base = serve_once(OK_JSON);
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/health"));
    cx.simulate_keystrokes("ctrl-enter");

    let response: ResponseData = wait_for(&mut cx, "a response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });

    assert_eq!(response.status, 200);
    assert_eq!(response.status_text, "OK");
    assert_eq!(response.body_as_str(), Some("{\"ok\":true}"));
    assert_eq!(response.content_type(), Some("application/json"));
    assert!(response.timing.total >= response.timing.ttfb);

    // In-flight state must be cleared, and no error recorded.
    cx.update(|_, cx| {
        let view = view.read(cx);
        assert!(!view.is_sending(), "still marked as sending after completion");
        assert!(view.error.is_none(), "unexpected error: {:?}", view.error);
    });
}

#[gpui::test]
async fn the_url_bar_enter_key_also_sends(cx: &mut TestAppContext) {
    let base = serve_once(OK_JSON);
    let (view, mut cx) = open_workspace(cx);

    // Bare `enter` is bound only under the UrlBar context, and focus starts there.
    type_url(&mut cx, &format!("{base}/health"));
    cx.simulate_keystrokes("enter");

    let response: ResponseData = wait_for(&mut cx, "a response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });
    assert_eq!(response.status, 200);
}

#[gpui::test]
async fn a_connection_failure_is_shown_and_not_mistaken_for_a_response(
    cx: &mut TestAppContext,
) {
    let base = closed_port();
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/"));
    cx.simulate_keystrokes("ctrl-enter");

    let error: EngineError = wait_for(&mut cx, "an error", |cx| {
        cx.update(|_, cx| view.read(cx).error.clone())
    });

    assert!(
        matches!(error, EngineError::Connect { .. }),
        "expected a Connect error, got {error:?}"
    );
    cx.update(|_, cx| {
        let view = view.read(cx);
        assert!(view.response.is_none(), "a failure must not leave a response");
        assert!(!view.is_sending());
    });
}

#[gpui::test]
async fn a_local_failure_is_reported_without_touching_the_network(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    // No server anywhere — an unresolved variable must fail before any socket opens.
    type_url(&mut cx, "{{baseUrl}}/users");
    cx.simulate_keystrokes("ctrl-enter");

    let error: EngineError = wait_for(&mut cx, "an error", |cx| {
        cx.update(|_, cx| view.read(cx).error.clone())
    });

    assert!(matches!(error, EngineError::UnresolvedVariable { .. }));
    assert!(error.is_local());
}

#[gpui::test]
async fn escape_cancels_an_in_flight_request(cx: &mut TestAppContext) {
    let base = serve_never();
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/slow"));
    cx.simulate_keystrokes("ctrl-enter");

    // Confirm it really is in flight before cancelling, so this tests cancellation and
    // not a race with submission.
    wait_for(&mut cx, "the request to start", |cx| {
        cx.update(|_, cx| view.read(cx).is_sending().then_some(()))
    });

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    cx.update(|_, cx| {
        let view = view.read(cx);
        assert!(!view.is_sending(), "escape did not clear the in-flight state");
        assert!(view.response.is_none());
        assert!(
            view.error.is_none(),
            "a cancellation is not a failure, but got {:?}",
            view.error
        );
    });
}

#[gpui::test]
async fn the_in_flight_hint_names_a_key_that_actually_cancels(cx: &mut TestAppContext) {
    // The pane used to say "Ctrl+C or Escape to cancel", and `ctrl-c` has only ever been bound to
    // `text_input::Copy` — so half that sentence told people to press a key that does nothing to a
    // request. The hint is read from the keymap now, and this test presses whatever it names rather
    // than what a human thought it said.
    let base = serve_never();
    let (window, view, mut cx) = boot(cx, None, None);

    type_url(&mut cx, &format!("{base}/slow"));
    cx.simulate_keystrokes("ctrl-enter");
    wait_for(&mut cx, "the request to start", |cx| {
        cx.update(|_, cx| view.read(cx).is_sending().then_some(()))
    });

    let advertised = window
        .update(&mut cx, |_, window, _| {
            crate::workspace::keybinding_hint(&crate::actions::CancelRequest, window)
        })
        .expect("window");
    assert!(
        !advertised.is_empty(),
        "the pane has to be able to name a key at all"
    );

    cx.simulate_keystrokes(&advertised);
    cx.run_until_parked();

    assert!(
        !cx.update(|_, cx| view.read(cx).is_sending()),
        "the keystroke the pane advertises ({advertised:?}) must actually cancel the request"
    );
}

/// Every action whose keystroke appears in a piece of UI copy.
///
/// A list rather than "all actions", because only these are *advertised* — an action reachable
/// only from the palette can be unbound without any sentence going stale.
fn advertised_actions() -> Vec<Box<dyn gpui::Action>> {
    use crate::actions::*;
    vec![
        Box::new(AddHeader),
        Box::new(AddQuery),
        Box::new(AddFormField),
        Box::new(AddMultipartField),
        Box::new(ChooseBodyFile),
        Box::new(OpenBodyType),
        Box::new(ShowHistory),
        Box::new(SendRequest),
        Box::new(SaveRequest),
        Box::new(SaveResponse),
        Box::new(ToggleTheme),
        Box::new(OpenRequest),
        Box::new(OpenPalette),
        Box::new(SwitchEnvironment),
        Box::new(CancelRequest),
    ]
}

#[gpui::test]
async fn keybinding_label_matches_the_keymap(cx: &mut TestAppContext) {
    // Ten pieces of UI copy used to write their own keystroke as a literal — "Ctrl+Shift+H to
    // add", "Ctrl+H to pick another", and so on — while `keybinding_hint` had exactly one caller
    // and the docs told the story as though the class were closed. They read from the keymap now,
    // through `keybinding_label`, which spells a binding `Ctrl+Shift+H` instead of gpui's
    // `ctrl-shift-h` so the prose didn't visibly regress for the sake of the fix.
    //
    // **That second spelling is the risk this test exists for.** A hand-written formatter can
    // disagree with the keymap it claims to read — wrong modifier order, a mangled punctuation
    // key — and every symptom would be cosmetic and unnoticed. So: lowercase the label back into
    // gpui's form and require it to equal what gpui itself produced.
    let (window, _view, mut cx) = boot(cx, None, None);

    for action in advertised_actions() {
        let (label, hint) = window
            .update(&mut cx, |_, window, _| {
                (
                    crate::workspace::keybinding_label(action.as_ref(), window),
                    crate::workspace::keybinding_hint(action.as_ref(), window),
                )
            })
            .expect("window");

        assert!(
            !label.is_empty(),
            "{} is named in UI copy but has no binding, so the copy would render a hole",
            action.name()
        );
        // Both sides lowercased: gpui spells a shifted letter `ctrl-shift-H` while the label
        // capitalizes every part, so the *key's* case legitimately differs. What must not differ
        // is the modifier set, its order, or the key itself.
        assert_eq!(
            label.to_lowercase().replace('+', "-"),
            hint.to_lowercase(),
            "the two spellings of {}'s binding disagree",
            action.name()
        );
    }
}

#[gpui::test]
async fn migrated_hints_render_exactly_what_the_literals_did(cx: &mut TestAppContext) {
    // The migration's real risk isn't a wrong key — it's a *differently spelled* key, quietly
    // changing ten pieces of copy as a side effect of making them correct. These are the literals
    // that were in the source before, character for character.
    //
    // This also makes the labels load-bearing in the other direction: rebind any of these and the
    // test fails, which is the moment to notice that the copy now says something new.
    let (window, _view, mut cx) = boot(cx, None, None);
    use crate::actions::*;

    let expected: Vec<(Box<dyn gpui::Action>, &str)> = vec![
        (Box::new(AddHeader), "Ctrl+Shift+H"),
        (Box::new(AddQuery), "Ctrl+Shift+Y"),
        (Box::new(AddFormField), "Ctrl+Shift+F"),
        (Box::new(AddMultipartField), "Ctrl+Shift+M"),
        (Box::new(ChooseBodyFile), "Ctrl+Shift+O"),
        (Box::new(OpenBodyType), "Ctrl+Shift+B"),
        (Box::new(ShowHistory), "Ctrl+H"),
        (Box::new(SendRequest), "Ctrl+Enter"),
        (Box::new(SaveRequest), "Ctrl+S"),
        (Box::new(SaveResponse), "Ctrl+Shift+S"),
        (Box::new(ToggleTheme), "Ctrl+Shift+T"),
        (Box::new(OpenRequest), "Ctrl+P"),
        (Box::new(OpenPalette), "Ctrl+K"),
        (Box::new(SwitchEnvironment), "Ctrl+E"),
    ];

    for (action, want) in expected {
        let got = window
            .update(&mut cx, |_, window, _| {
                crate::workspace::keybinding_label(action.as_ref(), window)
            })
            .expect("window");
        assert_eq!(got, want, "{}'s label changed", action.name());
    }
}

#[gpui::test]
async fn an_advertised_key_actually_fires_its_action(cx: &mut TestAppContext) {
    // The stronger half: a label that merely *parses* is not the same as one that works. Take the
    // key the copy advertises for "add a header", press it, and require a header row to appear.
    // This is the shape the in-flight cancel hint is tested in, generalised to the copy that used
    // to be hardcoded.
    let (window, view, mut cx) = boot(cx, None, None);

    let label = window
        .update(&mut cx, |_, window, _| {
            crate::workspace::keybinding_label(&crate::actions::AddHeader, window)
        })
        .expect("window");

    let before = cx.update(|_, cx| view.read(cx).headers.len());

    // Back to gpui's spelling, which is what `simulate_keystrokes` parses.
    cx.simulate_keystrokes(&label.to_lowercase().replace('+', "-"));
    cx.run_until_parked();

    assert_eq!(
        cx.update(|_, cx| view.read(cx).headers.len()),
        before + 1,
        "pressing the advertised key ({label:?}) must add a header"
    );
}

#[gpui::test]
async fn a_hint_for_an_unbound_action_drops_the_clause(cx: &mut TestAppContext) {
    // The failure mode of a keymap-derived hint: an unbound action yields an empty key, and
    // interpolating that leaves "No headers —  to add" — uglier than the literal it replaced.
    // `hint_sentence` drops the clause instead, and the dash with it when nothing survives.
    //
    // `ClearCookies` is the only `zuno::` action with no binding at all (palette-only), which is
    // what makes it the one honest probe for this. The first assertion guards the premise: if it
    // ever gains a binding, this test would silently start proving nothing.
    let (window, _view, mut cx) = boot(cx, None, None);
    use crate::actions::{AddHeader, ClearCookies};

    let (unbound, dropped, kept, mixed) = window
        .update(&mut cx, |_, window, _| {
            (
                crate::workspace::keybinding_label(&ClearCookies, window),
                crate::workspace::hint_sentence(
                    "No cookies",
                    &[(&ClearCookies as &dyn gpui::Action, "to clear")],
                    window,
                ),
                crate::workspace::hint_sentence(
                    "No headers",
                    &[(&AddHeader as &dyn gpui::Action, "to add")],
                    window,
                ),
                crate::workspace::hint_sentence(
                    "No parts",
                    &[
                        (&AddHeader as &dyn gpui::Action, "to add"),
                        (&ClearCookies as &dyn gpui::Action, "to clear"),
                    ],
                    window,
                ),
            )
        })
        .expect("window");

    assert!(
        unbound.is_empty(),
        "ClearCookies is supposed to be unbound; this test proves nothing otherwise"
    );
    assert_eq!(dropped, "No cookies", "the whole clause and the dash must go");
    assert_eq!(
        kept, "No headers — Ctrl+Shift+H to add",
        "a bound action still renders, in the conventional spelling"
    );
    assert_eq!(
        mixed, "No parts — Ctrl+Shift+H to add",
        "one unbound clause among several drops only itself"
    );
}

#[gpui::test]
async fn resending_replaces_the_previous_response(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    let first = serve_once(OK_JSON);
    type_url(&mut cx, &format!("{first}/one"));
    cx.simulate_keystrokes("ctrl-enter");
    wait_for(&mut cx, "the first response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });

    const SECOND: &str = "HTTP/1.1 404 Not Found\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: 9\r\n\
         \r\n\
         not found";
    let second = serve_once(SECOND);
    type_url(&mut cx, &format!("{second}/two"));
    cx.simulate_keystrokes("ctrl-enter");

    let response: ResponseData = wait_for(&mut cx, "the second response", |cx| {
        cx.update(|_, cx| {
            view.read(cx)
                .response
                .clone()
                .filter(|response| response.status == 404)
        })
    });
    assert_eq!(response.body_as_str(), Some("not found"));
}

#[gpui::test]
async fn edits_made_before_sending_are_the_ones_that_go_out(cx: &mut TestAppContext) {
    let base = serve_once(OK_JSON);
    let (view, mut cx) = open_workspace(cx);

    // The whole point of deriving the spec instead of storing one: what's on screen at
    // the moment of Send is what gets sent.
    type_url(&mut cx, &format!("{base}/derived"));
    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("X-Derived");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("yes");

    let spec = spec_of(&view, &mut cx);
    assert!(spec.url.ends_with("/derived"));
    assert_eq!(spec.headers.last().unwrap().name, "X-Derived");

    cx.simulate_keystrokes("ctrl-enter");
    let response: ResponseData = wait_for(&mut cx, "a response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });
    assert_eq!(response.status, 200);
}

// ---------------------------------------------------------------------------
// The response viewer (M1.3) — a real response, indexed off-thread
// ---------------------------------------------------------------------------

/// Wait until the background body index has landed.
fn wait_for_body(
    view: &gpui::Entity<RequestView>,
    cx: &mut VisualTestContext,
) -> (bool, usize, usize) {
    wait_for(cx, "the body index", |cx| {
        cx.update(|_, cx| {
            view.read(cx).body_view.as_ref().map(|body| {
                (
                    body.is_json(),
                    body.row_count(),
                    body.outline().map(|o| o.len()).unwrap_or(0),
                )
            })
        })
    })
}

#[gpui::test]
async fn a_json_response_is_indexed_off_thread(cx: &mut TestAppContext) {
    const BODY: &str = "{\"a\":1,\"b\":[2,3]}";
    let response: &'static str = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{BODY}",
            BODY.len()
        )
        .into_boxed_str(),
    );

    let base = serve_once(response);
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/json"));
    cx.simulate_keystrokes("ctrl-enter");

    let (is_json, visible, total) = wait_for_body(&view, &mut cx);
    assert!(is_json, "an application/json body should be parsed");
    // { , "a":1 , "b":[ , 2 , 3 , ] , }
    assert_eq!(total, 7);
    assert_eq!(visible, 7, "nothing folded initially");
}

#[gpui::test]
async fn folding_hides_rows_without_losing_them(cx: &mut TestAppContext) {
    const BODY: &str = "{\"outer\":{\"x\":1,\"y\":2},\"z\":3}";
    let response: &'static str = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{BODY}",
            BODY.len()
        )
        .into_boxed_str(),
    );

    let base = serve_once(response);
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/json"));
    cx.simulate_keystrokes("ctrl-enter");

    let (_, visible_before, total) = wait_for_body(&view, &mut cx);
    assert_eq!(visible_before, total);

    // Row 1 is the nested object's open row. Folding acts on the selection now — one verb for
    // the chevron, the double-click and the menu — so select it first.
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.select_body_row_at(1, cx);
            view.toggle_selected_fold(cx);
        })
    });
    cx.run_until_parked();

    let visible_after = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count());
    assert!(
        visible_after < visible_before,
        "folding should hide rows ({visible_after} vs {visible_before})"
    );

    // The rows still exist — folding is a view concern, not a data one.
    let still_total = cx.update(|_, cx| {
        view.read(cx)
            .body_view
            .as_ref()
            .unwrap()
            .outline()
            .unwrap()
            .len()
    });
    assert_eq!(still_total, total, "folding must not discard rows");
}

#[gpui::test]
async fn a_non_json_response_falls_back_to_lines(cx: &mut TestAppContext) {
    const RESPONSE: &str = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: 11\r\n\
         \r\n\
         one\ntwo\nsix";

    let base = serve_once(RESPONSE);
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/text"));
    cx.simulate_keystrokes("ctrl-enter");

    let (is_json, rows, _) = wait_for_body(&view, &mut cx);
    assert!(!is_json, "text/plain should not be parsed as JSON");
    assert_eq!(rows, 3, "three lines");
}

#[gpui::test]
async fn malformed_json_shows_raw_text_with_a_notice(cx: &mut TestAppContext) {
    const RESPONSE: &str = "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: 8\r\n\
         \r\n\
         {\"a\":1,}";

    let base = serve_once(RESPONSE);
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/bad"));
    cx.simulate_keystrokes("ctrl-enter");

    let (is_json, rows, _) = wait_for_body(&view, &mut cx);
    assert!(!is_json, "invalid JSON must not be shown as parsed");
    assert!(rows > 0, "the raw body must still be visible");

    let notice = cx.update(|_, cx| {
        view.read(cx)
            .body_view
            .as_ref()
            .unwrap()
            .notice
            .as_ref()
            .map(|n| format!("{n:?}"))
    });
    assert!(
        notice.is_some_and(|n| n.contains("ParseFailed")),
        "the user must be told why it isn't a tree"
    );
}

#[gpui::test]
async fn resending_reindexes_the_new_body(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    let first = serve_once(OK_JSON);
    type_url(&mut cx, &format!("{first}/one"));
    cx.simulate_keystrokes("ctrl-enter");
    let (is_json, _, _) = wait_for_body(&view, &mut cx);
    assert!(is_json);

    const TEXT: &str = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: 5\r\n\
         \r\n\
         plain";
    let second = serve_once(TEXT);
    type_url(&mut cx, &format!("{second}/two"));
    cx.simulate_keystrokes("ctrl-enter");

    // The stale JSON index must be replaced, not kept alongside.
    let is_json = wait_for(&mut cx, "the reindexed body", |cx| {
        cx.update(|_, cx| {
            view.read(cx)
                .body_view
                .as_ref()
                .map(BodyView::is_json)
                .filter(|is_json| !is_json)
        })
    });
    assert!(!is_json);
}

// ---------------------------------------------------------------------------
// The loop (M1.4) — body editor and diffing
// ---------------------------------------------------------------------------

#[gpui::test]
async fn the_body_editor_accepts_multiple_lines(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    clear_body(&mut cx);
    cx.simulate_input("{");
    // Bare `enter` inserts a newline here, where it *sends* in the URL bar. Same key,
    // two meanings, separated only by key context.
    cx.simulate_keystrokes("enter");
    cx.simulate_input("  \"a\": 1");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("}");

    let spec = spec_of(&view, &mut cx);
    let Body::Raw { text, kind } = &spec.body else {
        panic!("expected a raw body, got {:?}", spec.body);
    };
    assert_eq!(*kind, RawKind::Json);
    assert!(text.contains('\n'), "newlines should survive: {text:?}");
    assert_eq!(text.lines().count(), 3, "{text:?}");
    assert!(text.starts_with('{'));
    assert!(text.contains("\"a\": 1"), "{text:?}");
}

#[gpui::test]
async fn enter_in_the_body_editor_does_not_send(cx: &mut TestAppContext) {
    let base = serve_once(OK_JSON);
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/never"));
    clear_body(&mut cx);
    cx.simulate_input("hello");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    cx.update(|_, cx| {
        let view = view.read(cx);
        assert!(!view.is_sending(), "enter in the editor must not start a request");
        assert!(view.response.is_none());
    });
}

#[gpui::test]
async fn a_new_line_inherits_the_previous_indent(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    clear_body(&mut cx);
    cx.simulate_input("    indented");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("next");

    let spec = spec_of(&view, &mut cx);
    let Body::Raw { text, .. } = &spec.body else {
        panic!("expected a raw body");
    };
    let second = text.lines().nth(1).expect("a second line");
    assert_eq!(second, "    next", "indent should carry over: {text:?}");
}

#[gpui::test]
async fn vertical_movement_lands_on_the_line_above(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    clear_body(&mut cx);
    cx.simulate_input("aaa");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("bbb");

    // Up then Home puts the cursor at the start of the first line; typing there proves
    // where it landed.
    cx.simulate_keystrokes("up home");
    cx.simulate_input("X");

    let spec = spec_of(&view, &mut cx);
    let Body::Raw { text, .. } = &spec.body else {
        panic!("expected a raw body");
    };
    assert_eq!(text.lines().next(), Some("Xaaa"), "{text:?}");
}

#[gpui::test]
async fn a_blank_body_sends_nothing_at_all(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    clear_body(&mut cx);

    // An empty editor must mean no body, not an empty JSON body with a Content-Type.
    assert_eq!(spec_of(&view, &mut cx).body, Body::Empty);
}

#[gpui::test]
async fn the_body_sub_kind_is_chosen_by_name(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    clear_body(&mut cx);
    cx.simulate_input("x");
    assert!(matches!(
        spec_of(&view, &mut cx).body,
        Body::Raw { kind: RawKind::Json, .. }
    ));

    // Ctrl+Shift+B used to cycle RawKind; it now opens the picker, so the sub-kind is
    // chosen by name rather than reached by repetition.
    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("text");
    cx.simulate_keystrokes("enter");
    assert!(matches!(
        spec_of(&view, &mut cx).body,
        Body::Raw { kind: RawKind::Text, .. }
    ));
}

#[gpui::test]
async fn the_first_run_has_nothing_to_diff_against(cx: &mut TestAppContext) {
    let base = serve_once(OK_JSON);
    let (view, mut cx) = open_workspace(cx);

    type_url(&mut cx, &format!("{base}/one"));
    cx.simulate_keystrokes("ctrl-enter");
    wait_for_body(&view, &mut cx);

    cx.update(|_, cx| {
        let view = view.read(cx);
        assert!(view.diff.is_none(), "no previous run exists");
        assert!(view.history.is_empty());
    });
}

#[gpui::test]
async fn a_changed_response_is_diffed_against_the_previous_run(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    let first = serve_once(OK_JSON);
    type_url(&mut cx, &format!("{first}/one"));
    cx.simulate_keystrokes("ctrl-enter");
    wait_for_body(&view, &mut cx);

    const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\n\
         Content-Type: application/json\r\n\
         Content-Length: 12\r\n\
         \r\n\
         {\"e\":\"gone\"}";
    let second = serve_once(NOT_FOUND);
    type_url(&mut cx, &format!("{second}/two"));
    cx.simulate_keystrokes("ctrl-enter");

    let diff = wait_for(&mut cx, "a diff", |cx| {
        cx.update(|_, cx| view.read(cx).diff.clone())
    });

    assert_eq!(diff.status, Some((200, 404)));
    assert!(diff.body_changed);
    assert!(!diff.is_quiet());

    // The superseded response is retained for comparison.
    cx.update(|_, cx| {
        let view = view.read(cx);
        assert_eq!(view.history.len(), 1);
        assert_eq!(view.history[0].status, 200);
    });
}

#[gpui::test]
async fn an_identical_resend_reports_no_change(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    let first = serve_once(OK_JSON);
    type_url(&mut cx, &format!("{first}/same"));
    cx.simulate_keystrokes("ctrl-enter");
    wait_for_body(&view, &mut cx);

    // Byte-identical response from a fresh server. Only timing differs, and timing
    // alone must never claim a change.
    let second = serve_once(OK_JSON);
    type_url(&mut cx, &format!("{second}/same"));
    cx.simulate_keystrokes("ctrl-enter");

    let diff = wait_for(&mut cx, "a diff", |cx| {
        cx.update(|_, cx| view.read(cx).diff.clone())
    });
    assert!(diff.is_quiet(), "expected a quiet diff, got {diff:?}");
}

#[gpui::test]
async fn the_diff_describes_the_two_most_recent_runs(cx: &mut TestAppContext) {
    // The diff is computed off-thread now, so it arrives a frame or two after the response and
    // has to describe the *right* pair when it does. Three runs rather than two: with only two,
    // "diffed against the previous run" and "diffed against the first run" are the same
    // assertion, so a wrong baseline would pass.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, r#"{"a":1}"#), (201, r#"{"b":2}"#), (202, r#"{"c":3}"#)]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    send_and_wait(&mut cx, &view, 201);
    send_and_wait(&mut cx, &view, 202);

    // `apply` clears `diff` when the response lands, so anything found here is the new one
    // rather than a leftover from the run before.
    let diff = wait_for(&mut cx, "the diff for the newest run", |cx| {
        cx.update(|_, cx| view.read(cx).diff.clone())
    });

    assert_eq!(
        diff.status,
        Some((201, 202)),
        "the baseline must be the run this one replaced, not an older one"
    );
    assert!(diff.body_changed);

    // And the retired run is the one it was compared against.
    cx.update(|_, cx| {
        let view = view.read(cx);
        assert_eq!(view.history[0].status, 201);
        assert_eq!(view.response.as_ref().expect("live").status, 202);
    });
}

#[gpui::test]
async fn a_failure_clears_the_diff_but_keeps_the_baseline(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    let first = serve_once(OK_JSON);
    type_url(&mut cx, &format!("{first}/ok"));
    cx.simulate_keystrokes("ctrl-enter");
    wait_for_body(&view, &mut cx);

    let dead = closed_port();
    type_url(&mut cx, &format!("{dead}/gone"));
    cx.simulate_keystrokes("ctrl-enter");

    wait_for(&mut cx, "an error", |cx| {
        cx.update(|_, cx| view.read(cx).error.clone())
    });

    cx.update(|_, cx| {
        let view = view.read(cx);
        assert!(view.diff.is_none(), "a failed run has nothing to compare");
        assert!(
            view.response.is_some(),
            "the last good response is the baseline for the next send"
        );
    });
}

// ---------------------------------------------------------------------------
// curl import
// ---------------------------------------------------------------------------

fn put_on_clipboard(cx: &mut VisualTestContext, text: &str) {
    let text = text.to_string();
    cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)));
}

#[gpui::test]
async fn a_curl_command_on_the_clipboard_becomes_the_request(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);

    put_on_clipboard(
        &mut cx,
        r#"curl 'https://api.example.com/v2/items?page=2' \
  -H 'accept: application/json' \
  -H 'content-type: application/json' \
  --data-raw '{"name":"zuno"}' \
  --compressed"#,
    );
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();

    // An import opens a *new* buffer, so the handle taken at open is no longer the one to
    // read — that's asserted on its own in
    // `a_curl_import_opens_a_new_buffer_instead_of_replacing_one`. This test is about the
    // parse landing correctly, so it reads whatever is now in front.
    let (_, spec) = tabs_of(&window, &mut cx);

    assert_eq!(spec.method, Method::Post);
    assert_eq!(spec.url, "https://api.example.com/v2/items?page=2");
    assert_eq!(spec.headers.len(), 2);
    assert_eq!(spec.name, "items");
    assert!(matches!(
        spec.body,
        Body::Raw { kind: RawKind::Json, .. }
    ));
}

#[gpui::test]
async fn an_unparseable_clipboard_reports_instead_of_wrecking_the_request(
    cx: &mut TestAppContext,
) {
    let (view, mut cx) = open_workspace(cx);

    let before = spec_of(&view, &mut cx);
    // Genuinely unparseable: an unbalanced quote. (Prose is rejected too, but by the
    // NotCurl guard rather than the tokenizer.)
    put_on_clipboard(&mut cx, "curl 'https://x.test/a");
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();

    // `NoUrl` is the failure here, and the existing request must be untouched.
    let after = spec_of(&view, &mut cx);
    assert_eq!(before.url, after.url);
    assert_eq!(before.method, after.method);

    let status = cx.update(|_, cx| view.read(cx).status.clone());
    assert!(
        status.is_some_and(|s| s.contains("Could not import")),
        "the failure should be reported"
    );
}

#[gpui::test]
async fn a_composed_replacement_leaves_a_copyable_selection(cx: &mut TestAppContext) {
    // Driven through `EntityInputHandler` directly, because that is the surface an IME talks to and
    // there is no way to reach it with keystrokes. The case that matters is replacing a *non-empty*
    // range mid-composition: with both ends of the new selection offset by `range.start` it lands
    // on the inserted text, while offsetting the end by `range.end` overshoots past the end of the
    // content — and `copy` then slices with it and panics. At an insertion point the two are
    // identical, which is why this went unnoticed.
    use gpui::EntityInputHandler;

    let (view, mut cx) = open_workspace(cx);
    type_url(&mut cx, "abcdef");
    let url = cx.update(|_, cx| view.read(cx).url.clone());

    // Replace "bcd" with "XY" and mark it, selecting the inserted text as an IME would.
    cx.update(|window, cx| {
        url.update(cx, |input, cx| {
            input.replace_and_mark_text_in_range(Some(1..4), "XY", Some(0..2), window, cx)
        })
    });
    assert_eq!(spec_of(&view, &mut cx).url, "aXYef");

    let selection = cx
        .update(|window, cx| {
            url.update(cx, |input, cx| input.selected_text_range(false, window, cx))
        })
        .expect("a selection");
    assert_eq!(
        selection.range, 1..3,
        "the selection must cover the inserted text, not run past the end of the content"
    );

    // The real consequence: an out-of-range selection is a panic waiting for the next copy.
    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(
        clipboard_text(&mut cx).as_deref(),
        Some("XY"),
        "copying the composed selection must yield exactly the inserted text"
    );
}

#[gpui::test]
async fn newlines_never_enter_a_single_line_input(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    cx.simulate_keystrokes("ctrl-a");
    // `shape_line` carries a debug_assert against embedded newlines, so this would
    // panic in a debug build if sanitization regressed.
    cx.simulate_input("https://a.test\nmalicious\r\nsecond");

    let url = spec_of(&view, &mut cx).url;
    assert!(!url.contains('\n'), "newline leaked into the input: {url:?}");
    assert!(!url.contains('\r'), "carriage return leaked into the input: {url:?}");
}

/// A spec distinguishable from `sample()` by name, so restored tab order can be asserted.
fn named(name: &str) -> RequestSpec {
    RequestSpec {
        name: name.to_string(),
        ..RequestSpec::sample()
    }
}

/// The number of open buffers and which one is active.
fn tabs_of(
    window: &gpui::WindowHandle<Workspace>,
    cx: &mut VisualTestContext,
) -> (usize, RequestSpec) {
    window
        .update(cx, |workspace, _, cx| {
            (
                workspace.tab_count(),
                workspace.active().expect("an active buffer").read(cx).spec(cx),
            )
        })
        .expect("window")
}

#[gpui::test]
async fn ctrl_t_opens_a_buffer_and_focus_follows_it(cx: &mut TestAppContext) {
    let (window, first, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-t");
    let (count, _) = tabs_of(&window, &mut cx);
    assert_eq!(count, 2);

    // The real risk in a switch: a FocusHandle belongs to the entity that made it, so if
    // `activate` forgets to move focus it stays inside the *old* buffer and typing edits
    // the request you just navigated away from.
    cx.simulate_input("https://second.test");

    let (_, active) = tabs_of(&window, &mut cx);
    assert_eq!(active.url, "https://second.test", "typing must land in the new buffer");

    let untouched = spec_of(&first, &mut cx);
    assert_eq!(
        untouched.url,
        RequestSpec::sample().url,
        "the first buffer must be untouched"
    );
}

#[gpui::test]
async fn a_new_buffer_gets_a_distinct_id(cx: &mut TestAppContext) {
    // Duplicate ids would make the saved session ambiguous, and nothing allocates them —
    // `sample()` hardcodes 1 and `default()` 0.
    let (window, first, mut cx) = boot(cx, None, None);
    let original = spec_of(&first, &mut cx).id;

    cx.simulate_keystrokes("ctrl-t");
    let (_, new) = tabs_of(&window, &mut cx);
    assert_ne!(new.id, original, "a new buffer must not reuse an id");
}

#[gpui::test]
async fn ctrl_tab_cycles_buffers_and_wraps(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);

    // Three buffers, each identifiable by its URL.
    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://b.test");
    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://c.test");

    let urls = |cx: &mut VisualTestContext| tabs_of(&window, cx).1.url;

    // Sitting on the third, so one forward step must wrap to the first.
    cx.simulate_keystrokes("ctrl-tab");
    assert_eq!(urls(&mut cx), RequestSpec::sample().url, "next should wrap to the front");

    cx.simulate_keystrokes("ctrl-shift-tab");
    assert_eq!(urls(&mut cx), "https://c.test", "prev should wrap to the back");

    cx.simulate_keystrokes("ctrl-shift-tab");
    assert_eq!(urls(&mut cx), "https://b.test");
}

#[gpui::test]
async fn ctrl_w_closes_a_buffer_and_leaves_focus_usable(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://second.test");
    cx.simulate_keystrokes("ctrl-w");

    let (count, active) = tabs_of(&window, &mut cx);
    assert_eq!(count, 1);
    assert_eq!(active.url, RequestSpec::sample().url, "closing should fall back left");

    // After a close, focus is inside a dropped entity unless `activate` moved it — and
    // then no key context matches, so the keymap goes dead with nothing on screen saying
    // so. Typing is the only way to prove it recovered.
    cx.simulate_input("/extra");
    let (_, after) = tabs_of(&window, &mut cx);
    assert!(
        after.url.contains("/extra"),
        "typing after a close must reach the surviving buffer, got {:?}",
        after.url
    );
}

#[gpui::test]
async fn closing_a_buffer_cancels_its_in_flight_request(cx: &mut TestAppContext) {
    // Dropping the buffer drops the task that consumes events, which *looks* like
    // cancellation and isn't: the socket keeps draining into a buffer nothing will read until
    // the timeout. `Engine::cancel` is the half that stops it.
    //
    // Observable because this test holds an `Entity` handle, so the buffer outlives its
    // removal from `views` and can still be read. Without the cancel, `inflight` is still
    // `Some` in it.
    let base = serve_never();
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-t");
    let doomed = active_view(&window, &mut cx);
    cx.simulate_input(&format!("{base}/slow"));
    cx.simulate_keystrokes("ctrl-enter");

    // Confirm it really is in flight, so this tests cancellation and not a race with
    // submission.
    wait_for(&mut cx, "the request to start", |cx| {
        cx.update(|_, cx| doomed.read(cx).is_sending().then_some(()))
    });

    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();

    assert_eq!(tabs_of(&window, &mut cx).0, 1, "the buffer should be gone");
    assert!(
        !cx.update(|_, cx| doomed.read(cx).is_sending()),
        "closing a tab must abandon its request, not just stop listening to it"
    );
}

#[gpui::test]
async fn closing_the_last_buffer_leaves_a_fresh_one(cx: &mut TestAppContext) {
    // An empty `views` makes `active()` return None, which every handler reads as "do
    // nothing" — a window that is still there and silently inert. Ctrl+W must not do that,
    // and must not quit either; that's Ctrl+Q's job.
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-w");

    let (count, active) = tabs_of(&window, &mut cx);
    assert_eq!(count, 1, "there must always be a buffer");
    assert_eq!(active.url, "", "and it should be a fresh one");

    cx.simulate_input("https://after-close.test");
    assert_eq!(tabs_of(&window, &mut cx).1.url, "https://after-close.test");
}

#[gpui::test]
async fn a_curl_import_opens_a_new_buffer_instead_of_replacing_one(cx: &mut TestAppContext) {
    // Replacing was only defensible while there was nowhere else to put the result: an
    // import over unsaved work destroyed it with no undo.
    let (window, first, mut cx) = boot(cx, None, None);
    let before = spec_of(&first, &mut cx);

    put_on_clipboard(&mut cx, "curl https://imported.test/widgets -H 'X-Key: abc'");
    cx.simulate_keystrokes("ctrl-shift-v");

    let (count, active) = tabs_of(&window, &mut cx);
    assert_eq!(count, 2, "the import should open a buffer");
    assert_eq!(active.url, "https://imported.test/widgets");
    assert_ne!(active.id, before.id, "and not collide with the existing id");

    assert_eq!(
        spec_of(&first, &mut cx).url,
        before.url,
        "the original buffer must survive an import"
    );
}

#[gpui::test]
async fn a_tab_is_labelled_from_its_url_as_it_is_typed(cx: &mut TestAppContext) {
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/v2/invoices");

    let label = cx.update(|_, cx| view.read(cx).label(cx));
    assert_eq!(label, "invoices", "the label must track the URL, not the stale name");
}

#[gpui::test]
async fn every_open_buffer_is_persisted_on_send(cx: &mut TestAppContext) {
    // The multi-buffer half of the save path, which could not be written before `NewTab`
    // existed — a send is a save point, so it checkpoints the whole window.
    let path = scratch_dir("multisave").join("session.json");
    let (_, _, mut cx) = boot(cx, Some(path.clone()), None);

    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://kept.test/two");

    let url = serve_once(OK_JSON);
    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input(&url);
    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();

    let written = std::fs::read(&path).expect("the send should have written a session");
    let session: crate::session::Session = serde_json::from_slice(&written).expect("envelope");

    assert_eq!(session.tabs.len(), 3, "every buffer, not just the one that sent");
    assert_eq!(session.active, 2, "and which one was in front");
    assert_eq!(session.tabs[1].spec.url, "https://kept.test/two");
    assert_eq!(session.tabs[2].spec.url, url);

    // Ids have to stay distinct through a round trip, or the file is ambiguous.
    let mut ids: Vec<_> = session.tabs.iter().map(|tab| tab.spec.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "saved buffers must have distinct ids");

    remove_scratch(&mut cx, &path);
}

#[gpui::test]
async fn every_saved_buffer_is_restored_not_just_the_active_one(cx: &mut TestAppContext) {
    // Boots against a real file rather than calling `session::load` directly, because the
    // bug this guards is in `Workspace::new` — building one view and dropping the rest —
    // not in the parsing, which `session`'s own tests already cover.
    let path = scratch_dir("restore").join("session.json");
    let session = crate::session::Session::new(
        vec![named("first"), named("second"), named("third")]
            .into_iter()
            .map(crate::session::Tab::scratch)
            .collect(),
        1, // not 0, so ignoring `active` fails the test rather than passing by luck
        None,
    );
    std::fs::write(&path, serde_json::to_vec(&session).expect("serialize")).expect("write");

    let (window, view, mut cx) = boot(cx, Some(path.clone()), None);

    let tabs = window
        .update(&mut cx, |workspace, _, _| workspace.tab_count())
        .expect("window");
    assert_eq!(tabs, 3, "all three buffers should be open");
    assert_eq!(
        spec_of(&view, &mut cx).name,
        "second",
        "the buffer that was in front should be in front again"
    );

    remove_scratch(&mut cx, &path);
}

#[gpui::test]
async fn an_out_of_range_active_index_opens_a_window_instead_of_panicking(
    cx: &mut TestAppContext,
) {
    // Hand-edited or truncated file. `Workspace::new` indexes `views[active_ix]` directly,
    // trusting `session::load` to have clamped — this is the test that keeps that contract
    // honest from the other side.
    let path = scratch_dir("clamped").join("session.json");
    let json = format!(
        r#"{{"version":1,"active":9,"tabs":[{}]}}"#,
        serde_json::to_string(&named("only")).expect("serialize")
    );
    std::fs::write(&path, json).expect("write");

    let (_, view, mut cx) = boot(cx, Some(path.clone()), None);
    assert_eq!(spec_of(&view, &mut cx).name, "only");

    remove_scratch(&mut cx, &path);
}

#[gpui::test]
async fn a_session_written_by_m1_still_opens(cx: &mut TestAppContext) {
    // The bare-spec format M1 shipped. Read through the real boot path, since the point is
    // that an existing install keeps its request after this change — the exact failure
    // mode `cookie_store` caused (CLAUDE.md, "Lessons").
    let path = scratch_dir("legacy").join("session.json");
    let spec = named("saved by m1");
    std::fs::write(&path, serde_json::to_vec(&spec).expect("serialize")).expect("write");

    let (window, view, mut cx) = boot(cx, Some(path.clone()), None);

    let tabs = window
        .update(&mut cx, |workspace, _, _| workspace.tab_count())
        .expect("window");
    assert_eq!(tabs, 1);
    assert_eq!(spec_of(&view, &mut cx).name, "saved by m1");

    remove_scratch(&mut cx, &path);
}

#[gpui::test]
async fn a_send_checkpoints_every_open_buffer(cx: &mut TestAppContext) {
    // A send is a save point, so this covers the *write* half of restore end to end.
    // With one buffer constructible today it can only prove the envelope reaches disk in
    // the new format; the multi-buffer assertion arrives with `NewTab`.
    let path = scratch_dir("checkpoint").join("session.json");
    let (_, view, mut cx) = boot(cx, Some(path.clone()), None);

    let url = serve_once(OK_JSON);
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    cx.simulate_keystrokes("ctrl-enter");
    wait_for(&mut cx, "the response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });

    let written = std::fs::read(&path).expect("the send should have written a session");
    let session: crate::session::Session =
        serde_json::from_slice(&written).expect("it should be an envelope, not a bare spec");
    assert_eq!(session.tabs.len(), 1);
    assert_eq!(session.active, 0);
    assert_eq!(session.tabs[0].spec.url, url);

    remove_scratch(&mut cx, &path);
}

/// A collection root and a session file in the same scratch directory, so a test can
/// restart the app against both.
fn scratch_collection(name: &str) -> (PathBuf, PathBuf) {
    let dir = scratch_dir(name);
    (dir.join("session.json"), dir.join("collections"))
}

/// Every `.json` in a collection root, sorted, for asserting what a save actually wrote.
fn collection_files(root: &PathBuf) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    names
}

#[gpui::test]
async fn ctrl_s_writes_the_request_as_a_file_named_from_its_url(cx: &mut TestAppContext) {
    let (session, root) = scratch_collection("save");
    let (_, view, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/v1/invoices");
    cx.simulate_keystrokes("ctrl-s");

    assert_eq!(collection_files(&root), ["invoices.json"]);

    // What landed on disk has to be the request as it appears on screen, not a stale copy.
    let bytes = std::fs::read(root.join("invoices.json")).expect("read");
    let saved: RequestSpec = serde_json::from_slice(&bytes).expect("parse");
    assert_eq!(saved.url, "https://api.test/v1/invoices");

    // The buffer now knows where it lives, which is what the next test depends on.
    let path = cx.update(|_, cx| view.read(cx).path.clone());
    assert_eq!(path, Some(root.join("invoices.json")));

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn saving_twice_overwrites_rather_than_making_a_second_file(cx: &mut TestAppContext) {
    // The bug `RequestView::path` exists to prevent: a filename derived from the URL is
    // not an identity, so without remembering the file, the second save finds
    // `invoices.json` taken and writes `invoices-2.json`.
    let (session, root) = scratch_collection("resave");
    let (_, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/v1/invoices");
    cx.simulate_keystrokes("ctrl-s");

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/v1/invoices?page=2");
    cx.simulate_keystrokes("ctrl-s");

    assert_eq!(
        collection_files(&root),
        ["invoices.json"],
        "a re-save must overwrite, not accumulate"
    );

    let bytes = std::fs::read(root.join("invoices.json")).expect("read");
    let saved: RequestSpec = serde_json::from_slice(&bytes).expect("parse");
    assert_eq!(saved.url, "https://api.test/v1/invoices?page=2", "and hold the edit");

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn a_buffer_still_knows_its_file_after_a_restart(cx: &mut TestAppContext) {
    // The reason the session envelope went to v2. Without persisting the path, Ctrl+S after
    // a restart derives a fresh name and breeds `invoices-2.json` beside the original.
    let (session, root) = scratch_collection("resave-restart");

    {
        let (_, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
        cx.simulate_keystrokes("ctrl-l ctrl-a");
        cx.simulate_input("https://api.test/v1/invoices");
        cx.simulate_keystrokes("ctrl-s");
        // A send is the save point that writes the session envelope.
        let served = serve_once(OK_JSON);
        cx.simulate_keystrokes("ctrl-l ctrl-a");
        cx.simulate_input(&served);
        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();
    }

    assert_eq!(collection_files(&root), ["invoices.json"]);

    // Reopen against the same session file, exactly as a restart would.
    let (_, view, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
    let restored = cx.update(|_, cx| view.read(cx).path.clone());
    assert_eq!(
        restored,
        Some(root.join("invoices.json")),
        "the collection file must survive a restart"
    );

    cx.simulate_keystrokes("ctrl-s");
    assert_eq!(
        collection_files(&root),
        ["invoices.json"],
        "saving after a restart must not create a duplicate"
    );

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn two_different_requests_with_the_same_label_both_survive(cx: &mut TestAppContext) {
    // Same derived name, genuinely different requests. Overwriting here would lose one.
    let (session, root) = scratch_collection("collide");
    let (_, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://one.test/posts");
    cx.simulate_keystrokes("ctrl-s");

    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://two.test/posts");
    cx.simulate_keystrokes("ctrl-s");

    assert_eq!(collection_files(&root), ["posts-2.json", "posts.json"]);

    let first = std::fs::read(root.join("posts.json")).expect("read");
    let first: RequestSpec = serde_json::from_slice(&first).expect("parse");
    assert_eq!(first.url, "https://one.test/posts");

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn a_save_with_no_collection_directory_reports_instead_of_failing_silently(
    cx: &mut TestAppContext,
) {
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-s");

    let status = cx.update(|_, cx| view.read(cx).status.clone());
    assert!(
        status.is_some_and(|s| s.contains("nothing was saved")),
        "a save that cannot happen has to say so"
    );
}

#[gpui::test]
async fn a_url_that_looks_like_a_path_cannot_write_outside_the_collection(
    cx: &mut TestAppContext,
) {
    // Containment is what this asserts, end to end: whatever you type, the write lands
    // inside the root. Two independent layers hold it up — `label_for` only ever returns a
    // single path segment or a host, and `slug` then strips separators — so this test
    // deliberately cannot tell which one did the work, and passes if either does. The
    // regression test for `slug` alone lives in `core/src/collection.rs`.
    let (session, root) = scratch_collection("traversal");
    let (_, view, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://evil.test/../../../../tmp/zuno-escaped");
    cx.simulate_keystrokes("ctrl-s");

    let files = collection_files(&root);
    assert_eq!(files.len(), 1, "exactly one file, inside the root: {files:?}");
    assert!(
        !std::path::Path::new("/tmp/zuno-escaped.json").exists(),
        "a save escaped the collection root"
    );

    // The direct statement of the property, rather than inferring it from a listing: the
    // file Zuno recorded is a direct child of the root and nothing above it.
    let saved = cx
        .update(|_, cx| view.read(cx).path.clone())
        .expect("the buffer should know where it was saved");
    assert_eq!(saved.parent(), Some(root.as_path()), "wrote outside the root: {saved:?}");
    assert!(saved.starts_with(&root), "{saved:?}");

    remove_scratch(&mut cx, &session);
}

/// The picker's currently-visible rows, top to bottom.
fn picker_rows(
    window: &gpui::WindowHandle<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<String> {
    window
        .update(cx, |workspace, _, cx| workspace.picker_rows(cx))
        .expect("window")
}

fn picker_is_open(window: &gpui::WindowHandle<Workspace>, cx: &mut VisualTestContext) -> bool {
    window
        .update(cx, |workspace, _, _| workspace.picker_is_open())
        .expect("window")
}

/// Save the active buffer to `url`'s derived name, in a collection.
fn save_as(cx: &mut VisualTestContext, url: &str) {
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(url);
    cx.simulate_keystrokes("ctrl-s");
}

#[gpui::test]
async fn ctrl_p_lists_the_open_buffers(cx: &mut TestAppContext) {
    // Ctrl+P earns its keystroke from the first press, before any collection exists.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://second.test/widgets");

    cx.simulate_keystrokes("ctrl-p");
    assert!(picker_is_open(&window, &mut cx));

    let rows = picker_rows(&window, &mut cx);
    assert_eq!(rows.len(), 2, "both buffers should be listed: {rows:?}");
    assert!(rows.iter().any(|row| row.contains("widgets")), "{rows:?}");
}

#[gpui::test]
async fn escape_closes_the_picker_rather_than_cancelling_a_request(cx: &mut TestAppContext) {
    // Guards a keymap *ordering* rule, not just a binding. `binding_enabled` gives a
    // context-less binding the maximum depth, so the global `escape` -> CancelRequest ties
    // with `escape` -> PickerDismiss instead of losing to it, and the tie is broken by
    // registration order. Move the picker block above the Application block in
    // `register_keymap` and this test is what fails.
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-p");
    assert!(picker_is_open(&window, &mut cx));

    cx.simulate_keystrokes("escape");
    assert!(!picker_is_open(&window, &mut cx), "escape must dismiss");

    // And focus has to come back, or every binding silently stops working.
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://after-dismiss.test");
    let (_, active) = tabs_of(&window, &mut cx);
    assert_eq!(active.url, "https://after-dismiss.test", "focus was stranded");
}

#[gpui::test]
async fn typing_filters_and_enter_opens_the_selection(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://a.test/invoices");
    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://a.test/payments");

    cx.simulate_keystrokes("ctrl-p");
    cx.simulate_input("invo");

    let rows = picker_rows(&window, &mut cx);
    assert_eq!(rows.len(), 1, "the filter should narrow to one: {rows:?}");
    assert!(rows[0].contains("invoices"), "{rows:?}");

    cx.simulate_keystrokes("enter");
    assert!(!picker_is_open(&window, &mut cx));

    let (count, active) = tabs_of(&window, &mut cx);
    assert_eq!(count, 3, "choosing an open buffer must not open a new one");
    assert_eq!(active.url, "https://a.test/invoices");
}

#[gpui::test]
async fn enter_with_no_match_does_nothing(cx: &mut TestAppContext) {
    // A typo must not dismiss the picker you were halfway through using.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-p");
    cx.simulate_input("zzzzz");

    assert!(picker_rows(&window, &mut cx).is_empty());
    cx.simulate_keystrokes("enter");
    assert!(picker_is_open(&window, &mut cx), "should still be open");
}

#[gpui::test]
async fn a_saved_request_can_be_reopened_from_the_picker(cx: &mut TestAppContext) {
    // The hole this whole slice closes: before `scan`, Ctrl+S wrote files that nothing in
    // the app could read back.
    let (session, root) = scratch_collection("reopen");
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    save_as(&mut cx, "https://api.test/v1/invoices");
    // Close it, so the only way back is through the collection.
    cx.simulate_keystrokes("ctrl-w");
    assert_eq!(tabs_of(&window, &mut cx).0, 1);

    cx.simulate_keystrokes("ctrl-p");
    // The scan is off-thread, so the row arrives after the picker opens.
    let rows = wait_for(&mut cx, "the scanned request", |cx| {
        let rows = picker_rows(&window, cx);
        rows.iter().any(|r| r.contains("invoices")).then_some(rows)
    });
    assert!(rows.iter().any(|r| r.contains("invoices.json")), "{rows:?}");

    cx.simulate_input("invoices");
    cx.simulate_keystrokes("enter");

    let (count, active) = tabs_of(&window, &mut cx);
    assert_eq!(count, 2, "the saved request should open in a new buffer");
    assert_eq!(active.url, "https://api.test/v1/invoices");

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn reopening_a_saved_request_remembers_its_file(cx: &mut TestAppContext) {
    // Opening from a collection has to set `path`, or the next Ctrl+S derives a fresh name
    // and writes `invoices-2.json` next to the original.
    let (session, root) = scratch_collection("reopen-path");
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    save_as(&mut cx, "https://api.test/v1/invoices");
    cx.simulate_keystrokes("ctrl-w");

    cx.simulate_keystrokes("ctrl-p");
    wait_for(&mut cx, "the scanned request", |cx| {
        picker_rows(&window, cx)
            .iter()
            .any(|r| r.contains("invoices"))
            .then_some(())
    });
    cx.simulate_input("invoices");
    cx.simulate_keystrokes("enter");

    cx.simulate_keystrokes("ctrl-s");
    assert_eq!(
        collection_files(&root),
        ["invoices.json"],
        "a reopened request must save back over its own file"
    );

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn an_already_open_request_is_not_listed_twice(cx: &mut TestAppContext) {
    // It's listed as the buffer, so choosing it switches instead of opening a second copy
    // of the same file.
    let (session, root) = scratch_collection("dedup");
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    save_as(&mut cx, "https://api.test/v1/invoices");

    cx.simulate_keystrokes("ctrl-p");
    cx.run_until_parked();
    // Give the scan a chance to add a duplicate row if it were going to.
    let rows = picker_rows(&window, &mut cx);
    let matching: Vec<&String> = rows.iter().filter(|r| r.contains("invoices")).collect();
    assert_eq!(matching.len(), 1, "listed twice: {rows:?}");

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn arrow_keys_move_the_selection_and_wrap(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://a.test/second");

    cx.simulate_keystrokes("ctrl-p");
    let selected = |cx: &mut VisualTestContext| {
        window
            .update(cx, |workspace, _, cx| workspace.picker_selection(cx))
            .expect("window")
    };

    assert_eq!(selected(&mut cx), 0);
    cx.simulate_keystrokes("down");
    assert_eq!(selected(&mut cx), 1);
    // Wraps rather than dead-ending at the last row.
    cx.simulate_keystrokes("down");
    assert_eq!(selected(&mut cx), 0);
    cx.simulate_keystrokes("up");
    assert_eq!(selected(&mut cx), 1, "up from the top must wrap to the end");
}

#[gpui::test]
async fn a_second_ctrl_p_does_not_nest_a_modal(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-p");
    cx.simulate_keystrokes("ctrl-p");
    assert!(picker_is_open(&window, &mut cx));
    cx.simulate_keystrokes("escape");
    assert!(!picker_is_open(&window, &mut cx), "one escape must close it");
}

#[gpui::test]
async fn tab_does_not_move_focus_out_of_the_picker(cx: &mut TestAppContext) {
    // The panes behind a modal are still painted, so their inputs are still tab stops and
    // `focus_next` walks straight past the scrim into them.
    //
    // `picker_is_open` cannot detect this on its own — the bug leaves the picker on screen and
    // merely strands it — so the only proof is where the keystrokes afterwards land.
    let (window, view, mut cx) = boot(cx, None, None);
    let url_before = spec_of(&view, &mut cx).url;

    cx.simulate_keystrokes("ctrl-p");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("zzzz");

    assert!(picker_is_open(&window, &mut cx));
    assert!(
        picker_rows(&window, &mut cx).is_empty(),
        "typing after Tab must still reach the picker's filter"
    );
    assert_eq!(
        spec_of(&view, &mut cx).url,
        url_before,
        "Tab must not put focus in the buffer behind the modal"
    );
}

#[gpui::test]
async fn tab_does_not_strand_the_settings_panel(cx: &mut TestAppContext) {
    // The harm, stated as the user meets it: once focus leaves the panel its leaf key context
    // stops matching, so `escape` resolves to the *global* CancelRequest and the panel becomes
    // closable only with the mouse. Every binding it owns dies at once and nothing on screen
    // says why.
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-,");
    assert!(settings_open(&window, &mut cx));

    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("escape");

    assert!(
        !settings_open(&window, &mut cx),
        "Escape must still dismiss the panel after Tab"
    );
}

#[gpui::test]
async fn a_picker_cannot_open_over_the_settings_panel(cx: &mut TestAppContext) {
    // Four of the six openers guarded on both modals and these two guarded on only the picker,
    // so they stacked. Closing the picker then restored focus to the buffer *behind* the panel,
    // stranding it exactly as Tab did — one defect, two routes in.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-,");

    cx.simulate_keystrokes("ctrl-p");
    assert!(
        !picker_is_open(&window, &mut cx),
        "Ctrl+P must not stack a picker over the panel"
    );
    cx.simulate_keystrokes("ctrl-k");
    assert!(
        !picker_is_open(&window, &mut cx),
        "Ctrl+K must not stack a picker over the panel"
    );
    assert!(
        settings_open(&window, &mut cx),
        "and the panel itself must be untouched"
    );

    // Still dismissable from the keyboard, which is what stacking took away.
    cx.simulate_keystrokes("escape");
    assert!(!settings_open(&window, &mut cx));
}

#[gpui::test]
async fn ctrl_k_lists_commands_with_their_keybindings(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-k");
    assert!(picker_is_open(&window, &mut cx));

    let rows = picker_rows(&window, &mut cx);
    assert!(rows.len() > 10, "the palette should be populated: {}", rows.len());

    // Read from the live keymap, not a hardcoded string — a rebinding must not leave the
    // palette advertising a shortcut that no longer works.
    let send = rows
        .iter()
        .find(|row| row.starts_with("Send request"))
        .expect("Send request should be listed");
    assert!(send.contains("ctrl-enter"), "missing its keybinding: {send:?}");

    // Text-editing actions are keystrokes, not commands, and must never appear.
    for row in &rows {
        assert!(!row.contains("Backspace"), "{row:?}");
        assert!(!row.contains("SelectLeft"), "{row:?}");
    }
}

#[gpui::test]
async fn a_palette_command_runs_the_same_path_as_its_keybinding(cx: &mut TestAppContext) {
    // Dispatching rather than calling directly is what keeps a palette row and its
    // shortcut from drifting apart.
    let (window, view, mut cx) = boot(cx, None, None);
    let before = spec_of(&view, &mut cx).headers.len();

    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("add head");
    cx.simulate_keystrokes("enter");

    assert!(!picker_is_open(&window, &mut cx), "should close on confirm");

    let after = spec_of(&view, &mut cx).headers.len();
    assert_eq!(after, before + 1, "the command should have added a header");

    // And focus must have landed in the new row, exactly as Ctrl+Shift+H does — proof the
    // action dispatched from the request view rather than from inside the dying modal.
    cx.simulate_input("X-From-Palette");
    let added = spec_of(&view, &mut cx).headers.pop().expect("a header");
    assert_eq!(added.name, "X-From-Palette");
}

#[gpui::test]
async fn the_palette_filters_fuzzily(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-k");

    cx.simulate_input("tgtm");
    let rows = picker_rows(&window, &mut cx);
    assert!(
        rows.iter().any(|row| row.starts_with("Toggle theme")),
        "initials should find it: {rows:?}"
    );
}

#[gpui::test]
async fn escape_closes_the_palette_too(cx: &mut TestAppContext) {
    // The palette reuses the picker's key context, so this is really asserting that the
    // one set of bindings serves both — principle 2 holding up in practice.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_keystrokes("escape");
    assert!(!picker_is_open(&window, &mut cx));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://after-palette.test");
    assert_eq!(tabs_of(&window, &mut cx).1.url, "https://after-palette.test");
}

#[gpui::test]
async fn the_palette_can_open_a_tab_through_a_command(cx: &mut TestAppContext) {
    // An end-to-end check that a dispatched action reaches a handler which mutates the
    // workspace itself, not just the active buffer.
    let (window, _, mut cx) = boot(cx, None, None);
    assert_eq!(tabs_of(&window, &mut cx).0, 1);

    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("new tab");
    cx.simulate_keystrokes("enter");

    assert_eq!(tabs_of(&window, &mut cx).0, 2, "the command should open a tab");
}

#[gpui::test]
async fn choosing_a_buffer_leaves_focus_in_that_buffer(cx: &mut TestAppContext) {
    // Ordering guard. `activate` focuses synchronously, so if the picker were closed
    // *after* acting, `close_picker`'s focus restore would clobber the switch: `active_ix`
    // would point at the chosen buffer while focus sat in the previous one, and typing
    // would silently edit the wrong request.
    //
    // Note this does not apply to `Target::Action`: `Window::dispatch_action` captures the
    // focus id and then `cx.defer`s, so a dispatched command behaves the same in either
    // order. Buffers and files are the reason the order is fixed.
    let (window, first, mut cx) = boot(cx, None, None);
    let first_url = spec_of(&first, &mut cx).url;

    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_input("https://second.test/widgets");

    // Jump back to the original buffer through the picker. Filter on `graphql`, not on the
    // sample's *name* ("List repositories") — labels derive from the URL, so the row reads
    // as the last path segment of https://api.github.com/graphql.
    cx.simulate_keystrokes("ctrl-p");
    cx.simulate_input("graphql");
    cx.simulate_keystrokes("enter");

    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("https://typed-after-picking.test");

    let (_, active) = tabs_of(&window, &mut cx);
    assert_eq!(
        active.url, "https://typed-after-picking.test",
        "typing must land in the buffer that was picked"
    );
    assert_ne!(first_url, active.url, "sanity: the buffer really changed");
}

fn settings_rows(window: &gpui::WindowHandle<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    window
        .update(cx, |workspace, _, cx| workspace.settings_rows(cx))
        .expect("window")
}

fn settings_open(window: &gpui::WindowHandle<Workspace>, cx: &mut VisualTestContext) -> bool {
    window
        .update(cx, |workspace, _, _| workspace.settings_is_open())
        .expect("window")
}

/// The value shown for a settings row, by label prefix.
fn setting_value(
    window: &gpui::WindowHandle<Workspace>,
    cx: &mut VisualTestContext,
    label: &str,
) -> String {
    settings_rows(window, cx)
        .into_iter()
        .find(|row| row.starts_with(label))
        .map(|row| row.rsplit(" = ").next().unwrap_or_default().to_string())
        .unwrap_or_else(|| panic!("no settings row starting with {label:?}"))
}

#[gpui::test]
async fn ctrl_comma_shows_the_engine_settings_that_were_previously_invisible(
    cx: &mut TestAppContext,
) {
    // §11's whole point: these are honoured on every request with no way to see them.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-,");
    assert!(settings_open(&window, &mut cx));

    let rows = settings_rows(&window, &mut cx).join("\n");
    for expected in [
        "Store and replay cookies = on",
        "Verify TLS certificates = on",
        "Follow redirects = on",
        "Maximum redirect hops = 10",
        "Accept compressed responses = on",
        "Timeout = 30s",
    ] {
        assert!(rows.contains(expected), "missing {expected:?} in:\n{rows}");
    }
}

#[gpui::test]
async fn toggling_a_setting_reaches_the_spec_that_gets_sent(cx: &mut TestAppContext) {
    // A panel that edits a copy nothing reads would look identical on screen and change
    // nothing about the request, so assert against `spec(cx)` — what actually goes on the
    // wire — rather than against the panel's own state.
    let (window, view, mut cx) = boot(cx, None, None);
    assert!(spec_of(&view, &mut cx).settings.cookie_store);

    cx.simulate_keystrokes("ctrl-,");
    // Cookies is the first row, so Enter toggles it without moving.
    cx.simulate_keystrokes("enter");

    assert!(
        !spec_of(&view, &mut cx).settings.cookie_store,
        "the toggle must reach the request that gets sent"
    );
    assert_eq!(setting_value(&window, &mut cx, "Store and replay cookies"), "off");
}

#[gpui::test]
async fn an_edit_survives_dismissing_the_panel(cx: &mut TestAppContext) {
    // There is no OK/Cancel here, so Esc must not silently discard what you changed.
    let (window, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-, enter escape");
    assert!(!settings_open(&window, &mut cx));
    assert!(!spec_of(&view, &mut cx).settings.cookie_store);

    // And focus has to come back, or the keymap is dead.
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://after-settings.test");
    assert_eq!(tabs_of(&window, &mut cx).1.url, "https://after-settings.test");
}

#[gpui::test]
async fn arrows_step_the_numeric_settings_within_bounds(cx: &mut TestAppContext) {
    let (window, view, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-,");

    // Down three times: cookies -> tls -> redirects -> max hops.
    cx.simulate_keystrokes("down down down");
    cx.simulate_keystrokes("right");
    assert_eq!(setting_value(&window, &mut cx, "Maximum redirect hops"), "11");
    cx.simulate_keystrokes("left left");
    assert_eq!(setting_value(&window, &mut cx, "Maximum redirect hops"), "9");
    assert_eq!(spec_of(&view, &mut cx).settings.max_redirects, 9);

    // Timeout is two rows further down, and steps in 5s.
    cx.simulate_keystrokes("down down");
    cx.simulate_keystrokes("right");
    assert_eq!(setting_value(&window, &mut cx, "Timeout"), "35s");

    // Clamped, not wrapped or underflowed: 30 downward steps would go far below zero.
    for _ in 0..30 {
        cx.simulate_keystrokes("left");
    }
    assert_eq!(
        setting_value(&window, &mut cx, "Timeout"),
        "1s",
        "the timeout must clamp above zero rather than reaching 0 or wrapping"
    );
}

#[gpui::test]
async fn the_selection_wraps_in_both_directions(cx: &mut TestAppContext) {
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-,");

    let selection = |cx: &mut VisualTestContext| {
        window
            .update(cx, |workspace, _, cx| workspace.settings_selection(cx))
            .expect("window")
    };

    assert_eq!(selection(&mut cx), 0);
    // Up from the first row must wrap rather than underflow a usize.
    cx.simulate_keystrokes("up");
    assert_eq!(selection(&mut cx), 6);
    cx.simulate_keystrokes("down");
    assert_eq!(selection(&mut cx), 0);
}

#[gpui::test]
async fn the_status_bar_says_when_cookies_are_on(cx: &mut TestAppContext) {
    // The indicator is the half that matters: the toggle says what *will* happen, the
    // indicator says what *is* happening, and the jar being invisible is what costs an hour.
    let (window, _, mut cx) = boot(cx, None, None);
    let cookies_on = |cx: &mut VisualTestContext| {
        window
            .update(cx, |workspace, _, cx| workspace.cookies_enabled(cx))
            .expect("window")
    };

    assert!(cookies_on(&mut cx), "on by default, matching the engine");

    cx.simulate_keystrokes("ctrl-, enter escape");
    assert!(!cookies_on(&mut cx), "the indicator must follow the setting");
}

#[gpui::test]
async fn clearing_cookies_is_reachable_and_reports_back(cx: &mut TestAppContext) {
    // The engine side is covered over a real socket in core/tests/engine.rs; this asserts
    // the action is wired and confirms itself, since a silent action leaves you unsure
    // whether you pressed it.
    let (window, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-,");
    // Clear cookies is the last row.
    cx.simulate_keystrokes("up");
    cx.simulate_keystrokes("enter");

    assert_eq!(setting_value(&window, &mut cx, "Clear stored cookies now"), "cleared");
    let status = cx.update(|_, cx| view.read(cx).status.clone());
    assert!(
        status.is_some_and(|s| s.contains("Cleared stored cookies")),
        "the action should report back"
    );
    // Clearing is not a settings change, so it must not have edited the request.
    assert!(spec_of(&view, &mut cx).settings.cookie_store);
}

#[gpui::test]
async fn settings_are_per_request_not_global(cx: &mut TestAppContext) {
    // The scope decision, asserted. A global-defaults layer is the same scope-model problem
    // environments has to solve, so this deliberately edits one buffer only.
    let (window, first, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-, enter escape");
    assert!(!spec_of(&first, &mut cx).settings.cookie_store);

    cx.simulate_keystrokes("ctrl-t");
    let (_, fresh) = tabs_of(&window, &mut cx);
    assert!(
        fresh.settings.cookie_store,
        "a new buffer must not inherit another buffer's settings"
    );
}

#[gpui::test]
async fn settings_survive_a_save_and_reopen(cx: &mut TestAppContext) {
    // `RequestSettings` is part of `RequestSpec`, so it rides along in the collection file.
    // Worth asserting: it's what makes "TLS off for this one request" durable.
    let (session, root) = scratch_collection("settings-roundtrip");
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/v1/insecure");
    // Row 1 is Verify TLS.
    cx.simulate_keystrokes("ctrl-, down enter escape");
    cx.simulate_keystrokes("ctrl-s");

    let bytes = std::fs::read(root.join("insecure.json")).expect("read");
    let saved: RequestSpec = serde_json::from_slice(&bytes).expect("parse");
    assert!(!saved.settings.verify_tls, "the setting must persist");
    assert!(saved.settings.cookie_store, "and the others must be untouched");

    remove_scratch(&mut cx, &session);
    let _ = window;
}

/// Write an environment file into a collection's reserved `environments/` directory.
fn write_env(root: &PathBuf, file: &str, json: &str) {
    let dir = root.join("environments");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(file), json).expect("write");
}

fn active_environment(
    window: &gpui::WindowHandle<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    window
        .update(cx, |workspace, _, _| workspace.active_environment())
        .expect("window")
}

#[gpui::test]
async fn ctrl_e_lists_environments_with_none_always_offered(cx: &mut TestAppContext) {
    let (session, root) = scratch_collection("env-list");
    write_env(&root, "dev.json", r#"{"baseUrl":"http://localhost:3000"}"#);
    write_env(&root, "prod.json", r#"{"baseUrl":"https://api.example.com"}"#);
    // Neither of these is selectable: globals is always-on, and a sidecar belongs to its
    // environment rather than being one.
    write_env(&root, "globals.json", r#"{"version":"v2"}"#);
    write_env(&root, "dev.local.json", r#"{"token":"secret"}"#);

    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
    cx.simulate_keystrokes("ctrl-e");

    let rows = picker_rows(&window, &mut cx);
    let labels: Vec<&str> = rows
        .iter()
        .map(|row| row.split(" — ").next().unwrap_or(""))
        .collect();
    assert_eq!(labels, ["None", "dev", "prod"], "{rows:?}");

    // The secret's *value* must never be on screen; a count is what tells you it loaded.
    let dev = rows.iter().find(|row| row.starts_with("dev")).expect("dev row");
    assert!(dev.contains("secret"), "should say a secret exists: {dev:?}");
    assert!(!rows.join("\n").contains("ghp_"), "no values on screen");
    assert!(
        !rows.join("\n").contains("http://localhost:3000"),
        "not even non-secret values: {rows:?}"
    );

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn a_variable_is_substituted_on_the_way_to_a_real_socket(cx: &mut TestAppContext) {
    // The whole point, asserted against the bytes a server actually received rather than
    // against the resolver in isolation.
    let (session, root) = scratch_collection("env-send");
    let url = serve_once(OK_JSON);
    let host = url.trim_start_matches("http://").to_string();
    write_env(&root, "dev.json", &format!(r#"{{"host":"{host}"}}"#));
    write_env(&root, "dev.local.json", r#"{"token":"s3cret"}"#);

    let (window, view, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-e");
    cx.simulate_input("dev");
    cx.simulate_keystrokes("enter");
    assert_eq!(active_environment(&window, &mut cx).as_deref(), Some("dev"));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("http://{{host}}/v1/things");
    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("Authorization");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Bearer {{token}}");

    cx.simulate_keystrokes("ctrl-enter");
    wait_for(&mut cx, "the response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });

    // The stored request keeps its placeholders — otherwise saving would bake the
    // environment into the file and the whole point is lost.
    let spec = spec_of(&view, &mut cx);
    assert_eq!(spec.url, "http://{{host}}/v1/things");
    assert!(
        spec.headers.iter().any(|h| h.value == "Bearer {{token}}"),
        "the buffer must keep its placeholder: {:?}",
        spec.headers
    );

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn a_variable_in_a_form_field_reaches_the_socket_substituted(cx: &mut TestAppContext) {
    // Asserted against the bytes a server actually received, because that is the only place
    // this bug was visible: `apply` substituted raw bodies only, and `build.rs` deliberately
    // never scans a body for `{{…}}`, so an unresolved secret was sent verbatim with no error
    // anywhere. A client-credentials token request is exactly this shape.
    let (session, root) = scratch_collection("env-form-send");
    write_env(&root, "dev.json", r#"{"id":"zuno-cli"}"#);
    write_env(&root, "dev.local.json", r#"{"secret":"s3cret"}"#);

    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
    let (url, server) = serve_capturing(OK_JSON);

    cx.simulate_keystrokes("ctrl-e");
    cx.simulate_input("dev");
    cx.simulate_keystrokes("enter");

    // A fresh buffer: the sample ships `Content-Type: application/json`, and an explicit
    // header outranks the form's derived type.
    cx.simulate_keystrokes("ctrl-t");
    let view = active_view(&window, &mut cx);
    cx.simulate_input(&url);

    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("client_id");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("{{id}}");
    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("client_secret");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("{{secret}}");

    cx.simulate_keystrokes("ctrl-enter");
    wait_for(&mut cx, "the response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });

    let request = server.join().expect("server thread");
    assert!(
        request.contains("client_id=zuno-cli&client_secret=s3cret"),
        "form values should be substituted on the way out:\n{request}"
    );
    assert!(
        !request.contains("%7B%7B") && !request.contains("{{"),
        "no placeholder may reach the server, encoded or not:\n{request}"
    );

    // And the buffer keeps its placeholders, or a later Ctrl+S would write the secret into a
    // committed collection file.
    let Body::Form(fields) = &spec_of(&view, &mut cx).body else {
        panic!("expected a form body");
    };
    assert_eq!(fields[1].value, "{{secret}}");

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn an_undefined_variable_is_reported_by_name_instead_of_being_sent(cx: &mut TestAppContext) {
    // `Url::parse("https://{{baseUrl}}/x")` succeeds and reads the placeholder as a
    // hostname, so without the check this is a pointless DNS lookup and a confusing error.
    let (view, mut cx) = open_workspace(cx);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://{{baseUrl}}/users");
    cx.simulate_keystrokes("ctrl-enter");

    let error = wait_for(&mut cx, "the failure", |cx| {
        cx.update(|_, cx| view.read(cx).error.clone())
    });
    assert!(
        matches!(&error, EngineError::UnresolvedVariable { name, .. } if name == "baseUrl"),
        "should name the variable: {error:?}"
    );
    assert!(error.is_local(), "nothing should have left the machine");
}

#[gpui::test]
async fn the_selected_environment_survives_a_restart(cx: &mut TestAppContext) {
    // The reason the session envelope went to v3.
    let (session, root) = scratch_collection("env-restart");
    write_env(&root, "staging.json", r#"{"baseUrl":"https://staging.test"}"#);

    {
        let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
        cx.simulate_keystrokes("ctrl-e");
        cx.simulate_input("staging");
        cx.simulate_keystrokes("enter");
        assert_eq!(active_environment(&window, &mut cx).as_deref(), Some("staging"));
    }

    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
    assert_eq!(
        active_environment(&window, &mut cx).as_deref(),
        Some("staging"),
        "switching and closing must not forget the choice"
    );

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn selecting_none_turns_substitution_back_off(cx: &mut TestAppContext) {
    // Otherwise there'd be no way out of an environment once you'd picked one.
    let (session, root) = scratch_collection("env-none");
    write_env(&root, "dev.json", r#"{"a":"1"}"#);
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-e");
    cx.simulate_input("dev");
    cx.simulate_keystrokes("enter");
    assert_eq!(active_environment(&window, &mut cx).as_deref(), Some("dev"));

    cx.simulate_keystrokes("ctrl-e");
    cx.simulate_input("none");
    cx.simulate_keystrokes("enter");
    assert_eq!(active_environment(&window, &mut cx), None);

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn loading_an_environment_with_secrets_writes_the_gitignore_rule(cx: &mut TestAppContext) {
    // Collections exist to be committed, so the first time a secret is in play the ignore
    // rule has to already be there. Narrow on purpose: only when there's something to
    // protect, and reported rather than done silently.
    let (session, root) = scratch_collection("env-gitignore");
    write_env(&root, "dev.json", r#"{"baseUrl":"http://localhost"}"#);
    write_env(&root, "dev.local.json", r#"{"token":"s3cret"}"#);

    let (window, view, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
    assert!(!root.join(".gitignore").exists(), "nothing written before a choice is made");

    cx.simulate_keystrokes("ctrl-e");
    cx.simulate_input("dev");
    cx.simulate_keystrokes("enter");

    // At switch, not at send: it's the earliest point secrets are known to be in play, and
    // a send clears `status`, so a notice set during one would never be seen.
    let ignore = std::fs::read_to_string(root.join(".gitignore")).expect("should exist");
    assert!(ignore.contains("*.local.json"), "{ignore:?}");

    let status = cx.update(|_, cx| view.read(cx).status.clone());
    assert!(
        status.is_some_and(|s| s.contains(".gitignore")),
        "touching the repo must be reported, not silent"
    );

    let _ = window;
    remove_scratch(&mut cx, &session);
}

/// A server that answers `count` times, with a different status each time, so runs are
/// distinguishable in history.
///
/// Each response says `Connection: close`, so one request means one connection. Left
/// keep-alive, reqwest may pool the socket and reuse it, and this server — which accepts
/// once per response — waits for a connection that never arrives. See the note on
/// `serve_twice_setting_a_cookie` in `core/tests/engine.rs`: that cost a six-hour CI run.
fn serve_sequence(statuses: &'static [(u16, &'static str)]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        for (code, body) in statuses {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            let response = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

/// Send the active request and wait for a response whose status matches.
fn send_and_wait(cx: &mut VisualTestContext, view: &gpui::Entity<RequestView>, status: u16) {
    cx.simulate_keystrokes("ctrl-enter");
    wait_for(cx, "the response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
            .filter(|response| response.status == status)
    });
}

fn viewing(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext) -> usize {
    cx.update(|_, cx| view.read(cx).viewing())
}

#[gpui::test]
async fn ctrl_h_lists_every_retained_run_including_the_live_one(cx: &mut TestAppContext) {
    // Ten runs per buffer were already retained and read by nothing. This is what makes the
    // retention worth its memory.
    let (window, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{\"a\":1}"), (500, "{\"b\":2}")]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    send_and_wait(&mut cx, &view, 500);

    cx.simulate_keystrokes("ctrl-h");
    let rows = picker_rows(&window, &mut cx);
    assert_eq!(rows.len(), 2, "the live run and the one before it: {rows:?}");
    // The status is in the label because "which run was the 500?" is the question you open
    // this to answer.
    assert!(rows[0].starts_with("live · 500"), "{rows:?}");
    assert!(rows[1].starts_with("1 send ago · 200"), "{rows:?}");
    assert!(rows[0].contains("showing"), "the live row should be marked: {rows:?}");
}

#[gpui::test]
async fn choosing_an_earlier_run_shows_it_and_reindexes_its_body(cx: &mut TestAppContext) {
    // The two bodies deliberately differ in *row count*. An earlier version of this test
    // asserted `row_count() > 0`, which was true whichever body had been indexed — so it
    // passed even when the reindex was removed entirely. Row count is the cheapest
    // observable that actually distinguishes one response's outline from another's.
    let (window, view, mut cx) = boot(cx, None, None);
    const OLDER: &str = r#"{"only":1}"#;
    const NEWER: &str = r#"{"a":1,"b":2,"c":3,"d":4,"e":5}"#;
    let url = serve_sequence(&[(200, OLDER), (201, NEWER)]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    send_and_wait(&mut cx, &view, 201);

    let live_rows = wait_for(&mut cx, "the live body index", |cx| {
        cx.update(|_, cx| view.read(cx).body_view.as_ref().map(|body| body.row_count()))
    });

    cx.simulate_keystrokes("ctrl-h");
    cx.simulate_keystrokes("down enter");

    assert_eq!(viewing(&view, &mut cx), 1);
    let shown = cx
        .update(|_, cx| view.read(cx).displayed().cloned())
        .expect("a displayed response");
    assert_eq!(shown.status, 200, "the earlier run should be on screen");

    // The outline belongs to one response, so switching has to rebuild it — otherwise the
    // body pane shows the live response's rows under the older one's status line.
    let older_rows = wait_for(&mut cx, "the earlier body to be reindexed", |cx| {
        cx.update(|_, cx| view.read(cx).body_view.as_ref().map(|body| body.row_count()))
            .filter(|rows| *rows != live_rows)
    });
    assert!(
        older_rows < live_rows,
        "the smaller body should now be indexed: {older_rows} vs {live_rows}"
    );

    // The live response is untouched — history is a view, not a mutation.
    let live = cx.update(|_, cx| view.read(cx).response.clone()).expect("live");
    assert_eq!(live.status, 201);
    let _ = window;
}

#[gpui::test]
async fn sending_again_returns_the_view_to_live(cx: &mut TestAppContext) {
    // A response arriving while you read an old one must not leave you parked in the past
    // with no sign anything happened.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{}"), (201, "{}"), (202, "{}")]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    send_and_wait(&mut cx, &view, 201);

    cx.simulate_keystrokes("ctrl-h down enter");
    assert_eq!(viewing(&view, &mut cx), 1);

    send_and_wait(&mut cx, &view, 202);
    assert_eq!(viewing(&view, &mut cx), 0, "a new response returns to live");
}

#[gpui::test]
async fn history_is_capped_and_drops_the_oldest(cx: &mut TestAppContext) {
    // HISTORY_LIMIT is the memory bound; a picker listing more than that would mean the cap
    // stopped working.
    let (window, view, mut cx) = boot(cx, None, None);
    let statuses: &[(u16, &str)] = &[
        (200, "{}"), (201, "{}"), (202, "{}"), (203, "{}"), (204, "{}"), (205, "{}"),
        (206, "{}"), (207, "{}"), (208, "{}"), (209, "{}"), (210, "{}"), (211, "{}"),
    ];
    let url = serve_sequence(statuses);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    for (status, _) in statuses {
        send_and_wait(&mut cx, &view, *status);
    }

    cx.simulate_keystrokes("ctrl-h");
    let rows = picker_rows(&window, &mut cx);
    assert_eq!(
        rows.len(),
        crate::request_view::HISTORY_LIMIT + 1,
        "the live run plus at most HISTORY_LIMIT retained: {}",
        rows.len()
    );
    // The oldest is gone, not the newest.
    assert!(rows[0].contains("211"), "{rows:?}");
}

#[gpui::test]
async fn an_out_of_range_run_is_ignored_rather_than_blanking_the_pane(cx: &mut TestAppContext) {
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_once(OK_JSON);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);

    // Nothing in history yet, so offset 3 doesn't exist. Blanking the pane would leave no
    // way back to the response that is there.
    cx.update(|_, cx| view.update(cx, |view, cx| view.view_run(3, cx)));
    assert_eq!(viewing(&view, &mut cx), 0);
    assert!(
        cx.update(|_, cx| view.read(cx).displayed().is_some()),
        "the live response must still be on screen"
    );
}

#[gpui::test]
async fn history_does_not_survive_loading_a_different_request(cx: &mut TestAppContext) {
    // `load` replaces what the buffer *is*, so retained responses belong to a request that
    // is no longer there — showing them would attribute one request's responses to another.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{}"), (201, "{}")]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    send_and_wait(&mut cx, &view, 201);

    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.load(RequestSpec::sample(), cx);
        })
    });

    assert_eq!(viewing(&view, &mut cx), 0);
    assert!(
        cx.update(|_, cx| view.read(cx).runs().is_empty()),
        "a replaced buffer has no runs to show"
    );
}

#[gpui::test]
async fn clicking_a_tab_in_the_strip_activates_that_buffer(cx: &mut TestAppContext) {
    // The strip's clickable div gained a nested child, because a div carries one border colour
    // for all four sides and the active marker needed a different one from the divider between
    // tabs. `on_mouse_down` is Bubble-phase, so the ancestor still sees a click on its child —
    // but nothing tested the strip's click path before, and "the label is now a child element"
    // is exactly the change that would break it silently.
    let (window, first, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-t");
    let second = active_view(&window, &mut cx);
    assert_ne!(first.entity_id(), second.entity_id(), "ctrl-t opens a new buffer");
    cx.run_until_parked();

    let tab = cx
        .debug_bounds("tab-0")
        .expect("the strip should be painted at two buffers");
    cx.simulate_click(tab.center(), gpui::Modifiers::default());

    assert_eq!(
        active_view(&window, &mut cx).entity_id(),
        first.entity_id(),
        "clicking the first tab must activate the first buffer"
    );
}

// ---------------------------------------------------------------------------
// Response pane: body / headers tabs
// ---------------------------------------------------------------------------

fn response_view(
    view: &gpui::Entity<RequestView>,
    cx: &mut VisualTestContext,
) -> crate::request_view::ResponseView {
    cx.update(|_, cx| view.read(cx).response_view)
}

fn request_tab(
    view: &gpui::Entity<RequestView>,
    cx: &mut VisualTestContext,
) -> crate::request_view::RequestTab {
    cx.update(|_, cx| view.read(cx).request_tab)
}

#[gpui::test]
async fn alt_q_cycles_the_request_tabs_both_ways(cx: &mut TestAppContext) {
    // Three tabs, so this cannot reuse the response pane's single toggling action. Cycle order
    // is the visual order, deliberately not most-recently-used: with a fixed three-item strip,
    // MRU means the same keystroke lands somewhere different each time.
    let (_, view, mut cx) = boot(cx, None, None);
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Body, "authoring is the default");

    cx.simulate_keystrokes("alt-q");
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Headers, "forward wraps past the end");
    cx.simulate_keystrokes("alt-q");
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Query);
    cx.simulate_keystrokes("alt-q");
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Body);

    cx.simulate_keystrokes("alt-shift-q");
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Query, "and back the other way");
    cx.simulate_keystrokes("alt-shift-q");
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Headers);
}

#[gpui::test]
async fn clicking_a_request_tab_lands_on_that_tab(cx: &mut TestAppContext) {
    // The response pane gets away with one cycling action because it has exactly two tabs, so
    // "cycle from the only inactive tab" always arrives where you clicked. With three that is
    // false — clicking Body from Headers is two steps — so each tab dispatches its own action.
    // A cycling handler here would send this click to Params.
    let (_, view, mut cx) = boot(cx, None, None);
    cx.run_until_parked();

    let headers = cx
        .debug_bounds("request-tab-headers")
        .expect("the Headers tab should be painted");
    cx.simulate_click(headers.center(), gpui::Modifiers::default());
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Headers);

    // Two tabs away from Headers, so a cycle would land on Params instead.
    let body = cx.debug_bounds("request-tab-body").expect("the Body tab should be painted");
    cx.simulate_click(body.center(), gpui::Modifiers::default());
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Body);

    // The active tab is inert, so clicking where you already are is not a no-op by accident.
    cx.simulate_click(body.center(), gpui::Modifiers::default());
    assert_eq!(request_tab(&view, &mut cx), RequestTab::Body);
}

#[gpui::test]
async fn the_request_tab_is_sticky_per_buffer(cx: &mut TestAppContext) {
    // Same rule as the response pane's: two requests are open for different reasons, so the
    // section you were editing has to survive both a send and a trip through another buffer.
    let (window, first, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{}")]);

    cx.simulate_keystrokes("alt-q");
    assert_eq!(request_tab(&first, &mut cx), RequestTab::Headers);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &first, 200);
    assert_eq!(
        request_tab(&first, &mut cx),
        RequestTab::Headers,
        "a response arriving must not move you off the section you were editing"
    );

    cx.simulate_keystrokes("ctrl-t");
    let second = active_view(&window, &mut cx);
    assert_eq!(
        request_tab(&second, &mut cx),
        RequestTab::Body,
        "a new buffer gets the default, not the other buffer's choice"
    );

    cx.simulate_keystrokes("ctrl-shift-tab");
    assert_eq!(request_tab(&first, &mut cx), RequestTab::Headers);
    assert_eq!(request_tab(&second, &mut cx), RequestTab::Body);
}

#[gpui::test]
async fn the_response_pane_opens_on_the_body_and_alt_r_cycles(cx: &mut TestAppContext) {
    // The headers table was rendered inline above the body, unbounded, inside a pane that
    // clips and never scrolls — so a response with two dozen headers pushed the body off the
    // bottom edge with no way to reach it. Tabs are the fix, and Body has to be the one you
    // land on: it's the answer you sent the request for.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{}")]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);

    assert_eq!(response_view(&view, &mut cx), ResponseView::Body);

    cx.simulate_keystrokes("alt-r");
    assert_eq!(response_view(&view, &mut cx), ResponseView::Headers);

    cx.simulate_keystrokes("alt-r");
    assert_eq!(
        response_view(&view, &mut cx),
        ResponseView::Body,
        "one action for two tabs means it has to cycle back"
    );
}

#[gpui::test]
async fn clicking_the_headers_tab_switches_the_response_view(cx: &mut TestAppContext) {
    // What this proves: the tab is painted once a response lands, and clicking it is wired to
    // something. That catches a dead control, which is worth having.
    //
    // What it does **not** prove, though a first draft of this comment claimed it did: that the
    // tab dispatches `ToggleResponseView` rather than calling the view directly. Checked by
    // replacing the dispatch with a direct call — the test still passed, because with two tabs
    // and a cycling action both routes end in the same state. The body-kind chip's click test
    // *can* discriminate only because there, cycling and opening a picker have visibly
    // different outcomes. Here the convention is held by review, not by this assertion.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{}")]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    cx.run_until_parked();

    let headers_tab = cx
        .debug_bounds("response-tab-headers")
        .expect("the Headers tab should be painted once a response has landed");
    cx.simulate_click(headers_tab.center(), gpui::Modifiers::default());

    assert_eq!(response_view(&view, &mut cx), ResponseView::Headers);

    // Clicking the tab you are already on must land you there, not toggle away. This caught a
    // real bug: the action cycles, so a handler on *both* tabs made clicking "Headers" while on
    // Headers jump to Body — a control doing the opposite of its label. Only the inactive tab
    // is clickable now, which is why this holds.
    cx.simulate_click(headers_tab.center(), gpui::Modifiers::default());
    assert_eq!(
        response_view(&view, &mut cx),
        ResponseView::Headers,
        "clicking the active tab must be a no-op"
    );

    // And back, from the other tab.
    let body_tab = cx
        .debug_bounds("response-tab-body")
        .expect("the Body tab should still be painted");
    cx.simulate_click(body_tab.center(), gpui::Modifiers::default());
    assert_eq!(response_view(&view, &mut cx), ResponseView::Body);
}

#[gpui::test]
async fn the_response_view_survives_a_resend(cx: &mut TestAppContext) {
    // Deliberately *not* the history browser's "sending returns you to live" rule. Watching
    // one header change across sends is the reason to be on that tab, and snapping back to
    // the body on arrival would undo the thing you were doing.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{}"), (201, "{}")]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);

    cx.simulate_keystrokes("alt-r");
    assert_eq!(response_view(&view, &mut cx), ResponseView::Headers);

    send_and_wait(&mut cx, &view, 201);
    assert_eq!(
        response_view(&view, &mut cx),
        ResponseView::Headers,
        "a new response must not move you off the tab you chose"
    );
}

#[gpui::test]
async fn the_response_view_is_per_buffer(cx: &mut TestAppContext) {
    // The state is on `RequestView`, not `Workspace`. Two requests are open for different
    // reasons, so carrying the choice across a tab switch would be wrong — and this is the
    // assertion that fails if the field ever moves up to the workspace.
    let (window, first, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, "{}")]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &first, 200);
    cx.simulate_keystrokes("alt-r");
    assert_eq!(response_view(&first, &mut cx), ResponseView::Headers);

    cx.simulate_keystrokes("ctrl-t");
    let second = active_view(&window, &mut cx);
    assert_eq!(
        response_view(&second, &mut cx),
        ResponseView::Body,
        "a fresh buffer starts on the body"
    );

    // And switching back finds the first buffer where it was left.
    cx.simulate_keystrokes("ctrl-shift-tab");
    assert_eq!(response_view(&first, &mut cx), ResponseView::Headers);
}

// ---------------------------------------------------------------------------
// Find in response
// ---------------------------------------------------------------------------

/// Serve one JSON body, send for it, and wait until the body index exists.
///
/// Search needs the *index*, not just the response — the offset-to-row mapping reads it — so a
/// test that only waited for `response` would race the background indexing and search an
/// absent body.
fn respond_with_json(
    cx: &mut TestAppContext,
    body: &'static str,
) -> (gpui::Entity<RequestView>, VisualTestContext) {
    let (_, view, cx) = respond_with_json_in_window(cx, body);
    (view, cx)
}

/// As `respond_with_json`, keeping the window handle.
///
/// Needed wherever a test has to read whether a modal is *actually* open. `debug_bounds`
/// cannot answer that — see `menu_is_open`.
fn respond_with_json_in_window(
    cx: &mut TestAppContext,
    body: &'static str,
) -> (
    gpui::WindowHandle<Workspace>,
    gpui::Entity<RequestView>,
    VisualTestContext,
) {
    let (window, view, mut cx) = boot(cx, None, None);
    let url = serve_typed("application/json", body);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    wait_for_body(&view, &mut cx);

    (window, view, cx)
}

/// `(current, total)` from the find bar, or `None` when nothing matched.
fn find_position(
    view: &gpui::Entity<RequestView>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize)> {
    cx.update(|_, cx| view.read(cx).search.as_ref().and_then(|s| s.position()))
}

/// Replace the find bar's query and wait for the background scan to land.
///
/// The `ctrl-a` is not decoration: `simulate_input` appends, so a second call without it types
/// into whatever is already there and the probe below waits forever for a query that never
/// appears. `ctrl-a` is `text_input::SelectAll`, which reaches this input because its key
/// context is `"TextInput ResponseSearch"` — both identifiers in one string, per the leaf-only
/// predicate rule.
fn search_for(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext, query: &str) {
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input(query);
    wait_for(cx, "the search to run", |cx| {
        cx.update(|_, cx| {
            let view = view.read(cx);
            let search = view.search.as_ref()?;
            // The query having reached the input is not the same as the scan having finished;
            // the scan is a background task. Wait for the two to agree.
            (search.query.read(cx).text() == query).then_some(())
        })
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn ctrl_f_opens_the_find_bar_and_counts_matches(cx: &mut TestAppContext) {
    let (view, mut cx) = respond_with_json(cx, r#"{"a":"hit","b":"miss","c":"hit"}"#);

    assert!(
        !cx.update(|_, cx| view.read(cx).is_searching()),
        "the bar starts closed"
    );

    cx.simulate_keystrokes("ctrl-f");
    assert!(cx.update(|_, cx| view.read(cx).is_searching()));

    search_for(&view, &mut cx, "hit");
    assert_eq!(
        find_position(&view, &mut cx),
        Some((1, 2)),
        "two matches, sitting on the first"
    );
}

#[gpui::test]
async fn enter_steps_through_matches_and_wraps(cx: &mut TestAppContext) {
    let (view, mut cx) = respond_with_json(cx, r#"{"a":"x","b":"x","c":"x"}"#);

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "x");
    assert_eq!(find_position(&view, &mut cx), Some((1, 3)));

    cx.simulate_keystrokes("enter");
    assert_eq!(find_position(&view, &mut cx), Some((2, 3)));
    cx.simulate_keystrokes("enter");
    assert_eq!(find_position(&view, &mut cx), Some((3, 3)));

    // Wrapping in both directions — `rem_euclid`, not a saturating clamp.
    cx.simulate_keystrokes("enter");
    assert_eq!(find_position(&view, &mut cx), Some((1, 3)), "forward wrap");
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(find_position(&view, &mut cx), Some((3, 3)), "backward wrap");
}

#[gpui::test]
async fn the_current_match_is_the_row_the_needle_is_actually_in(cx: &mut TestAppContext) {
    // The whole point of the offset-to-row mapping, driven end to end rather than as a unit:
    // the highlighted row has to be the one holding the match.
    let (view, mut cx) = respond_with_json(cx, r#"{"alpha":1,"beta":2,"gamma":3}"#);

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "gamma");

    let row = cx
        .update(|_, cx| view.read(cx).current_match_row())
        .expect("a current match");

    let key = cx.update(|_, cx| {
        let view = view.read(cx);
        let outline = view.body_view.as_ref()?.outline()?.clone();
        let row = outline.row(row as usize)?;
        Some(outline.text(row.key).to_string())
    });

    assert_eq!(key.as_deref(), Some("\"gamma\""), "landed on row {row}");
}

#[gpui::test]
async fn jumping_to_a_match_unfolds_what_was_hiding_it(cx: &mut TestAppContext) {
    // Folding a big response and then searching into it is normal, not an edge case. Without
    // the unfold the target has no visible row at all, so the scroll goes somewhere arbitrary
    // and the search looks broken while reporting a match.
    let (view, mut cx) = respond_with_json(cx, r#"{"outer":{"inner":{"key":"buried"}},"z":1}"#);

    cx.simulate_keystrokes("alt-f");
    let folded_rows = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count());
    let total = cx.update(|_, cx| {
        view.read(cx).body_view.as_ref().unwrap().outline().unwrap().len()
    });
    assert!(folded_rows < total, "alt-f should have hidden rows");

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "buried");
    assert_eq!(find_position(&view, &mut cx), Some((1, 1)));

    // The target's row must now be present in the visible index, not merely counted.
    let row = cx.update(|_, cx| view.read(cx).current_match_row()).unwrap();
    let visible = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().visible());
    assert!(
        visible.contains(&row),
        "row {row} must be visible after jumping to it; visible = {visible:?}"
    );
}

#[gpui::test]
async fn escape_closes_the_find_bar_without_cancelling_the_request(cx: &mut TestAppContext) {
    // The keymap-ordering trap, driven through the real keymap: `escape` in the find bar's
    // context only beats the global `escape` -> CancelRequest because it is registered after
    // it. Reorder `register_keymap` and this is the test that notices.
    let (view, mut cx) = respond_with_json(cx, r#"{"a":"x"}"#);

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "x");

    cx.simulate_keystrokes("escape");
    assert!(
        !cx.update(|_, cx| view.read(cx).is_searching()),
        "escape must close the bar"
    );
    // And the response is untouched — escape reached the find bar, not the request.
    assert!(
        cx.update(|_, cx| view.read(cx).displayed().is_some()),
        "the response must still be on screen"
    );
}

#[gpui::test]
async fn reopening_the_find_bar_keeps_the_query(cx: &mut TestAppContext) {
    let (view, mut cx) = respond_with_json(cx, r#"{"a":"x","b":"x"}"#);

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "x");
    cx.simulate_keystrokes("enter");
    assert_eq!(find_position(&view, &mut cx), Some((2, 2)));

    // Ctrl+F again means "put me back in the box", not "throw away what I typed".
    cx.simulate_keystrokes("ctrl-f");
    assert_eq!(
        cx.update(|_, cx| view.read(cx).search.as_ref().unwrap().query.read(cx).text().to_string()),
        "x",
        "the query must survive a reopen"
    );

    // ...and the text is selected, so typing replaces rather than appends.
    cx.simulate_input("y");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).search.as_ref().unwrap().query.read(cx).text().to_string()),
        "y",
        "reopening selects all, so typing overwrites"
    );
}

#[gpui::test]
async fn a_resend_rescans_rather_than_leaving_a_stale_count(cx: &mut TestAppContext) {
    // Matches are byte offsets into one specific body. A resend replaces those bytes, so a
    // count carried over would describe a response that is no longer on screen — and the
    // offsets would index the new body at random.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, r#"{"a":"x","b":"x","c":"x"}"#), (201, r#"{"a":"x"}"#)]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    wait_for_body(&view, &mut cx);

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "x");
    assert_eq!(find_position(&view, &mut cx), Some((1, 3)));

    send_and_wait(&mut cx, &view, 201);
    wait_for(&mut cx, "the rescan", |cx| {
        find_position(&view, cx).filter(|(_, total)| *total == 1)
    });

    assert_eq!(
        find_position(&view, &mut cx),
        Some((1, 1)),
        "the new body has one match, not the old body's three"
    );
}

#[gpui::test]
async fn searching_a_non_json_body_uses_lines(cx: &mut TestAppContext) {
    // The raw fallback has to be searchable too, and it maps through `LineIndex` rather than
    // the outline — a different code path with the same contract.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_typed("text/plain", "alpha\nbeta needle\ngamma\nneedle again");

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    wait_for_body(&view, &mut cx);
    assert!(
        !cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().is_json()),
        "this fixture must take the raw-text path or the test proves nothing"
    );

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "needle");
    assert_eq!(find_position(&view, &mut cx), Some((1, 2)));

    // Line 1 holds the first, line 3 the second.
    assert_eq!(cx.update(|_, cx| view.read(cx).current_match_row()), Some(1));
    cx.simulate_keystrokes("enter");
    assert_eq!(cx.update(|_, cx| view.read(cx).current_match_row()), Some(3));
}

/// Serve one 200 with the given content type.
///
/// Takes the body by value rather than as part of a `&'static [(u16, &str)]` like
/// `serve_sequence`: that signature only accepts a slice the compiler can promote to `'static`,
/// which a literal built from a parameter is not. Here the body is moved into the thread, so
/// `&'static str` is all it needs.
/// `serve_typed` for a body built at runtime.
///
/// Exists because `MAX_DISPLAY_LINE` is 4096 and a `&'static str` that long cannot be written
/// inline — and a copy test that stays under the display cut cannot tell `line` from
/// `full_line` at all.
fn serve_typed_owned(content_type: &'static str, body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

fn serve_typed(content_type: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            // `Connection: close` matters — left keep-alive, reqwest pools the socket and a
            // server that accepts once can block on a connection the client meant to reuse.
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

#[gpui::test]
async fn a_query_that_matches_nothing_reports_nothing_rather_than_holding_the_last_hit(
    cx: &mut TestAppContext,
) {
    let (view, mut cx) = respond_with_json(cx, r#"{"a":"findme"}"#);

    cx.simulate_keystrokes("ctrl-f");
    search_for(&view, &mut cx, "findme");
    assert_eq!(find_position(&view, &mut cx), Some((1, 1)));

    // Keep typing until it stops matching. The count must clear, not linger on the old hit.
    search_for(&view, &mut cx, "findmezzz");
    assert_eq!(find_position(&view, &mut cx), None);
    assert_eq!(
        cx.update(|_, cx| view.read(cx).current_match_row()),
        None,
        "no match means no highlighted row"
    );
}

#[gpui::test]
async fn find_is_per_buffer_and_ctrl_f_leaves_the_headers_tab(cx: &mut TestAppContext) {
    let (view, mut cx) = respond_with_json(cx, r#"{"a":"x"}"#);

    // Searching applies to the body, so opening the bar from the Headers tab has to take you
    // where the search can be seen.
    cx.simulate_keystrokes("alt-r");
    assert_eq!(
        cx.update(|_, cx| view.read(cx).response_view),
        ResponseView::Headers
    );
    cx.simulate_keystrokes("ctrl-f");
    assert_eq!(
        cx.update(|_, cx| view.read(cx).response_view),
        ResponseView::Body,
        "Ctrl+F must move to where the matches are"
    );
}

// ---------------------------------------------------------------------------
// Copy as curl
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Mouse affordances: icons, tooltips, and the actions behind them
// ---------------------------------------------------------------------------

#[test]
fn every_icon_resolves_and_is_renderable_svg() {
    // **The silent failure this exists for.** `paint_svg` swallows a missing asset with `log_err`,
    // and `Svg` paints nothing when `text.color` is unset — so a typo'd path or a malformed file
    // is an invisible button, not a crash or a test failure. Nothing else in the suite would
    // notice, because a button with no glyph still has bounds and still dispatches.
    use crate::ui::{Assets, Icon};
    use gpui::AssetSource;

    for icon in Icon::ALL {
        let bytes = Assets
            .load(icon.path())
            .unwrap_or_else(|error| panic!("{} failed to load: {error}", icon.path()))
            .unwrap_or_else(|| panic!("{} is not in the asset source", icon.path()));

        let text = std::str::from_utf8(&bytes).expect("an svg should be utf-8");
        assert!(
            text.contains("<svg") && text.contains("viewBox"),
            "{} needs a viewBox or usvg cannot scale it",
            icon.path()
        );
        // gpui keeps only the alpha channel, so a shape with no paint at all rasterizes to a fully
        // transparent mask — which looks exactly like a missing file.
        assert!(
            text.contains("stroke=") || text.contains("fill=\"#"),
            "{} would rasterize to nothing",
            icon.path()
        );
    }

    // And a path that isn't an icon must report absence rather than pretending.
    assert!(Assets.load("icons/nope.svg").expect("no error").is_none());
}

#[test]
fn every_icon_rasterizes_to_visible_pixels() {
    // **The half of "invisible icon" that a test can actually reach.** `every_icon_resolves` proves
    // the bytes are there; it says nothing about whether they draw. gpui renders an SVG with usvg
    // and keeps only the alpha channel, so a malformed path, a missing `viewBox`, or a shape with
    // no paint all rasterize to a fully transparent mask — indistinguishable on screen from a
    // missing file, and invisible to every other test because the button still hovers and still
    // dispatches.
    //
    // Rendered here through gpui's *own* resvg version (see the dev-dependency note in
    // Cargo.toml), because a newer renderer could parse an icon that gpui's cannot.
    //
    // What this does **not** cover, and did not catch: the glyph element needing its own
    // `text_color`. That's a property of the element tree, not the file, and the test platform
    // cannot observe a paint. `ui::glyph` exists so that one is impossible by construction.
    use crate::ui::{Assets, Icon};
    use gpui::AssetSource;

    for icon in Icon::ALL {
        let bytes = Assets.load(icon.path()).expect("load").expect("present");
        let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default())
            .unwrap_or_else(|error| panic!("{} is not parseable svg: {error}", icon.path()));

        let mut pixmap = resvg::tiny_skia::Pixmap::new(32, 32).expect("pixmap");
        let scale = 32.0 / tree.size().width();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );

        let opaque = pixmap.pixels().iter().filter(|p| p.alpha() > 16).count();
        assert!(
            opaque > 20,
            "{} rasterizes to {opaque} visible pixels — it would render as an empty button",
            icon.path()
        );
        // And it must not be a solid block, which is what a stray full-canvas `fill` looks like.
        assert!(
            opaque < 32 * 32 / 2,
            "{} covers {opaque} of 1024 pixels — that isn't an icon, it's a rectangle",
            icon.path()
        );
    }
}

#[test]
fn the_asset_list_matches_the_icon_enum() {
    // `Icon::ALL` is hand-maintained and feeds the test above; if it falls behind the enum, that
    // test silently stops covering the new icon. `list` is derived from `ALL`, so comparing the two
    // catches a path added to `load` without an enum variant.
    use crate::ui::{Assets, Icon};
    use gpui::AssetSource;

    let listed = Assets.list("icons").expect("list");
    assert_eq!(
        listed.len(),
        Icon::ALL.len(),
        "Icon::ALL and the asset listing disagree"
    );
    for icon in Icon::ALL {
        assert!(
            listed.iter().any(|path| path == icon.path()),
            "{} is missing from the listing",
            icon.path()
        );
    }
}

/// Every icon/text button, paired with the action clicking it must dispatch.
///
/// The point of the table: this is the list that used to be *empty* for nine of these verbs. A
/// button added without an entry here isn't covered, and an entry without a button fails outright.
///
/// **The action column is documentation, not an assertion** — nothing headless can read which
/// action an element would dispatch, so only the selector is checked here. That the wiring is
/// real rather than a decorative glyph is spot-checked by
/// `clicking_an_icon_button_dispatches_its_action` below, at four representative buttons.
fn affordances() -> Vec<(&'static str, &'static str)> {
    vec![
        ("action-find", "zuno::FindInResponse"),
        ("action-copy-body", "zuno::CopyResponse"),
        ("action-save-body", "zuno::SaveResponse"),
        ("action-history", "zuno::ShowHistory"),
        ("action-save-request", "zuno::SaveRequest"),
        ("action-import-curl", "zuno::ImportCurl"),
        ("action-copy-curl", "zuno::CopyAsCurl"),
        ("action-settings", "zuno::OpenSettings"),
        ("action-new-tab", "zuno::NewTab"),
        ("environment-badge", "zuno::SwitchEnvironment"),
        ("hint-find", "zuno::OpenRequest"),
        ("hint-commands", "zuno::OpenPalette"),
        ("hint-env", "zuno::SwitchEnvironment"),
        ("hint-send", "zuno::SendRequest"),
        ("theme-toggle", "zuno::ToggleTheme"),
        ("fold-all", "zuno::FoldAll"),
        ("unfold-all", "zuno::UnfoldAll"),
    ]
}

/// The row menu's mouse path is a *gesture*, not a standing control, so it cannot be in the
/// table above — nothing is painted until you right-click. `right_clicking_a_row_opens_a_menu_
/// of_what_applies_to_it` covers it instead.

#[gpui::test]
async fn every_affordance_is_painted_once_a_response_has_landed(cx: &mut TestAppContext) {
    // Painted, not merely written: `debug_bounds` returning None means the element never made it
    // into a frame, which is how a button added to a branch that doesn't render looks fine in the
    // source and is absent on screen.
    let (view, mut cx) = respond_with_json(cx, r#"{"a":1}"#);
    let _ = &view;
    cx.run_until_parked();

    for (selector, _) in affordances() {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} is not painted — the verb has no mouse path"
        );
    }
}

#[gpui::test]
async fn clicking_an_icon_button_dispatches_its_action(cx: &mut TestAppContext) {
    // Three representative clicks, each with an observable effect. The full table above proves the
    // buttons *exist*; these prove the wiring behind them is real and not a decorative glyph.
    // Nested on purpose: `set_all_folded` never folds the root, so a flat object has nothing to
    // fold and the assertion at the end would hold vacuously.
    let (view, mut cx) = respond_with_json(cx, r#"{"a":{"b":1},"c":2}"#);
    cx.run_until_parked();

    // Find: opens the find bar.
    let find = cx.debug_bounds("action-find").expect("find button");
    cx.simulate_click(find.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| view.read(cx).is_searching()),
        "the find icon must open the find bar"
    );

    // Copy as curl: fills the clipboard.
    let curl = cx.debug_bounds("action-copy-curl").expect("curl button");
    cx.simulate_click(curl.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        clipboard_text(&mut cx).unwrap_or_default().starts_with("curl "),
        "the terminal icon must copy a curl command"
    );

    // Fold all: was calling the view directly instead of dispatching, so this is the regression
    // guard for that fix as much as for the button.
    let before = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count());
    let fold = cx.debug_bounds("fold-all").expect("fold-all button");
    cx.simulate_click(fold.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count()) < before,
        "clicking fold-all must fold"
    );

}

#[gpui::test]
async fn the_new_tab_button_does_not_also_drag_the_window(cx: &mut TestAppContext) {
    // It sits inside the drag-to-move titlebar, and `on_mouse_down` is Bubble-phase, so without
    // `stop_propagation` the click reaches the titlebar too. `start_window_move` is
    // `unimplemented!()` on the test platform, which is precisely what makes this observable: the
    // bug would be a panic rather than a subtle misbehaviour.
    let (window, first, mut cx) = boot(cx, None, None);
    cx.run_until_parked();

    let plus = cx.debug_bounds("action-new-tab").expect("new-tab button");
    cx.simulate_click(plus.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let second = active_view(&window, &mut cx);
    assert_ne!(
        first.entity_id(),
        second.entity_id(),
        "clicking + must open a new buffer"
    );
}

#[gpui::test]
async fn a_tooltip_names_the_keystroke_from_the_keymap(cx: &mut TestAppContext) {
    // The tooltip is what makes the mouse path *teach* the keyboard one rather than replace it, so
    // it has to read the live keymap — the same rule the migrated hint strings follow. Asserted on
    // the label builder rather than by hovering, because a rendered tooltip is not inspectable.
    let (window, _view, mut cx) = boot(cx, None, None);

    let label = window
        .update(&mut cx, |_, window, _| {
            crate::ui::Tooltip::label_for("Find in response", &crate::actions::FindInResponse, window)
        })
        .expect("window");

    assert_eq!(label, "Find in response · Ctrl+F");

    // And an unbound action must not leave a dangling separator.
    let unbound = window
        .update(&mut cx, |_, window, _| {
            crate::ui::Tooltip::label_for("Clear cookies", &crate::actions::ClearCookies, window)
        })
        .expect("window");
    assert_eq!(unbound, "Clear cookies", "no trailing separator when unbound");
}

/// The active buffer's status-bar message, or empty.
fn buffer_status(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| view.read(cx).status.clone())
        .map(|status| status.to_string())
        .unwrap_or_default()
}

#[gpui::test]
async fn ctrl_shift_x_copies_the_request_as_curl(cx: &mut TestAppContext) {
    let (_, _view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.example.com/things");
    cx.simulate_keystrokes("ctrl-shift-x");
    cx.run_until_parked();

    let command = clipboard_text(&mut cx).unwrap_or_default();
    assert!(command.starts_with("curl "), "{command}");
    assert!(command.contains("https://api.example.com/things"), "{command}");
}

#[gpui::test]
async fn copy_as_curl_resolves_variables_but_withholds_secrets(cx: &mut TestAppContext) {
    // **The security-relevant claim, and the reason this feature needed a decision at all.** A
    // copied command is pasted into issues and chat. Resolving everything would put a live token
    // there; resolving nothing would make the command useless. The committed/gitignored file split
    // already marks which is which (invariant 10), so this uses it.
    let (session, root) = scratch_collection("curl-secrets");
    write_env(&root, "dev.json", r#"{"host":"api.dev.test"}"#);
    write_env(&root, "dev.local.json", r#"{"token":"sk-live-do-not-leak"}"#);

    let (window, _view, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
    cx.simulate_keystrokes("ctrl-e");
    cx.simulate_input("dev");
    cx.simulate_keystrokes("enter");
    assert_eq!(active_environment(&window, &mut cx).as_deref(), Some("dev"));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://{{host}}/v1/things");
    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("Authorization");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Bearer {{token}}");

    cx.simulate_keystrokes("ctrl-shift-x");
    cx.run_until_parked();
    let command = clipboard_text(&mut cx).unwrap_or_default();

    assert!(
        !command.contains("sk-live-do-not-leak"),
        "a gitignored value must never reach the clipboard:\n{command}"
    );
    assert!(
        command.contains("{{token}}"),
        "and it must be visibly left as a placeholder:\n{command}"
    );
    assert!(
        command.contains("api.dev.test"),
        "the committed value must be resolved, or the command is useless:\n{command}"
    );
    assert!(
        !command.contains("{{host}}"),
        "the non-secret placeholder must not survive:\n{command}"
    );

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn copy_as_curl_says_when_it_withheld_something(cx: &mut TestAppContext) {
    // A command with `{{token}}` in it looks broken unless the app says it did that on purpose.
    let (session, root) = scratch_collection("curl-status");
    write_env(&root, "dev.json", r#"{"host":"api.dev.test"}"#);
    write_env(&root, "dev.local.json", r#"{"token":"s3cret"}"#);

    let (window, _view, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));
    cx.simulate_keystrokes("ctrl-e");
    cx.simulate_input("dev");
    cx.simulate_keystrokes("enter");

    // No secret referenced yet: the status must not claim one was held back.
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://{{host}}/a");
    cx.simulate_keystrokes("ctrl-shift-x");
    cx.run_until_parked();
    let quiet = buffer_status(&active_view(&window, &mut cx), &mut cx);
    assert!(
        !quiet.contains("token"),
        "nothing was withheld, so nothing should be announced: {quiet:?}"
    );

    // Now reference one.
    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("Authorization");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Bearer {{token}}");
    cx.simulate_keystrokes("ctrl-shift-x");
    cx.run_until_parked();

    let told = buffer_status(&active_view(&window, &mut cx), &mut cx);
    assert!(
        told.contains("{{token}}"),
        "the status must name what was left for you to fill in: {told:?}"
    );

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn copy_as_curl_exports_the_request_on_screen_including_its_body(cx: &mut TestAppContext) {
    // `spec(cx)` derives from the inputs, so this also proves the export reads what is *typed*
    // rather than some stored copy — the corollary of "derive state rather than mirroring it".
    let (_, _view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://x.test/things");
    cx.simulate_keystrokes("ctrl-m");
    cx.simulate_input("POST");
    cx.simulate_keystrokes("enter");

    clear_body(&mut cx);
    cx.simulate_input("{\"name\":\"ada\"}");

    cx.simulate_keystrokes("ctrl-shift-x");
    cx.run_until_parked();

    let command = clipboard_text(&mut cx).unwrap_or_default();
    assert!(command.contains("-X POST"), "{command}");
    assert!(
        command.contains(r#"--data-raw '{"name":"ada"}'"#),
        "the body as typed must be in the command: {command}"
    );
}

#[gpui::test]
async fn a_copied_command_imports_back_into_an_equivalent_request(cx: &mut TestAppContext) {
    // Export then import, through the real keystrokes for both. The core test round-trips the
    // string; this one round-trips through the *app*, which is where the clipboard, the derived
    // spec, and curl import all have to agree.
    let (window, _view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://x.test/v1/items");
    cx.simulate_keystrokes("ctrl-shift-h");
    cx.simulate_input("X-Trace-Id");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("abc123");

    cx.simulate_keystrokes("ctrl-shift-x");
    cx.run_until_parked();

    // Ctrl+Shift+V imports from the clipboard into a *new* buffer, so the original is untouched.
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();

    let imported = spec_of(&active_view(&window, &mut cx), &mut cx);
    // The sample buffer ships `per_page=50` as a query row, and `parse` folds a query string into
    // the URL rather than splitting it — so this is also the assertion that query rows survive the
    // round trip at all.
    assert_eq!(imported.url, "https://x.test/v1/items?per_page=50");
    assert_eq!(
        imported
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("X-Trace-Id"))
            .map(|header| header.value.as_str()),
        Some("abc123"),
        "headers must survive the trip through the clipboard: {:?}",
        imported.headers
    );

    let status = buffer_status(&active_view(&window, &mut cx), &mut cx);
    assert!(
        !status.to_lowercase().contains("ignored"),
        "an exported command must not contain flags the importer drops: {status:?}"
    );
}

/// A response with an arbitrary content type and raw body bytes.
fn serve_bytes(response: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            let _ = stream.write_all(response);
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

// ---------------------------------------------------------------------------
// Row selection in the response viewer
// ---------------------------------------------------------------------------

/// The row index the reader has selected, if any.
fn selected_row(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext) -> Option<u32> {
    cx.update(|_, cx| view.read(cx).selected_body_row())
}

/// Focus the response pane, which is what makes `up`/`down` resolve to the selection verbs.
fn focus_response(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("ctrl-shift-r");
    cx.run_until_parked();
}

#[gpui::test]
async fn a_response_row_spans_the_full_width_of_the_list(cx: &mut TestAppContext) {
    // The picker's bug, in the surface that has a million rows — and here it was latent rather
    // than merely ugly: the search highlight was ragged, and the moment a row became a click
    // target most of it would have swallowed clicks.
    //
    // `uniform_list` lays each item out as a taffy **root** with the list's width as definite
    // available space, but taffy stretches a root to fill that only for `display: block`
    // (`compute_root_layout`'s `style.is_block()` gate in taffy 0.9). A `.flex()` row takes the
    // other branch and sizes to its content.
    //
    // **Measured against the *list*, never the row.** Before the fix the row's own bounds *are*
    // the narrow box, so anything derived from them passes against the bug.
    let (view, mut cx) = respond_with_json(cx, r#"{"alpha":1,"beta":2}"#);
    cx.run_until_parked();

    let list = cx.debug_bounds("response-body").expect("the body list should be painted");
    let row = cx.debug_bounds("response-row-1").expect("the alpha row should be painted");
    // The list carries `px_2` on each side, so the row is inset by 8px twice.
    assert!(
        row.size.width >= list.size.width - gpui::px(20.),
        "a row must span the list, not its text: row {:?} inside list {:?}",
        row.size.width,
        list.size.width
    );

    // And the consequence, at a point that was dead space: far right of the list, on the row.
    assert_eq!(selected_row(&view, &mut cx), None, "nothing is selected yet");
    cx.simulate_click(
        gpui::point(list.right() - gpui::px(12.), row.center().y),
        gpui::Modifiers::default(),
    );
    cx.run_until_parked();

    assert_eq!(
        selected_row(&view, &mut cx),
        Some(1),
        "clicking the empty right-hand side of a row must select that row"
    );
}

#[gpui::test]
async fn clicking_a_row_takes_focus_so_the_keyboard_can_carry_on(cx: &mut TestAppContext) {
    // Asserted through *another binding working*, never by inspecting which handle has focus:
    // a click that highlights a row and leaves focus in the URL bar sends the next `down` to a
    // text input, so the selection the click just made refuses to move. That is the failure,
    // and it is invisible to any check of the selection alone.
    //
    // Nothing in `select_body_row_at` moves focus — the pane's `track_focus` does it, via a
    // Bubble-phase listener gpui registers in `Interactivity::paint`. This test held when an
    // explicit `window.focus` was deleted from that method, which is how the call was found to
    // be dead code. It still earns its keep: removing `track_focus`, or stopping propagation
    // anywhere between the row and the pane, breaks it — see the chevron test above.
    let (view, mut cx) = respond_with_json(cx, r#"{"alpha":1,"beta":2}"#);
    cx.run_until_parked();

    let row = cx.debug_bounds("response-row-1").expect("the alpha row");
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(selected_row(&view, &mut cx), Some(1));

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(
        selected_row(&view, &mut cx),
        Some(2),
        "after clicking a row, the arrow keys must keep working"
    );
}

#[gpui::test]
async fn clicking_a_fold_chevron_folds_and_still_leaves_the_keyboard_working(
    cx: &mut TestAppContext,
) {
    // The chevron is a clickable nested inside a clickable, which `CLAUDE.md`'s trap table says
    // needs `cx.stop_propagation()`. It deliberately does not, and this test is why.
    //
    // `track_focus` transfers focus by registering an ordinary **Bubble-phase** mouse listener
    // rather than by anything special-cased, so stopping propagation on a descendant silently
    // suppresses it too. With the call in place the fold worked, the row stayed unselected, and
    // the response pane never took focus — so the next arrow key did nothing at all, which
    // reads as a dead keystroke with nothing on screen explaining it.
    //
    // Asserted through a *later keystroke* working, never by asking which handle has focus.
    let (view, mut cx) = respond_with_json(cx, r#"{"outer":{"inner":1},"z":2}"#);
    cx.run_until_parked();

    let before = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count());
    let row = cx.debug_bounds("response-row-1").expect("the outer object's row");
    // Past the depth indent, on the 12px chevron itself.
    cx.simulate_click(
        gpui::point(row.left() + gpui::px(4.0 + 13.0 + 6.0), row.center().y),
        gpui::Modifiers::default(),
    );
    cx.run_until_parked();

    assert!(
        cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count()) < before,
        "clicking the chevron must fold the container"
    );
    assert_eq!(
        selected_row(&view, &mut cx),
        Some(1),
        "and select it — folding a container and standing on it is one intent, not two"
    );

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(
        selected_row(&view, &mut cx),
        Some(4),
        "the pane must have taken focus, and `down` must skip the folded subtree"
    );
}

#[gpui::test]
async fn the_arrow_keys_step_the_selection_and_stop_at_the_ends(cx: &mut TestAppContext) {
    // Clamped rather than wrapping: running off the end of a huge response and reappearing at
    // the top loses your place, with no scrollbar movement to explain it.
    let (view, mut cx) = respond_with_json(cx, r#"{"alpha":1}"#);
    focus_response(&mut cx);

    // Rows: 0 `{`, 1 alpha, 2 `}`. The first press lands on an end, not one step in from it.
    cx.simulate_keystrokes("down");
    assert_eq!(selected_row(&view, &mut cx), Some(0));

    cx.simulate_keystrokes("down down");
    assert_eq!(selected_row(&view, &mut cx), Some(2));

    cx.simulate_keystrokes("down");
    assert_eq!(selected_row(&view, &mut cx), Some(2), "the last row is the floor");

    cx.simulate_keystrokes("up up up up");
    assert_eq!(selected_row(&view, &mut cx), Some(0), "and the first is the ceiling");
}

#[gpui::test]
async fn arrow_keys_in_the_url_bar_are_still_the_url_bars(cx: &mut TestAppContext) {
    // `up`/`down` are scoped to `ResponsePane`, and GPUI matches only the *leaf* context. Scope
    // them wrongly — or globally — and typing in the URL bar starts moving a selection in the
    // response instead. Driven through the real keymap, which is the only way to see it.
    let (view, mut cx) = respond_with_json(cx, r#"{"alpha":1}"#);

    cx.simulate_keystrokes("ctrl-l");
    cx.simulate_keystrokes("down down");
    cx.run_until_parked();

    assert_eq!(
        selected_row(&view, &mut cx),
        None,
        "the response selection must not move while the URL bar has focus"
    );
}

#[gpui::test]
async fn copying_a_row_value_decodes_the_string(cx: &mut TestAppContext) {
    // Decoded, not merely unquoted. `\n` pasted as a backslash and an `n` is the tempting
    // version and the wrong one: it *looks* decoded, so nothing tells you it isn't.
    let (view, mut cx) = respond_with_json(cx, r#"{"note":"first\nsecond","n":42}"#);
    focus_response(&mut cx);

    cx.simulate_keystrokes("down down");
    assert_eq!(selected_row(&view, &mut cx), Some(1), "the note row");

    cx.simulate_keystrokes("ctrl-c");
    cx.run_until_parked();
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("first\nsecond"));

    // A number keeps its own text — `unquote` passes anything unquoted straight through, which
    // is why there is no match on ScalarKind at the call site.
    cx.simulate_keystrokes("down ctrl-c");
    cx.run_until_parked();
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("42"));
}

#[gpui::test]
async fn copying_a_container_row_gives_the_whole_subtree(cx: &mut TestAppContext) {
    // A `{` is a row you can land on, so copying it has to mean something. Without the braces
    // this verb would be dead on every open and close row — roughly half a nested document.
    let (view, mut cx) = respond_with_json(cx, r#"{"outer":{"inner":1},"z":2}"#);
    focus_response(&mut cx);

    cx.simulate_keystrokes("down down");
    assert_eq!(selected_row(&view, &mut cx), Some(1), "the outer object's row");

    cx.simulate_keystrokes("ctrl-c");
    cx.run_until_parked();
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some(r#"{"inner":1}"#));
}

#[gpui::test]
async fn copying_a_row_path_gives_a_jsonpath(cx: &mut TestAppContext) {
    let (view, mut cx) = respond_with_json(cx, r#"{"users":[{"email":"a@b.test"}]}"#);
    focus_response(&mut cx);

    // 0 `{`, 1 `users` array, 2 the element `{`, 3 email.
    cx.simulate_keystrokes("down down down down");
    assert_eq!(selected_row(&view, &mut cx), Some(3), "the email row");

    cx.simulate_keystrokes("alt-c");
    cx.run_until_parked();
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("$.users[0].email"));
}

#[gpui::test]
async fn a_copy_with_nothing_selected_explains_itself(cx: &mut TestAppContext) {
    // Silence here reads as a broken keystroke. The palette can dispatch these with focus
    // anywhere, so "nothing is selected" is a normal state, not a misuse.
    let (view, mut cx) = respond_with_json(cx, r#"{"a":1}"#);
    cx.run_until_parked();
    assert_eq!(selected_row(&view, &mut cx), None);

    // Dispatched rather than typed: `ctrl-c` is scoped to the response pane, and the point
    // here is the path a palette row takes, which resolves with focus anywhere.
    cx.update(|window, cx| {
        window.dispatch_action(gpui::Action::boxed_clone(&crate::actions::CopyRowValue), cx)
    });
    cx.run_until_parked();

    let status = buffer_status(&view, &mut cx);
    assert!(
        status.contains("Select a row"),
        "a copy with no selection must say so, not do nothing: {status:?}"
    );
    assert_eq!(clipboard_text(&mut cx), None, "and must not put anything on the clipboard");
}

#[gpui::test]
async fn folding_a_container_keeps_the_selection_on_something_drawn(cx: &mut TestAppContext) {
    // Folding removes the selected row from `visible`. A selection nothing paints is a cursor
    // the reader has lost: the next `down` jumps from wherever it secretly still was.
    let (view, mut cx) = respond_with_json(cx, r#"{"outer":{"inner":1},"z":2}"#);
    focus_response(&mut cx);

    // Land on `inner`, which lives inside `outer`.
    cx.simulate_keystrokes("down down down");
    assert_eq!(selected_row(&view, &mut cx), Some(2), "the inner row");

    cx.simulate_keystrokes("alt-f");
    cx.run_until_parked();

    let selected = selected_row(&view, &mut cx).expect("still a selection");
    let visible = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().visible());
    assert!(
        visible.contains(&selected),
        "the selection must land on a drawn row; selected {selected}, visible {visible:?}"
    );
    assert_eq!(selected, 1, "specifically the container that swallowed it");
}

/// Right-click a point, which `simulate_click` cannot do — it hardcodes the left button.
fn right_click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    cx.simulate_mouse_down(at, gpui::MouseButton::Right, gpui::Modifiers::default());
    cx.run_until_parked();
}

/// Double-click a point. `simulate_click` hardcodes `click_count: 1`, and the count is the
/// entire signal the fold gesture reads.
fn double_click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    cx.simulate_event(gpui::MouseDownEvent {
        position: at,
        modifiers: gpui::Modifiers::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.run_until_parked();
}

/// Whether the row menu is open, read from the workspace rather than from the painted frame.
///
/// **`debug_bounds` cannot answer this.** It reads `window.rendered_frame`, so an element that
/// has been removed keeps its entry until another frame is drawn — `is_some()` is trustworthy,
/// `is_none()` is not. Four tests here asserted a menu had closed by its bounds vanishing and
/// failed against code that was closing it correctly, with the chosen action demonstrably run.
fn menu_is_open(window: &gpui::WindowHandle<Workspace>, cx: &mut VisualTestContext) -> bool {
    window
        .update(cx, |workspace, _, _| workspace.menu_open())
        .expect("window")
}

/// How many rows the open menu is offering.
///
/// Counted by probing selectors rather than reading the entity, because the count is the
/// *adaptation* being asserted — the menu drops items that cannot apply, and a check against
/// its own item vector would pass even if none of them were painted.
fn menu_row_count(cx: &mut VisualTestContext) -> usize {
    // Literals, because `debug_bounds` takes `&'static str` — a formatted selector cannot
    // outlive the call. Six is well past any menu this app builds.
    const ROWS: [&str; 6] = [
        "menu-row-0",
        "menu-row-1",
        "menu-row-2",
        "menu-row-3",
        "menu-row-4",
        "menu-row-5",
    ];

    let mut count = 0;
    while count < ROWS.len() && cx.debug_bounds(ROWS[count]).is_some() {
        count += 1;
    }
    count
}

#[gpui::test]
async fn right_clicking_a_row_opens_a_menu_of_what_applies_to_it(cx: &mut TestAppContext) {
    // The menu is the mouse path for verbs that previously had a toolbar label visible only
    // once a row was selected — which meant it was findable only by someone who already knew
    // the keyboard path. A right-click is a blind reflex, so it has to answer.
    let (window, view, mut cx) = respond_with_json_in_window(cx, r#"{"outer":{"inner":1},"z":2}"#);
    cx.run_until_parked();

    assert!(!menu_is_open(&window, &mut cx), "closed to begin with");

    let row = cx.debug_bounds("response-row-2").expect("the inner row");
    right_click(&mut cx, row.center());

    assert!(cx.debug_bounds("context-menu").is_some(), "right-click must open the menu");
    assert_eq!(
        selected_row(&view, &mut cx),
        Some(2),
        "and select the row first, or \"Copy value\" is ambiguous about which row it means"
    );
}

#[gpui::test]
async fn the_menu_offers_fold_only_on_a_container(cx: &mut TestAppContext) {
    // Adapt, don't disable. A greyed row that can never apply is noise in a menu this short,
    // and it is the same rule the pane already followed for the raw view's missing path.
    let (view, mut cx) = respond_with_json(cx, r#"{"outer":{"inner":1},"z":2}"#);
    cx.run_until_parked();

    // A scalar: two items, no fold.
    let scalar = cx.debug_bounds("response-row-2").expect("the inner row");
    right_click(&mut cx, scalar.center());
    assert_eq!(menu_row_count(&mut cx), 2, "value and path, no fold on a scalar");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // A container: three.
    let container = cx.debug_bounds("response-row-1").expect("the outer object's row");
    right_click(&mut cx, container.center());
    assert!(view.read_with(&cx, |view, _| view.selected_is_container()));
    assert_eq!(menu_row_count(&mut cx), 3, "value, path, and fold on a container");
}

#[gpui::test]
async fn the_menu_drops_copy_path_on_a_raw_body(cx: &mut TestAppContext) {
    // No structure to name a position within, so the item is absent rather than inert.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_typed("text/plain", "alpha\nbeta");

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    wait_for_body(&view, &mut cx);
    cx.run_until_parked();

    let row = cx.debug_bounds("response-row-0").expect("the alpha line");
    right_click(&mut cx, row.center());
    assert_eq!(menu_row_count(&mut cx), 1, "copy value only");
}

#[gpui::test]
async fn choosing_a_menu_row_runs_it_and_closes(cx: &mut TestAppContext) {
    // The whole point of the gesture: right-click, click, done — no keyboard, no toolbar.
    let (window, view, mut cx) = respond_with_json_in_window(cx, r#"{"note":"hello","n":2}"#);
    cx.run_until_parked();

    let row = cx.debug_bounds("response-row-1").expect("the note row");
    right_click(&mut cx, row.center());

    let item = cx.debug_bounds("menu-row-0").expect("the first menu row");
    cx.simulate_click(item.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("hello"));
    assert!(!menu_is_open(&window, &mut cx), "choosing must close the menu");

    // And the consequence closing has to deliver: focus is back in the response pane, so the
    // keyboard carries on. Asserting the menu is gone without this would miss a close that
    // strands focus on the dropped entity and kills every binding.
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(
        selected_row(&view, &mut cx),
        Some(2),
        "focus must return to the pane, not stay on the dismissed menu"
    );
}

#[gpui::test]
async fn the_menu_is_keyboard_navigable_and_escape_does_not_cancel_the_request(
    cx: &mut TestAppContext,
) {
    // Two things at once. A menu you can only click is a mouse-only feature in a keyboard-first
    // app. And `escape` here must beat the global `escape` -> CancelRequest, which it does only
    // because a context-less binding *ties* at maximum depth and the later registration wins —
    // the fourth time that ordering has decided behaviour with no compile error to catch it.
    let (window, view, mut cx) = respond_with_json_in_window(cx, r#"{"outer":{"inner":1},"z":2}"#);
    cx.run_until_parked();

    let row = cx.debug_bounds("response-row-1").expect("the outer object's row");
    right_click(&mut cx, row.center());
    assert_eq!(menu_row_count(&mut cx), 3);

    // Third row is Fold. Arrow down twice and confirm.
    cx.simulate_keystrokes("down down enter");
    cx.run_until_parked();

    assert!(!menu_is_open(&window, &mut cx), "confirming closes the menu");
    assert!(
        view.read_with(&cx, |view, _| view.selected_is_folded()),
        "the third row must be Fold, reached by the keyboard alone"
    );

    // And escape dismisses without cancelling the request — the ordering half.
    right_click(&mut cx, row.center());
    assert!(menu_is_open(&window, &mut cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!menu_is_open(&window, &mut cx), "escape must dismiss the menu");
}

#[gpui::test]
async fn a_menu_cannot_stack_over_another_modal(cx: &mut TestAppContext) {
    // `modal_open` is consulted by every opener because the checks had drifted once already —
    // four openers checked both modals and two checked only the picker. A menu opening behind
    // a picker would leave focus restoring to the wrong place on dismiss.
    let (window, _view, mut cx) = respond_with_json_in_window(cx, r#"{"a":1}"#);
    cx.run_until_parked();

    let row = cx.debug_bounds("response-row-1").expect("a row");
    cx.simulate_keystrokes("ctrl-k");
    cx.run_until_parked();

    right_click(&mut cx, row.center());
    assert!(
        !menu_is_open(&window, &mut cx),
        "the palette is open, so the menu must refuse"
    );
}

#[gpui::test]
async fn double_clicking_a_container_folds_it(cx: &mut TestAppContext) {
    // The file-tree convention, and it stops the 12px chevron being the only fold target.
    // Dispatched rather than called directly, because three surfaces now reach this one verb.
    let (view, mut cx) = respond_with_json(cx, r#"{"outer":{"inner":1},"z":2}"#);
    cx.run_until_parked();

    let before = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count());
    let row = cx.debug_bounds("response-row-1").expect("the outer object's row");
    double_click(&mut cx, row.center());

    assert!(
        cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count()) < before,
        "double-clicking a container must fold it"
    );

    // A scalar has nothing to fold, and must not become a dead-feeling gesture that changes
    // the row count by accident.
    let steady = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count());
    // Visible index 2, not row index 4: the selector counts *drawn* rows, and folding `outer`
    // above just renumbered everything below it.
    let scalar = cx.debug_bounds("response-row-2").expect("the z row");
    double_click(&mut cx, scalar.center());
    assert_eq!(
        cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().row_count()),
        steady,
        "double-clicking a scalar must do nothing"
    );
}

// ---------------------------------------------------------------------------
// Horizontal scrolling
// ---------------------------------------------------------------------------

/// How far the response body is scrolled sideways, in pixels. Negative as you scroll right,
/// which is gpui's convention.
fn body_h_offset(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext) -> f32 {
    cx.update(|_, cx| {
        f32::from(
            view.read(cx)
                .body_scroll
                .0
                .borrow()
                .base_handle
                .offset()
                .x,
        )
    })
}

/// How much of the body is hidden to the right. Zero means it fits.
fn body_h_hidden(view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext) -> f32 {
    cx.update(|_, cx| {
        f32::from(
            view.read(cx)
                .body_scroll
                .0
                .borrow()
                .base_handle
                .max_offset()
                .width,
        )
    })
}

/// Serve a response carrying one very long header value.
fn respond_with_header(
    cx: &mut TestAppContext,
    value: &str,
) -> (gpui::Entity<RequestView>, VisualTestContext) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let value = value.to_string();

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Trace: {value}\r\n\
                 Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let (_, view, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&format!("http://{addr}"));
    send_and_wait(&mut cx, &view, 200);
    wait_for_body(&view, &mut cx);
    cx.run_until_parked();

    (view, cx)
}

/// Serve one body of the given type and wait for the index, keeping the view.
fn respond_with(
    cx: &mut TestAppContext,
    content_type: &'static str,
    body: String,
) -> (gpui::Entity<RequestView>, VisualTestContext) {
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_typed_owned(content_type, body);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    wait_for_body(&view, &mut cx);
    cx.run_until_parked();

    (view, cx)
}

#[gpui::test]
async fn the_scroll_region_is_sized_from_the_widest_row_not_the_first(cx: &mut TestAppContext) {
    // The whole slice turns on this. `uniform_list` sizes its horizontal content from **one**
    // sampled row — `with_width_from_item`, default index 0 — and row 0 of a JSON document is
    // `{`, the narrowest row there is. Take the default and `Unconstrained` is switched on with
    // nothing to scroll to, which looks exactly like horizontal scrolling not working.
    let wide = format!(r#"{{"k":"{}"}}"#, "x".repeat(400));
    let (view, mut cx) = respond_with(cx, "application/json", wide);

    let widest = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().widest_visible_ix());
    assert_ne!(widest, 0, "row 0 is the opening brace, never the widest");

    assert!(
        body_h_hidden(&view, &mut cx) > 0.,
        "a 400-character value must leave something hidden to the right"
    );
}

#[gpui::test]
async fn a_body_that_fits_hides_the_scroll_indicator(cx: &mut TestAppContext) {
    // The bar reports "there is more to the right". Drawing it permanently would report that
    // when it isn't true, which is worse than not drawing it at all.
    let (view, mut cx) = respond_with(cx, "application/json", r#"{"a":1}"#.to_string());

    assert_eq!(body_h_hidden(&view, &mut cx), 0., "this fits");
    assert!(
        cx.debug_bounds("h-scroll").is_none(),
        "nothing is hidden, so nothing should be indicated"
    );
}

#[gpui::test]
async fn a_wide_body_shows_the_scroll_indicator(cx: &mut TestAppContext) {
    // Painted on the *first* frame, which is the reason it is a `UniformListDecoration` rather
    // than a sibling div: `max_offset` and `bounds` are written during `interactivity.prepaint`,
    // after the surrounding tree is built, so a sibling reading the handle draws nothing until
    // some unrelated repaint happens along. This assertion fails against that version.
    let wide = format!(r#"{{"k":"{}"}}"#, "x".repeat(400));
    let (_view, mut cx) = respond_with(cx, "application/json", wide);

    assert!(
        cx.debug_bounds("h-scroll").is_some(),
        "a body wider than the pane must say so without waiting for another frame"
    );
}

#[gpui::test]
async fn the_arrow_keys_scroll_the_body_sideways_and_clamp(cx: &mut TestAppContext) {
    let wide = format!(r#"{{"k":"{}"}}"#, "x".repeat(400));
    let (view, mut cx) = respond_with(cx, "application/json", wide);
    focus_response(&mut cx);

    assert_eq!(body_h_offset(&view, &mut cx), 0.);

    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    let stepped = body_h_offset(&view, &mut cx);
    assert!(stepped < 0., "right must scroll right; offsets run negative");

    // Clamped at both ends — by gpui, in `interactivity.prepaint`, not by us. Asserted anyway,
    // because it is the behaviour the pane depends on: without it the view slides into blank
    // space and there is nothing on screen to say which way back is.
    cx.simulate_keystrokes("left left left");
    cx.run_until_parked();
    assert_eq!(body_h_offset(&view, &mut cx), 0., "left stops at column zero");

    for _ in 0..80 {
        cx.simulate_keystrokes("right");
    }
    cx.run_until_parked();
    let hidden = body_h_hidden(&view, &mut cx);
    assert!(
        (body_h_offset(&view, &mut cx) + hidden).abs() < 1.,
        "right stops at the end of the content"
    );

    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    assert_eq!(body_h_offset(&view, &mut cx), 0., "home returns to column zero");
}

#[gpui::test]
async fn arrow_keys_in_the_url_bar_still_move_its_caret(cx: &mut TestAppContext) {
    // `left`/`right` are scoped to `ResponsePane`. **This test does not prove that scoping**,
    // and saying so is the point — it was written believing it did.
    //
    // Making the bindings global leaves it passing, in either registration position, because
    // `text_input::Left` already exists under `TextInput` and wins regardless. Compare
    // `arrow_keys_in_the_url_bar_are_still_the_url_bars` for the selection verbs, which *does*
    // fail when globalised: nothing binds `up`/`down` in a plain `TextInput`, so there a global
    // binding really would steal the key. The difference is whether the input has a competing
    // binding at all, which is not something the scoping itself can be credited for.
    //
    // What it does pin is worth keeping: that the URL bar's caret keys still work, asserted
    // where the *character lands* rather than by watching the body sit still.
    let wide = format!(r#"{{"k":"{}"}}"#, "x".repeat(400));
    let (view, mut cx) = respond_with(cx, "application/json", wide);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("abc");
    cx.simulate_keystrokes("left");
    cx.simulate_input("X");
    cx.run_until_parked();

    assert_eq!(
        spec_of(&view, &mut cx).url,
        "abXc",
        "left must move the URL bar's caret, not scroll the response"
    );
    assert_eq!(
        body_h_offset(&view, &mut cx),
        0.,
        "and the body must not have moved"
    );
}

#[gpui::test]
async fn a_raw_body_scrolls_from_its_widest_line(cx: &mut TestAppContext) {
    // The raw view has the same problem and a different index: `LineIndex::widest_line`, which
    // measures as *drawn* so a minified megabyte doesn't size the region to a megabyte.
    let long = "y".repeat(400);
    let (view, mut cx) = respond_with(cx, "text/plain", format!("short\n{long}\nalso short"));

    let widest = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().widest_visible_ix());
    assert_eq!(widest, 1, "the long line, not the first one");
    assert!(body_h_hidden(&view, &mut cx) > 0.);
}

/// Send a scroll-wheel gesture at a point.
fn wheel(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>, dx: f32, dy: f32) {
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: at,
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(dx), gpui::px(dy))),
        modifiers: gpui::Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn the_scroll_bar_sits_at_the_bottom_and_stays_put_while_scrolling(
    cx: &mut TestAppContext,
) {
    // Two bugs in one assertion, both of which shipped.
    //
    // The bar was drawn along the **top** edge: a decoration is laid out as a root at the
    // list's origin, so a 3px-tall element with `bottom_0` puts the bar at the bottom of its own
    // 3px box, which is the top of the list.
    //
    // And the whole bar **slid left as you scrolled right**, by exactly the scroll offset,
    // because a decoration is a child of the list and gpui translates it with the content. A
    // scrollbar that leaves the viewport is worse than none.
    // Wide *and* tall: the vertical assertion at the end needs somewhere to scroll down to.
    let rows: Vec<String> = (0..200).map(|i| format!(r#""k{i}":"{}""#, "x".repeat(400))).collect();
    let wide = format!("{{{}}}", rows.join(","));
    let (_view, mut cx) = respond_with(cx, "application/json", wide);

    let list = cx.debug_bounds("response-body").expect("the body list");
    let bar = cx.debug_bounds("h-scroll").expect("the scroll bar");
    assert!(
        (bar.bottom() - list.bottom()).abs() < gpui::px(1.),
        "the bar belongs on the bottom edge: bar {:?} vs list {:?}",
        bar.bottom(),
        list.bottom()
    );

    let thumb_before = cx.debug_bounds("h-scroll-thumb").expect("the thumb").left();
    focus_response(&mut cx);
    for _ in 0..6 {
        cx.simulate_keystrokes("right");
    }
    cx.run_until_parked();

    let bar_after = cx.debug_bounds("h-scroll").expect("the scroll bar");
    let thumb_after = cx.debug_bounds("h-scroll-thumb").expect("the thumb").left();

    assert!(
        (bar_after.left() - bar.left()).abs() < gpui::px(1.),
        "the bar must stay pinned to the viewport, not scroll with the rows"
    );
    assert!(
        thumb_after > thumb_before,
        "the thumb must move right as the body scrolls right: {thumb_before:?} -> {thumb_after:?}"
    );

    // **And it must not drift up as the body scrolls down.** The translation a decoration
    // inherits is two-dimensional; only the x half was cancelled the first time, so the bar
    // climbed into the middle of the response as soon as the body was tall enough to scroll.
    // A wheel, not `down`: `down` moves the row *selection*, and `scroll_to_item` only scrolls
    // once the selection leaves the viewport — fifty rows fit, so thirty presses scrolled
    // nothing at all and the assertion below was vacuous.
    wheel(&mut cx, list.center(), 0., -600.);
    let bar_scrolled = cx.debug_bounds("h-scroll").expect("the scroll bar");
    assert!(
        (bar_scrolled.bottom() - list.bottom()).abs() < gpui::px(1.),
        "the bar must stay on the bottom edge while the body scrolls vertically: {:?} vs {:?}",
        bar_scrolled.bottom(),
        list.bottom()
    );
}

#[gpui::test]
async fn a_long_header_value_wraps_instead_of_being_cut_off(cx: &mut TestAppContext) {
    // Three assertions for three separate ways this has been wrong.
    //
    // It first shipped with the value `truncate`d, so a JWT or a CSP header was unreadable past
    // the pane's edge. Then it shipped scrolling sideways, which broke two other things: a short
    // table no longer filled the pane, and scrolling carried the *name* column off the left edge
    // so you could not tell which header you were reading. Wrapping is the answer §6 named for
    // this tab all along — it is not virtualized, so a variable row height costs nothing.
    let (view, mut cx) = respond_with_header(cx, &"e".repeat(400));

    cx.simulate_keystrokes("alt-r");
    cx.run_until_parked();
    assert_eq!(response_view(&view, &mut cx), ResponseView::Headers);

    // `header-row` resolves to the last row painted, and `collect_headers` sorts by name, so
    // `x-trace` is the one measured.
    let row = cx.debug_bounds("header-row").expect("a header row");
    let container = cx.debug_bounds("response-headers").expect("the headers pane");

    assert!(
        (row.size.width - container.size.width).abs() < gpui::px(2.),
        "a row must span the pane. Sizing the table to its content instead left short tables \
         floating in a narrow column: row {:?} in {:?}",
        row.size.width,
        container.size.width
    );
    assert!(
        row.size.height > gpui::px(40.),
        "400 characters must wrap onto several lines rather than being clipped to one: {:?}",
        row.size.height
    );

    let hidden = cx.update(|_, cx| f32::from(view.read(cx).headers_scroll.max_offset().width));
    assert_eq!(
        hidden, 0.,
        "and nothing may overflow sideways, or the name column scrolls out of view"
    );
}

#[gpui::test]
async fn a_hand_scroll_in_the_editor_survives_the_next_paint(cx: &mut TestAppContext) {
    // **The bug that made the editor feel broken.** The horizontal clamp bounded `h_offset` by
    // the width of the *cursor's* line. Park the caret on a short line — the start of the body,
    // say — and the maximum is zero, so any scroll was clamped straight back to column zero on
    // the very next frame: the view snapped home the instant you let go.
    //
    // Asserted after `run_until_parked`, which is what makes it a *next paint* assertion rather
    // than a read of the handler's own write.
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-b");
    cx.simulate_input(&"x".repeat(400));
    cx.simulate_keystrokes("ctrl-home");
    cx.run_until_parked();

    let editor = cx.debug_bounds("body-editor").expect("the body editor");
    wheel(&mut cx, editor.center(), -200., 0.);

    let offset = cx.update(|_, cx| f32::from(view.read(cx).body_editor.read(cx).h_offset()));
    assert!(
        offset > 0.,
        "a hand scroll must still be there on the next paint, not snap back to zero"
    );
}

#[gpui::test]
async fn a_sideways_swipe_in_the_editor_does_not_drift_vertically(cx: &mut TestAppContext) {
    // The handler shares the wheel with the container, which owns vertical scrolling. Without
    // consuming a predominantly horizontal gesture, the small vertical component every trackpad
    // swipe carries also scrolls the document — so sliding sideways wanders up and down.
    let (_, view, mut cx) = boot(cx, None, None);

    // Loaded from a spec rather than typed: 300 lines through `simulate_input` is 36,000
    // keystrokes and forty seconds, and the document has to be tall enough to *have* somewhere
    // vertical to drift to or the assertion is vacuous.
    let body = (0..300)
        .map(|_| "x".repeat(120))
        .collect::<Vec<_>>()
        .join("\n");
    let mut spec = RequestSpec::sample();
    spec.body = Body::Raw {
        text: body,
        kind: RawKind::Text,
    };
    cx.update(|_, cx| view.update(cx, |view, cx| view.load(spec, cx)));

    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();

    // Scroll down first, so there is room to drift in *either* direction.
    let editor = cx.debug_bounds("body-editor").expect("the body editor");
    wheel(&mut cx, editor.center(), 0., -400.);
    let before = cx.update(|_, cx| {
        f32::from(view.read(cx).body_editor.read(cx).vertical_offset())
    });
    assert_ne!(before, 0., "the document must actually be scrollable vertically");

    // Mostly sideways, with the slight vertical wobble a real trackpad produces.
    wheel(&mut cx, editor.center(), -200., -6.);

    let after = cx.update(|_, cx| {
        f32::from(view.read(cx).body_editor.read(cx).vertical_offset())
    });
    assert_eq!(
        before, after,
        "a sideways swipe must not scroll the document up or down"
    );
}

#[gpui::test]
async fn folding_shrinks_the_scroll_region_and_pulls_the_view_back(cx: &mut TestAppContext) {
    // Folding is *how* a wide response is made readable, so a horizontal extent that ignores it
    // defeats the point. The extent was computed once at index time over every row in the
    // document and never revisited, so collapsing everything left the region as wide as the
    // longest row it used to show — and the view sat scrolled into blank space with nothing out
    // there to find.
    //
    // Asserted at both consequences: the region shrinks, *and* the offset comes back with it.
    let long = "x".repeat(400);
    let body = format!(r#"{{"outer":{{"buried":"{long}"}},"z":1}}"#);
    let (view, mut cx) = respond_with(cx, "application/json", body);
    focus_response(&mut cx);

    let wide = body_h_hidden(&view, &mut cx);
    assert!(wide > 0., "the long value must overflow to begin with");

    for _ in 0..40 {
        cx.simulate_keystrokes("right");
    }
    cx.run_until_parked();
    assert!(body_h_offset(&view, &mut cx) < 0., "and we must be scrolled into it");

    // Fold everything. The long value is now inside a collapsed container and off screen.
    cx.simulate_keystrokes("alt-f");
    cx.run_until_parked();

    assert!(
        body_h_hidden(&view, &mut cx) < wide,
        "the region must shrink to what is still drawn: {} vs {wide}",
        body_h_hidden(&view, &mut cx)
    );
    assert_eq!(
        body_h_offset(&view, &mut cx),
        0.,
        "and the view must come back, not stay parked past the end of the content"
    );
}

#[gpui::test]
async fn the_editors_colouring_follows_the_body_kind(cx: &mut TestAppContext) {
    // Colour is the one thing a headless test cannot see, so this asserts the *decision* instead:
    // that the editor is told to lex JSON exactly when the body is JSON. The lexer itself is
    // covered exhaustively in core, where it is pure.
    //
    // `body_kind` used to be a public field assigned from two places. Two entities holding one
    // fact is how they drift — assigning it directly still compiles and silently leaves JSON
    // painted flat, or XML painted as JSON — so it goes through `set_body_kind` now, and this is
    // what would fail if a third call site skipped the funnel.
    let (_, view, mut cx) = boot(cx, None, None);

    let highlighting =
        |view: &gpui::Entity<RequestView>, cx: &mut VisualTestContext| -> bool {
            cx.update(|_, cx| view.read(cx).body_editor.read(cx).highlights_json())
        };

    assert!(
        highlighting(&view, &mut cx),
        "the sample request has a JSON body, so it starts coloured"
    );

    // Switch to plain text through the real picker, the way a person would.
    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("text");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).body_kind()),
        RawKind::Text,
        "the picker must have taken"
    );
    assert!(
        !highlighting(&view, &mut cx),
        "plain text is not JSON and must not be lexed as it"
    );

    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("json");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        highlighting(&view, &mut cx),
        "and switching back turns it on again"
    );
}

#[gpui::test]
async fn the_widest_row_is_found_in_a_realistically_shaped_body(cx: &mut TestAppContext) {
    // The reported shape: one long `description` nested three levels down among many shallow
    // short rows. The extent feeds the pixel width the pane computes, so getting the wrong row
    // here is what leaves the end of the longest line unreachable.
    let desc = "Pagination metadata returned by registries that implement dynamic search. Its presence on a catalog response signals that the items are already filtered and paged by the registry.";
    let body = format!(
        r#"{{"a":1,"properties":{{"pagination":{{"description":"{desc}","type":"object"}},"name":{{"type":"string"}}}}}}"#
    );
    let (view, mut cx) = respond_with(cx, "application/json", body);

    let (depth, chars) = cx.update(|_, cx| view.read(cx).body_view.as_ref().unwrap().widest_extent());
    assert_eq!(depth, 3, "the description is three levels down");
    assert!(
        chars as usize > desc.len(),
        "the extent must cover the key and the whole quoted value, not a prefix: {chars}"
    );
}

#[gpui::test]
async fn the_widest_row_can_actually_be_reached(cx: &mut TestAppContext) {
    // The complaint this slice was reopened for: scrolling worked, but stopped short of the end.
    // Asserting `hidden > 0` — which is what the first version of these tests did — is true for
    // a region sized to the *wrong* row, so it cannot see that at all.
    //
    // 400 characters at any plausible monospace advance is well over 2500px; a region sized from
    // the wrong row collapses to roughly the viewport, about 960px, so the floor separates the
    // two by a wide margin without pinning a font metric.
    //
    // **The threshold was 3000 and had to come down, which is the interesting part.** gpui was
    // measuring the row *before* the list's text style applied, so it shaped in the ambient font
    // at 8.47px per character where the row actually draws at 7.29. The old number was calibrated
    // against that over-measurement. Computing the width from the render font gives 2977 — and
    // being wrong in the other direction is what left the real window short of the line's end.
    let wide = format!(r#"{{"k":"{}"}}"#, "x".repeat(400));
    let (view, mut cx) = respond_with(cx, "application/json", wide);

    let viewport = cx.update(|_, cx| {
        f32::from(view.read(cx).body_scroll.0.borrow().base_handle.bounds().size.width)
    });
    let content = viewport + body_h_hidden(&view, &mut cx);

    assert!(
        content > 2500.,
        "the scroll region must span the whole 400-character value, not stop short: {content}"
    );

    // **And the upper bound is the load-bearing half.** Below 2500 means the region was sized
    // from the wrong row; above ~3300 means it was sized by gpui measuring a row *before* the
    // list's text style applied, which shapes in the ambient font — 8.47px per character here
    // against the 7.29 the row actually draws at. Only the second of those was the reported bug,
    // and only this bound can see it: with the pane computing the width from the render font the
    // total is 2977, and with gpui measuring it in the ambient font it is 3465.
    //
    // It is a font-metric assertion and therefore fragile, which is a fair price: the headless
    // platform cannot reproduce the real font pair, so a range is the only way to pin *which*
    // font decided the answer rather than merely that the answer is large.
    assert!(
        content < 3300.,
        "the region must be sized in the font the rows are drawn in, not the ambient one: \
         {content}"
    );
}

#[gpui::test]
async fn the_editor_cannot_be_scrolled_past_its_longest_line(cx: &mut TestAppContext) {
    // The mirror of the bug above, from the same wrong reference line: when the cursor's line
    // was scrolled out of view the clamp never ran at all, so there was no bound and the text
    // could be pushed arbitrarily far off to the left into blank space.
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-b");
    cx.simulate_input(&"x".repeat(120));
    cx.simulate_keystrokes("ctrl-home");
    cx.run_until_parked();

    let editor = cx.debug_bounds("body-editor").expect("the body editor");
    for _ in 0..40 {
        wheel(&mut cx, editor.center(), -400., 0.);
    }

    let offset = cx.update(|_, cx| f32::from(view.read(cx).body_editor.read(cx).h_offset()));
    // 120 monospace characters is well under 4000px however it shapes, so anything past that
    // means the offset ran away rather than stopping at the end of the content.
    assert!(
        offset < 4000.,
        "scrolling must stop at the widest line, not run on forever: {offset}"
    );
}

#[gpui::test]
async fn a_raw_body_copies_the_whole_line_and_refuses_a_path(cx: &mut TestAppContext) {
    // Two halves of the same decision. The drawn line stops at `MAX_DISPLAY_LINE`; a copy that
    // silently handed back 4KB of a longer one would be a wrong answer wearing a right one.
    // And there is no structure to name a position within, so the path verb says so rather
    // than inventing a line number no tool downstream accepts.
    let (_, view, mut cx) = boot(cx, None, None);
    // Deliberately past `MAX_DISPLAY_LINE`. A short line would make `line` and `full_line`
    // identical, so the test would pass against a call site that copies the drawn text — the
    // unit test on `full_line` would be covering the type while leaving this unheld.
    let long = "x".repeat(zuno_core::lines::MAX_DISPLAY_LINE * 2);
    let url = serve_typed_owned("text/plain", format!("alpha\n{long}"));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    wait_for_body(&view, &mut cx);
    focus_response(&mut cx);

    cx.simulate_keystrokes("down down");
    assert_eq!(selected_row(&view, &mut cx), Some(1), "the long line");

    cx.simulate_keystrokes("ctrl-c");
    cx.run_until_parked();
    assert_eq!(
        clipboard_text(&mut cx).as_deref(),
        Some(long.as_str()),
        "the whole line, not the 4KB the viewer draws"
    );

    cx.simulate_keystrokes("alt-c");
    cx.run_until_parked();
    let status = buffer_status(&view, &mut cx);
    assert!(
        status.contains("JSON"),
        "a raw body has no path, and the verb has to say why: {status:?}"
    );
}

#[gpui::test]
async fn ctrl_shift_c_copies_the_response_body_verbatim(cx: &mut TestAppContext) {
    // Raw bytes, not the pretty-printed outline on screen: what you paste into a fixture or
    // a bug report has to be what actually came back.
    const BODY: &str = "{\"a\":1,\"b\":[2,3]}";
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_bytes(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"a\":1,\"b\":[2,3]}",
    );

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);

    cx.simulate_keystrokes("ctrl-shift-c");
    assert_eq!(
        clipboard_text(&mut cx).as_deref(),
        Some(BODY),
        "the clipboard must hold the bytes the server sent, unreformatted"
    );

    let status = cx.update(|_, cx| view.read(cx).status.clone());
    assert!(
        status.is_some_and(|s| s.contains("Copied")),
        "a copy that says nothing is indistinguishable from one that failed"
    );
}

#[gpui::test]
async fn copying_a_binary_response_points_at_saving_instead(cx: &mut TestAppContext) {
    // Invariant 4: a body that isn't valid UTF-8 is normal, not an error. The clipboard
    // needs a `String`, so the honest move is to name the alternative rather than copy
    // mojibake.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_bytes(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\n\r\n\xff\xfe\xfd\xfc",
    );

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);

    cx.simulate_keystrokes("ctrl-shift-c");
    let status = cx
        .update(|_, cx| view.read(cx).status.clone())
        .expect("a status");
    assert!(status.contains("isn't text"), "{status:?}");
    assert!(status.contains("save"), "must name the way out: {status:?}");
}

#[gpui::test]
async fn copying_with_no_response_says_so(cx: &mut TestAppContext) {
    let (_, view, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-shift-c");

    let status = cx.update(|_, cx| view.read(cx).status.clone());
    assert!(
        status.is_some_and(|s| s.contains("No response")),
        "a no-op keystroke should explain itself"
    );
}

#[gpui::test]
async fn copying_while_browsing_history_copies_the_run_on_screen(cx: &mut TestAppContext) {
    // Egress reads `displayed()`, so it follows the history browser rather than always
    // grabbing the live response — copying something other than what you're looking at
    // would be the worst possible bug in this feature.
    let (_, view, mut cx) = boot(cx, None, None);
    let url = serve_sequence(&[(200, r#"{"older":1}"#), (201, r#"{"newer":2}"#)]);

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input(&url);
    send_and_wait(&mut cx, &view, 200);
    send_and_wait(&mut cx, &view, 201);

    cx.simulate_keystrokes("ctrl-h down enter");
    assert_eq!(viewing(&view, &mut cx), 1);

    cx.simulate_keystrokes("ctrl-shift-c");
    assert_eq!(
        clipboard_text(&mut cx).as_deref(),
        Some(r#"{"older":1}"#),
        "must copy the run being shown, not the live one"
    );
}

#[test]
fn a_suggested_filename_is_safe_and_matches_the_content_type() {
    use crate::workspace::suggested_filename;

    assert_eq!(suggested_filename("invoices", Some("application/json")), "invoices.json");
    // Parameters don't change the essence.
    assert_eq!(
        suggested_filename("invoices", Some("application/json; charset=utf-8")),
        "invoices.json"
    );
    assert_eq!(suggested_filename("report", Some("text/csv")), "report.csv");
    assert_eq!(suggested_filename("page", Some("text/html")), "page.html");
    // An unknown or absent type claims nothing.
    assert_eq!(suggested_filename("blob", Some("application/octet-stream")), "blob.bin");
    assert_eq!(suggested_filename("blob", None), "blob.bin");
    // Any `text/*` is readable.
    assert_eq!(suggested_filename("notes", Some("text/plain")), "notes.txt");

    // The label comes from a URL, so the same traversal guard as saving a request applies.
    let escaped = suggested_filename("../../.ssh/config", Some("application/json"));
    assert!(!escaped.contains('/'), "{escaped:?}");
    assert!(!escaped.starts_with('.'), "{escaped:?}");
}

#[gpui::test]
async fn a_multipart_body_survives_a_curl_import(cx: &mut TestAppContext) {
    // The data loss this fixes, at its most reachable: curl import has parsed `-F` since
    // M1, and `load` used to map anything non-raw to an empty editor.
    let (window, _, mut cx) = boot(cx, None, None);

    put_on_clipboard(
        &mut cx,
        "curl https://api.test/upload -F name=zuno -F file=@/tmp/payload.bin",
    );
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();

    let (_, spec) = tabs_of(&window, &mut cx);
    let Body::Multipart(fields) = &spec.body else {
        panic!("the multipart body was lost: {:?}", spec.body);
    };
    assert_eq!(fields.len(), 2, "{fields:?}");
}

#[gpui::test]
async fn saving_a_request_does_not_overwrite_an_imported_body(cx: &mut TestAppContext) {
    // The step that made the old loss permanent: the emptied editor was derived into `Empty`
    // and written to the collection file. Every body type is authorable now, so this guards
    // the round trip rather than a read-only holding pen.
    let (session, root) = scratch_collection("preserve-save");
    let (_, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    put_on_clipboard(&mut cx, "curl https://api.test/upload -F name=zuno");
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-s");

    let bytes = std::fs::read(root.join("upload.json")).expect("saved file");
    let saved: RequestSpec = serde_json::from_slice(&bytes).expect("parse");
    assert!(
        matches!(&saved.body, Body::Multipart(fields) if fields.len() == 1),
        "the saved file must keep the body: {:?}",
        saved.body
    );

    // And it round-trips: reopening must not lose it either.
    cx.simulate_keystrokes("ctrl-w");
    let reopened = zuno_core::collection::read(&root.join("upload.json")).expect("read");
    assert!(matches!(reopened.body, Body::Multipart(_)));

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn an_imported_binary_body_arrives_editable(cx: &mut TestAppContext) {
    // Binary used to be *preserved* — held read-only because nothing could author it. Now
    // that it's authorable, an import has to arrive as a chosen file rather than a blob.
    let (window, _, mut cx) = boot(cx, None, None);

    put_on_clipboard(
        &mut cx,
        "curl https://api.test/blob -X PUT --data-binary @/tmp/payload.bin",
    );
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();

    let (_, spec) = tabs_of(&window, &mut cx);
    assert_eq!(spec.body, Body::Binary(PathBuf::from("/tmp/payload.bin")));

    let imported = active_view(&window, &mut cx);
    // Asserts the *type*, not just the body: while non-raw bodies were held read-only in a
    // since-removed `preserved_body` field, `spec.body` and `body_label` both looked correct
    // either way, so only the body type distinguished "editable" from "held".
    assert_eq!(
        cx.update(|_, cx| imported.read(cx).body_type),
        crate::request_view::BodyType::Binary,
        "an import must arrive as an editable body of the right type"
    );
    assert_eq!(
        cx.update(|_, cx| imported.read(cx).binary_path.clone()),
        Some(PathBuf::from("/tmp/payload.bin"))
    );
    assert_eq!(cx.update(|_, cx| imported.read(cx).body_label()), "Binary");
}

#[gpui::test]
async fn a_raw_body_is_still_editable_and_not_preserved(cx: &mut TestAppContext) {
    // The guard against over-applying this: only variants the editor *can't* express get
    // set aside, or every request would become read-only.
    let (_, view, mut cx) = boot(cx, None, None);

    assert_eq!(
        cx.update(|_, cx| view.read(cx).body_type),
        crate::request_view::BodyType::Raw,
        "the sample's raw body must stay editable"
    );

    clear_body(&mut cx);
    cx.simulate_input("{\"typed\":true}");
    let spec = spec_of(&view, &mut cx);
    assert_eq!(spec.body.as_text(), Some("{\"typed\":true}"));
}

#[gpui::test]
async fn a_fresh_buffer_starts_with_no_body(cx: &mut TestAppContext) {
    // Empty is a real body type, not an absence to be papered over: typing into the editor
    // is how you get a raw body from here.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-t");
    // The *new* buffer, not the one `boot` returned — `ctrl-t` opens one.
    let view = active_view(&window, &mut cx);

    assert_eq!(
        cx.update(|_, cx| view.read(cx).body_type),
        crate::request_view::BodyType::Empty,
        "a fresh buffer starts with no body"
    );
}



#[gpui::test]
async fn clicking_a_window_control_does_not_also_start_a_window_drag(cx: &mut TestAppContext) {
    // The whole titlebar is a drag handle, and `on_mouse_down` registers a *Bubble*-phase
    // listener — GPUI runs every bubble listener whose hitbox was hit, in reverse paint order,
    // until one clears `propagate_event`. So without `stop_propagation` on the button, the
    // button acts and *then* its ancestor titlebar calls `start_window_move`: the compositor
    // starts dragging a window the user was trying to close.
    //
    // What makes this observable at all is that the test platform's `start_window_move` is
    // `unimplemented!()`, so reaching the titlebar's handler panics and fails this test.
    // Minimize and maximize can't stand in here — their own platform calls are
    // `unimplemented!()` too, so they'd panic either way — which is why this clicks close,
    // whose action is a plain flag on the window.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.run_until_parked();

    let close = cx
        .debug_bounds("close")
        .expect("the close button should be painted");
    // A bare mouse-*down*, not `simulate_click`: `on_mouse_down` fires on the down event, and
    // this particular button removes the window, so the paired mouse-up would land on a window
    // that no longer exists and panic inside gpui's own test context.
    cx.simulate_event(gpui::MouseDownEvent {
        position: close.center(),
        modifiers: gpui::Modifiers::default(),
        button: gpui::MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });

    // And the button's own action still has to run: stopping propagation must not cost the
    // click its effect, which is the plausible way to "fix" this wrongly.
    assert!(
        window.update(&mut cx, |_, _, _| ()).is_err(),
        "the close button should have closed the window"
    );
}

#[gpui::test]
async fn clicking_the_body_chip_opens_the_type_picker(cx: &mut TestAppContext) {
    // The chip used to cycle `RawKind` in place, so it could never reach Form, Binary or
    // Multipart — and on those three it mutated hidden state while the label stayed put, which
    // looked like a dead control. A real click, because the bug was *in* the click path: only
    // dispatching the action makes the chip and Ctrl+Shift+B the same verb.
    let (window, view, mut cx) = boot(cx, None, None);
    cx.run_until_parked();

    let chip = cx
        .debug_bounds("body-kind-chip")
        .expect("the body-kind chip should be painted");
    cx.simulate_click(chip.center(), gpui::Modifiers::default());

    assert!(
        picker_is_open(&window, &mut cx),
        "the chip must open the body-type picker"
    );
    let rows = picker_rows(&window, &mut cx);
    assert!(
        rows.iter().any(|row| row.starts_with("Multipart")),
        "and it must be the picker that reaches every type: {rows:?}"
    );

    // The discriminating half: cycling would have moved the sample's JSON body to Text on this
    // very click. Opening a picker changes nothing until something is chosen.
    assert_eq!(
        cx.update(|_, cx| view.read(cx).body_kind()),
        RawKind::Json,
        "clicking must not mutate the body kind in place"
    );
}

#[gpui::test]
async fn a_body_less_request_says_none_rather_than_a_retained_sub_kind(cx: &mut TestAppContext) {
    // `body_label` folded `Empty` in with `Raw` and returned `body_kind`, which defaults to
    // Json — so a fresh buffer's chip read "JSON" next to a pane reading "No body".
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-t");
    let view = active_view(&window, &mut cx);

    assert_eq!(
        cx.update(|_, cx| view.read(cx).body_type),
        crate::request_view::BodyType::Empty
    );
    assert_eq!(
        cx.update(|_, cx| view.read(cx).body_label()),
        "None",
        "a request that sends no body must not advertise a content type"
    );
}

#[gpui::test]
async fn the_body_type_picker_marks_none_as_current_on_a_fresh_buffer(cx: &mut TestAppContext) {
    // The consequence that made the mislabel more than cosmetic: the picker marks its current
    // row by comparing `body_label()` against the row labels, so it marked JSON as current on
    // a buffer with no body and could never mark None. Deliberately on a *fresh* buffer — the
    // existing coverage boots the sample request, whose raw JSON body makes the old label
    // accidentally correct.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-t");
    cx.simulate_keystrokes("ctrl-shift-b");

    let rows = picker_rows(&window, &mut cx);
    let none = rows.iter().find(|row| row.starts_with("None")).expect("a None row");
    assert!(none.contains("current"), "{rows:?}");
    let json = rows.iter().find(|row| row.starts_with("JSON")).expect("a JSON row");
    assert!(
        !json.contains("current"),
        "JSON must not claim to be the current body: {rows:?}"
    );
}

#[gpui::test]
async fn a_form_body_reaches_the_wire_urlencoded(cx: &mut TestAppContext) {
    // Asserted against the bytes a server received, not against the spec — the encoding is
    // the part that could silently be wrong.
    let (window, _, mut cx) = boot(cx, None, None);
    let (url, server) = serve_capturing(OK_JSON);

    // A *fresh* buffer, because the sample ships `Content-Type: application/json` and an
    // explicit header outranks the derived one — leaving it would send a urlencoded body
    // labelled JSON. That precedence has its own test.
    cx.simulate_keystrokes("ctrl-t");
    let view = active_view(&window, &mut cx);
    assert!(
        spec_of(&view, &mut cx).headers.is_empty(),
        "a new buffer should start with no headers"
    );
    cx.simulate_input(&url);

    // Ctrl+Shift+F switches to a form and adds a field in one go.
    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("grant_type");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("client_credentials");
    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("scope");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("read write");

    assert_eq!(spec_of(&view, &mut cx).body.label(), "Form");

    cx.simulate_keystrokes("ctrl-enter");
    wait_for(&mut cx, "the response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });

    let request = server.join().expect("server thread");
    assert!(
        request.contains("grant_type=client_credentials&scope=read+write"),
        "the form body should be urlencoded:\n{request}"
    );
    assert!(
        request.to_lowercase().contains("content-type: application/x-www-form-urlencoded"),
        "and carry the derived content type:\n{request}"
    );
}

#[gpui::test]
async fn a_disabled_form_field_is_left_out(cx: &mut TestAppContext) {
    // Same contract as headers and query rows: muting a field must not lose what you typed.
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("keep");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("yes");
    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("drop");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("no");
    // Focus is in the second row, so this mutes it.
    cx.simulate_keystrokes("alt-t");

    let spec = spec_of(&view, &mut cx);
    let Body::Form(fields) = &spec.body else {
        panic!("expected a form body: {:?}", spec.body);
    };
    assert_eq!(fields.len(), 2, "the muted row must still exist");
    assert!(!fields[1].enabled, "and be marked disabled: {fields:?}");
    assert_eq!(fields[1].value, "no", "with its text intact");
}

#[gpui::test]
async fn a_form_body_survives_a_save_and_reopen(cx: &mut TestAppContext) {
    let (session, root) = scratch_collection("form-roundtrip");
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/token");
    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("grant_type");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("password");
    cx.simulate_keystrokes("ctrl-s");

    let bytes = std::fs::read(root.join("token.json")).expect("saved");
    let saved: RequestSpec = serde_json::from_slice(&bytes).expect("parse");
    let Body::Form(fields) = &saved.body else {
        panic!("the form body was not persisted: {:?}", saved.body);
    };
    assert_eq!(fields[0].name, "grant_type");

    // And reopening gives back an *editable* form, not a preserved blob.
    cx.simulate_keystrokes("ctrl-w");
    cx.simulate_keystrokes("ctrl-p");
    wait_for(&mut cx, "the scanned request", |cx| {
        picker_rows(&window, cx).iter().any(|r| r.contains("token")).then_some(())
    });
    cx.simulate_input("token");
    cx.simulate_keystrokes("enter");

    let reopened = active_view(&window, &mut cx);
    assert_eq!(
        cx.update(|_, cx| reopened.read(cx).body_type),
        crate::request_view::BodyType::Form,
        "a reopened form must come back as an editable form"
    );
    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("extra");
    let Body::Form(fields) = &spec_of(&reopened, &mut cx).body else {
        panic!("expected a form body");
    };
    assert_eq!(fields.len(), 2, "the reopened form must be editable: {fields:?}");

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn choosing_none_sends_no_body_and_switching_back_is_lossless(cx: &mut TestAppContext) {
    // Two things at once, because they're the same decision. "None" has to mean *no body*
    // even though the editor still holds text — falling through to the editor meant the
    // setting looked applied and wasn't. And what you can still see is kept, so switching
    // back restores it rather than silently emptying your work.
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-shift-f");
    cx.simulate_input("field");
    assert_eq!(spec_of(&view, &mut cx).body.label(), "Form");

    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("none");
    cx.simulate_keystrokes("enter");
    assert!(
        matches!(spec_of(&view, &mut cx).body, Body::Empty),
        "None must send nothing, whatever the editors still hold: {:?}",
        spec_of(&view, &mut cx).body
    );

    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("form");
    cx.simulate_keystrokes("enter");
    let Body::Form(fields) = &spec_of(&view, &mut cx).body else {
        panic!("expected the form back");
    };
    assert_eq!(fields.len(), 1, "the form rows must survive a round trip: {fields:?}");
    assert_eq!(fields[0].name, "field");
}

#[gpui::test]
async fn a_stale_content_type_header_is_reported_not_silently_obeyed(cx: &mut TestAppContext) {
    // The trap this closes: `build.rs` only derives a Content-Type when no explicit header is
    // set, so the sample's `application/json` header outranks a form body — the request goes
    // out urlencoded while declaring itself JSON, and the server misparses it.
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("form");
    cx.simulate_keystrokes("enter");

    let status = cx
        .update(|_, cx| view.read(cx).status.clone())
        .expect("a status");
    assert!(status.contains("Content-Type"), "{status:?}");
    assert!(status.contains("x-www-form-urlencoded"), "should name what was expected: {status:?}");

    // And no false alarm when they agree.
    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("json");
    cx.simulate_keystrokes("enter");
    assert!(
        cx.update(|_, cx| view.read(cx).conflicting_content_type(cx)).is_none(),
        "application/json and a JSON body do not conflict"
    );
}

#[gpui::test]
async fn an_imported_multipart_body_arrives_editable(cx: &mut TestAppContext) {
    // Multipart used to be *held* read-only, because nothing could author it. It's the last
    // body type to become editable, which is what let `preserved_body` go entirely.
    let (window, _, mut cx) = boot(cx, None, None);

    put_on_clipboard(
        &mut cx,
        "curl https://api.test/upload -F caption=hello -F avatar=@/tmp/pic.png",
    );
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();

    let imported = active_view(&window, &mut cx);
    assert_eq!(
        cx.update(|_, cx| imported.read(cx).body_type),
        crate::request_view::BodyType::Multipart
    );
    assert_eq!(cx.update(|_, cx| imported.read(cx).body_label()), "Multipart");

    // Editable means the parts are real rows, and the file part is marked as one.
    let (text_parts, file_parts) = cx.update(|_, cx| {
        let view = imported.read(cx);
        (
            view.multipart.iter().filter(|part| !part.is_file).count(),
            view.multipart.iter().filter(|part| part.is_file).count(),
        )
    });
    assert_eq!((text_parts, file_parts), (1, 1), "one text part and one file part");

    // And it still round-trips through the spec.
    let Body::Multipart(fields) = &spec_of(&imported, &mut cx).body else {
        panic!("expected multipart");
    };
    assert_eq!(fields.len(), 2);
    assert!(matches!(&fields[1].value, MultipartValue::File(path) if path.ends_with("pic.png")));
}

#[gpui::test]
async fn the_body_type_picker_offers_every_authorable_type(cx: &mut TestAppContext) {
    // Cycling could only reach JSON/Text/XML/HTML, so form, binary and multipart were
    // unreachable no matter how many times you pressed it. Every `Body` variant is now here,
    // which is what let `preserved_body` go.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-shift-b");

    let rows = picker_rows(&window, &mut cx);
    let labels: Vec<&str> = rows
        .iter()
        .map(|row| row.split(" — ").next().unwrap_or(""))
        .collect();
    assert_eq!(
        labels,
        ["None", "JSON", "Form", "Binary", "Multipart", "Text", "XML", "HTML"],
        "{rows:?}"
    );
    // The sample request is JSON, and the list should say which one you're on.
    let json = rows.iter().find(|row| row.starts_with("JSON")).expect("a JSON row");
    assert!(json.contains("current"), "{json:?}");
}

#[gpui::test]
async fn a_binary_body_with_no_file_chosen_sends_nothing(cx: &mut TestAppContext) {
    // Incomplete, not malformed: `Binary("")` would be a request that can't succeed, and
    // sending nothing is the honest reading of "no file chosen".
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("binary");
    cx.simulate_keystrokes("enter");

    assert_eq!(cx.update(|_, cx| view.read(cx).body_label()), "Binary");
    assert!(
        matches!(spec_of(&view, &mut cx).body, Body::Empty),
        "{:?}",
        spec_of(&view, &mut cx).body
    );
}

#[gpui::test]
async fn a_chosen_file_becomes_the_body_and_its_bytes_reach_the_wire(cx: &mut TestAppContext) {
    // The engine reads the path at send, so this is the assertion that matters: the *file's
    // contents* on the socket, not just a path in the spec.
    let dir = scratch_dir("binary-body");
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"\x89PNG\r\n\x1a\n-not-really").expect("write");

    let (window, _, mut cx) = boot(cx, None, None);
    let (url, server) = serve_capturing(OK_JSON);

    // A fresh buffer: the sample's `Content-Type: application/json` header would otherwise
    // mislabel the upload, and `build.rs` sends no type of its own for binary.
    cx.simulate_keystrokes("ctrl-t");
    let view = active_view(&window, &mut cx);
    cx.simulate_input(&url);

    // The dialog can't be driven headlessly, so set the path the way the dialog's callback
    // does and assert everything downstream of it.
    cx.update(|_, cx| view.update(cx, |view, cx| view.set_binary_path(file.clone(), cx)));
    assert_eq!(
        spec_of(&view, &mut cx).body,
        Body::Binary(file.clone()),
        "the path should become the body"
    );

    cx.simulate_keystrokes("ctrl-enter");
    wait_for(&mut cx, "the response", |cx| {
        cx.update(|_, cx| view.read(cx).response.clone())
    });

    let request = server.join().expect("server thread");
    assert!(
        request.contains("-not-really"),
        "the file's bytes should be the request body:\n{request}"
    );
    // Read the length rather than hardcoding it — the first version of this test asserted 22
    // for a 19-byte file and would have "passed" for the wrong reason had it matched.
    let expected = std::fs::metadata(&file).expect("metadata").len();
    assert!(
        request
            .to_lowercase()
            .contains(&format!("content-length: {expected}")),
        "declared length should match the file ({expected} bytes):\n{request}"
    );
    // Deliberate: `build.rs` guesses no type for binary uploads.
    assert!(
        !request.to_lowercase().contains("content-type:"),
        "no Content-Type should be invented:\n{request}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
async fn a_missing_body_file_is_reported_rather_than_sent_empty(cx: &mut TestAppContext) {
    // The path is checked at send, not at selection — a file deleted between sends has to
    // fail loudly rather than quietly posting nothing.
    let (window, _, mut cx) = boot(cx, None, None);
    let url = serve_once(OK_JSON);

    cx.simulate_keystrokes("ctrl-t");
    let view = active_view(&window, &mut cx);
    cx.simulate_input(&url);

    // No scratch directory: the whole point is a path that doesn't exist, so creating one
    // would leave an empty directory behind for nothing.
    let missing = std::env::temp_dir().join("zuno-no-such-body-file.bin");
    assert!(!missing.exists(), "the test needs this to not exist");
    cx.update(|_, cx| view.update(cx, |view, cx| view.set_binary_path(missing.clone(), cx)));
    cx.simulate_keystrokes("ctrl-enter");

    let error = wait_for(&mut cx, "the failure", |cx| {
        cx.update(|_, cx| view.read(cx).error.clone())
    });
    assert!(
        matches!(&error, EngineError::BodyFileUnreadable { .. }),
        "{error:?}"
    );
    assert!(error.is_local(), "nothing should have left the machine");
}

#[gpui::test]
async fn a_binary_body_survives_a_save_and_reopen_as_editable(cx: &mut TestAppContext) {
    // Binary is authorable now, so it must come back as a chosen path rather than a
    // read-only preserved blob.
    let (session, root) = scratch_collection("binary-roundtrip");
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/v1/upload");
    let file = PathBuf::from("/tmp/zuno-example-payload.bin");
    let view = active_view(&window, &mut cx);
    cx.update(|_, cx| view.update(cx, |view, cx| view.set_binary_path(file.clone(), cx)));
    cx.simulate_keystrokes("ctrl-s");

    let bytes = std::fs::read(root.join("upload.json")).expect("saved");
    let saved: RequestSpec = serde_json::from_slice(&bytes).expect("parse");
    assert_eq!(saved.body, Body::Binary(file.clone()));

    cx.simulate_keystrokes("ctrl-w");
    cx.simulate_keystrokes("ctrl-p");
    wait_for(&mut cx, "the scanned request", |cx| {
        picker_rows(&window, cx).iter().any(|r| r.contains("upload")).then_some(())
    });
    cx.simulate_input("upload");
    cx.simulate_keystrokes("enter");

    let reopened = active_view(&window, &mut cx);
    assert_eq!(
        cx.update(|_, cx| reopened.read(cx).body_type),
        crate::request_view::BodyType::Binary,
        "a reopened binary body must come back editable"
    );
    assert_eq!(
        cx.update(|_, cx| reopened.read(cx).binary_path.clone()),
        Some(file),
        "the chosen file must come back"
    );
    assert_eq!(cx.update(|_, cx| reopened.read(cx).body_label()), "Binary");

    remove_scratch(&mut cx, &session);
}

#[gpui::test]
async fn switching_from_binary_keeps_the_path_for_switching_back(cx: &mut TestAppContext) {
    // Same rule as the editor's text and the form rows: what you can still see is kept, so
    // a mistaken type change isn't destructive.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-t");
    let view = active_view(&window, &mut cx);

    let file = PathBuf::from("/tmp/zuno-keepme.bin");
    cx.update(|_, cx| view.update(cx, |view, cx| view.set_binary_path(file.clone(), cx)));

    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("json");
    cx.simulate_keystrokes("enter");
    assert_eq!(cx.update(|_, cx| view.read(cx).body_label()), "JSON");

    cx.simulate_keystrokes("ctrl-shift-b");
    cx.simulate_input("binary");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        spec_of(&view, &mut cx).body,
        Body::Binary(file),
        "the path should survive a round trip through another type"
    );
}

#[gpui::test]
async fn ctrl_shift_m_adds_a_part_and_switches_the_body(cx: &mut TestAppContext) {
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("caption");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("hello");

    assert_eq!(cx.update(|_, cx| view.read(cx).body_label()), "Multipart");
    let Body::Multipart(fields) = &spec_of(&view, &mut cx).body else {
        panic!("expected multipart: {:?}", spec_of(&view, &mut cx).body);
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "caption");
    // A new part is text until a file is attached.
    assert_eq!(fields[0].value, MultipartValue::Text("hello".to_string()));
}

#[gpui::test]
async fn attaching_a_file_turns_a_part_into_a_file_part(cx: &mut TestAppContext) {
    // The dialog can't be driven headlessly, so this exercises everything downstream of the
    // path arriving — which is where the text/file distinction is decided.
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("avatar");
    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("caption");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("hi");

    let file = PathBuf::from("/tmp/zuno-avatar.png");
    cx.update(|_, cx| view.update(cx, |view, cx| view.set_multipart_file(0, file.clone(), cx)));

    let Body::Multipart(fields) = &spec_of(&view, &mut cx).body else {
        panic!("expected multipart");
    };
    assert_eq!(fields[0].value, MultipartValue::File(file), "part 0 became a file");
    assert_eq!(
        fields[1].value,
        MultipartValue::Text("hi".to_string()),
        "and the other part is untouched"
    );
}

#[gpui::test]
async fn attaching_a_file_targets_the_focused_part_or_the_whole_body(cx: &mut TestAppContext) {
    // One verb, two meanings, decided by focus. This asserts the decision itself, since the
    // native dialog can't be opened in a test.
    let (window, _, mut cx) = boot(cx, None, None);
    cx.simulate_keystrokes("ctrl-t");
    let view = active_view(&window, &mut cx);

    // No multipart body: the file would become the whole binary body.
    assert_eq!(
        cx.update(|window, cx| view.read(cx).focused_multipart_row(window, cx)),
        None
    );

    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("first");
    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("second");

    // Focus is in the second part's name cell, so that's the one a file would attach to.
    assert_eq!(
        cx.update(|window, cx| view.read(cx).focused_multipart_row(window, cx)),
        Some(1),
        "the focused part is the target"
    );
}

#[gpui::test]
async fn a_disabled_part_is_left_out_but_kept(cx: &mut TestAppContext) {
    let (_, view, mut cx) = boot(cx, None, None);

    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("keep");
    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("drop");
    cx.simulate_keystrokes("alt-t");

    let Body::Multipart(fields) = &spec_of(&view, &mut cx).body else {
        panic!("expected multipart");
    };
    assert_eq!(fields.len(), 2, "the muted part must still exist");
    assert!(!fields[1].enabled, "{fields:?}");
    assert_eq!(fields[1].name, "drop", "with its text intact");
}

#[gpui::test]
async fn a_multipart_body_survives_a_save_and_reopen(cx: &mut TestAppContext) {
    // Including which parts are files — that distinction is the body type's whole point, and
    // it's the part a round trip could quietly flatten.
    let (session, root) = scratch_collection("multipart-roundtrip");
    let (window, _, mut cx) = boot(cx, Some(session.clone()), Some(root.clone()));

    cx.simulate_keystrokes("ctrl-l ctrl-a");
    cx.simulate_input("https://api.test/v1/upload");
    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("caption");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("hello");
    cx.simulate_keystrokes("ctrl-shift-m");
    cx.simulate_input("avatar");

    let view = active_view(&window, &mut cx);
    let file = PathBuf::from("/tmp/zuno-pic.png");
    cx.update(|_, cx| view.update(cx, |view, cx| view.set_multipart_file(1, file.clone(), cx)));
    cx.simulate_keystrokes("ctrl-s");

    let bytes = std::fs::read(root.join("upload.json")).expect("saved");
    let saved: RequestSpec = serde_json::from_slice(&bytes).expect("parse");
    let Body::Multipart(fields) = &saved.body else {
        panic!("the multipart body was not persisted: {:?}", saved.body);
    };
    assert_eq!(fields[0].value, MultipartValue::Text("hello".to_string()));
    assert_eq!(fields[1].value, MultipartValue::File(file.clone()));

    // Reopen, and the file part must still be a file part rather than text holding a path.
    cx.simulate_keystrokes("ctrl-w");
    cx.simulate_keystrokes("ctrl-p");
    wait_for(&mut cx, "the scanned request", |cx| {
        picker_rows(&window, cx).iter().any(|r| r.contains("upload")).then_some(())
    });
    cx.simulate_input("upload");
    cx.simulate_keystrokes("enter");

    let reopened = active_view(&window, &mut cx);
    let marks = cx.update(|_, cx| {
        reopened
            .read(cx)
            .multipart
            .iter()
            .map(|part| part.is_file)
            .collect::<Vec<_>>()
    });
    assert_eq!(marks, [false, true], "the text/file marks must survive");

    remove_scratch(&mut cx, &session);
}
