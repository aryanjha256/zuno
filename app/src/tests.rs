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
use crate::request_view::RequestView;
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

    // Row 1 is the nested object's open row.
    cx.update(|_, cx| view.update(cx, |view, cx| view.toggle_fold(1, cx)));
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
async fn the_body_kind_cycles(cx: &mut TestAppContext) {
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
fn serve_sequence(statuses: &'static [(u16, &'static str)]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        for (code, body) in statuses {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            let response = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
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
    // The distinguishing assertion. `spec.body` and `body_label` both look right even when
    // the body is *held* rather than authorable, because `preserved_body` also round-trips —
    // this is the only observable that separates "editable" from "read-only".
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
