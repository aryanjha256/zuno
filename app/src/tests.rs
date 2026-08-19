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
use zuno_core::{Body, EngineError, Method, RawKind, RequestSpec, ResponseData};

use crate::body_view::BodyView;
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
        // Persistence must not touch the developer's real session file — the tests
        // drive SendRequest, and a send is a save point.
        crate::session::install_at(cx, None);
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

    cx.simulate_keystrokes("ctrl-shift-b");
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
