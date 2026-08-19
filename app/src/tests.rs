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
use std::time::{Duration, Instant};

use gpui::{TestAppContext, VisualTestContext};
use zuno_core::{EngineError, Method, RequestSpec, ResponseData};

use crate::request_view::RequestView;
use crate::theme::{Appearance, Theme};
use crate::workspace::Workspace;

/// Boot a window the same way `main` does, so the keymap, theme, and engine under test
/// are the real ones rather than a test-only arrangement.
fn open_workspace(cx: &mut TestAppContext) -> (gpui::Entity<RequestView>, VisualTestContext) {
    cx.update(|cx| {
        cx.set_global(Theme::new(Appearance::Dark, "monospace".into()));
        crate::register_keymap(cx);
        crate::engine::install(cx).expect("engine");
    });

    let window = cx.add_window(|window, cx| Workspace::new(window, cx));
    let view = window
        .update(cx, |workspace, _, _| workspace.active().unwrap())
        .unwrap();
    let vcx = VisualTestContext::from_window(window.into(), cx);

    (view, vcx)
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
async fn method_cycles_in_both_directions(cx: &mut TestAppContext) {
    let (view, mut cx) = open_workspace(cx);

    // The sample request is a POST.
    assert_eq!(spec_of(&view, &mut cx).method, Method::Post);

    cx.simulate_keystrokes("ctrl-m");
    assert_eq!(spec_of(&view, &mut cx).method, Method::Put);

    cx.simulate_keystrokes("ctrl-shift-m");
    assert_eq!(spec_of(&view, &mut cx).method, Method::Post);

    cx.simulate_keystrokes("ctrl-shift-m");
    assert_eq!(spec_of(&view, &mut cx).method, Method::Get);
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
