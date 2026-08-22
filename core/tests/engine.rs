//! End-to-end engine tests against a real socket.
//!
//! M1.2's acceptance criterion is "a real request goes out, real bytes come back, and
//! Escape cancels mid-flight". A mocked transport would prove none of that, so these
//! run against a throwaway HTTP server on localhost — about 60 lines of `std::net`,
//! no test dependencies.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use zuno_core::{
    Body, Engine, EngineError, Event, Header, Method, MultipartField, MultipartValue, RawKind,
    RequestSpec,
};

// ---------------------------------------------------------------------------
// A minimal one-shot HTTP server
// ---------------------------------------------------------------------------

/// Accept one connection, capture the raw request, reply with `response`.
/// Returns the base URL and a handle yielding the request text the client sent.
fn serve_once(response: &'static str) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = read_request(&mut stream);
        stream.write_all(response.as_bytes()).expect("write");
        stream.flush().expect("flush");
        request
    });

    (format!("http://{addr}"), handle)
}

/// Accept one connection and then never reply, so a request stays in flight.
fn serve_never() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            // Hold the connection open until the test process exits.
            std::thread::sleep(Duration::from_secs(60));
            drop(stream);
        }
    });

    (format!("http://{addr}"), handle)
}

/// Read headers, then exactly `Content-Length` more bytes. A single `read` can split
/// mid-request, which would make assertions about the body flaky.
fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];

    loop {
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(end) = find_subslice(&buffer, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let expected = content_length(&head);
            if buffer.len() - (end + 4) >= expected {
                break;
            }
        }
    }

    String::from_utf8_lossy(&buffer).to_string()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Drain a job's event stream to completion, returning every event in order.
fn drain(events: &async_channel_shim::Receiver) -> Vec<Event> {
    let mut collected = Vec::new();
    while let Ok(event) = events.recv_blocking() {
        collected.push(event);
    }
    collected
}

/// The engine returns `async_channel::Receiver<Event>`; alias it so tests don't need
/// async-channel as a direct dependency.
mod async_channel_shim {
    pub type Receiver = async_channel::Receiver<zuno_core::Event>;
}

fn spec_for(url: String) -> RequestSpec {
    RequestSpec {
        url,
        // Short, so a broken cancel fails fast instead of hanging for 30s.
        settings: zuno_core::RequestSettings {
            timeout: Some(Duration::from_secs(3)),
            ..Default::default()
        },
        ..RequestSpec::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const OK_JSON: &str = "HTTP/1.1 200 OK\r\n\
     Content-Type: application/json\r\n\
     Content-Length: 11\r\n\
     \r\n\
     {\"ok\":true}";

#[test]
fn a_real_request_goes_out_and_real_bytes_come_back() {
    let (base, server) = serve_once(OK_JSON);
    let engine = Engine::new().expect("engine");

    let mut spec = spec_for(format!("{base}/health"));
    spec.headers = vec![Header::new("X-Zuno-Test", "1")];

    let (job, events) = engine.send(spec);
    let events = drain(&events);

    // What the server actually received.
    let request = server.join().expect("server thread");
    assert!(
        request.starts_with("GET /health HTTP/1.1\r\n"),
        "unexpected request line in:\n{request}"
    );
    assert!(
        request.to_ascii_lowercase().contains("x-zuno-test: 1"),
        "custom header missing from:\n{request}"
    );

    // Every event belongs to the job we submitted.
    assert!(events.iter().all(|event| event.job() == job));

    assert!(
        matches!(events.first(), Some(Event::Started { .. })),
        "first event should be Started, got {:?}",
        events.first()
    );

    // Head must arrive before the body is complete — that's what lets the status line
    // paint at TTFB rather than at completion.
    let head_ix = events
        .iter()
        .position(|event| matches!(event, Event::Head { .. }))
        .expect("a Head event");
    let done_ix = events
        .iter()
        .position(|event| matches!(event, Event::Done { .. }))
        .expect("a Done event");
    assert!(head_ix < done_ix, "Head must precede Done");

    let Some(Event::Head { status, headers, .. }) = events.get(head_ix) else {
        unreachable!()
    };
    assert_eq!(*status, 200);
    assert!(headers.iter().any(|h| h.name == "content-type"));

    let Some(Event::Done { response, .. }) = events.get(done_ix) else {
        unreachable!()
    };
    assert_eq!(response.status, 200);
    assert_eq!(response.status_text, "OK");
    assert_eq!(response.body_as_str(), Some("{\"ok\":true}"));
    assert_eq!(response.size.decoded, 11);
    assert_eq!(
        response.content_type(),
        Some("application/json"),
        "response headers should be readable by name"
    );
    assert!(
        response.timing.total >= response.timing.ttfb,
        "total ({:?}) must not be less than ttfb ({:?})",
        response.timing.total,
        response.timing.ttfb
    );
}

#[test]
fn a_post_sends_its_body_and_a_derived_content_type() {
    let (base, server) = serve_once(OK_JSON);
    let engine = Engine::new().expect("engine");

    let mut spec = spec_for(format!("{base}/items"));
    spec.method = Method::Post;
    spec.body = Body::Raw {
        text: "{\"name\":\"zuno\"}".to_string(),
        kind: RawKind::Json,
    };

    let (_, events) = engine.send(spec);
    drain(&events);

    let request = server.join().expect("server thread");
    assert!(request.starts_with("POST /items HTTP/1.1\r\n"), "{request}");
    assert!(
        request.to_ascii_lowercase().contains("content-type: application/json"),
        "Content-Type should be filled in from the body kind:\n{request}"
    );
    assert!(
        request.ends_with("{\"name\":\"zuno\"}"),
        "body missing from:\n{request}"
    );
}

#[test]
fn query_params_reach_the_request_line() {
    let (base, server) = serve_once(OK_JSON);
    let engine = Engine::new().expect("engine");

    let mut spec = spec_for(format!("{base}/search?q=rust"));
    spec.query = vec![
        zuno_core::QueryParam::new("page", "2"),
        zuno_core::QueryParam {
            enabled: false,
            name: "debug".into(),
            value: "1".into(),
        },
    ];

    let (_, events) = engine.send(spec);
    drain(&events);

    let request = server.join().expect("server thread");
    assert!(
        request.starts_with("GET /search?q=rust&page=2 HTTP/1.1\r\n"),
        "unexpected request line in:\n{request}"
    );
    assert!(!request.contains("debug"), "disabled param was sent:\n{request}");
}

#[test]
fn a_local_failure_never_touches_the_network() {
    let engine = Engine::new().expect("engine");

    // No server, no listener — this must fail before any socket is opened.
    let spec = spec_for("{{baseUrl}}/users".to_string());
    let (_, events) = engine.send(spec);
    let events = drain(&events);

    let failure = events
        .iter()
        .find_map(|event| match event {
            Event::Failed { error, .. } => Some(error),
            _ => None,
        })
        .expect("a Failed event");

    assert!(matches!(failure, EngineError::UnresolvedVariable { .. }));
    assert!(failure.is_local(), "{failure:?} should be classed as local");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Head { .. })),
        "a local failure must not produce a Head event"
    );
}

#[test]
fn a_refused_connection_is_reported_as_a_connect_error() {
    let engine = Engine::new().expect("engine");

    // Bind then immediately drop, so the port is almost certainly closed.
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr")
    };

    let (_, events) = engine.send(spec_for(format!("http://{addr}/")));
    let events = drain(&events);

    let failure = events
        .iter()
        .find_map(|event| match event {
            Event::Failed { error, .. } => Some(error),
            _ => None,
        })
        .expect("a Failed event");

    assert!(
        matches!(failure, EngineError::Connect { .. }),
        "expected a Connect error, got {failure:?}"
    );
    assert!(!failure.is_local());
}

#[test]
fn cancel_abandons_a_request_mid_flight() {
    let (base, _server) = serve_never();
    let engine = Engine::new().expect("engine");

    let (job, events) = engine.send(spec_for(format!("{base}/slow")));

    // Wait for the job to actually start before cancelling, so this tests
    // cancellation rather than a race with submission.
    let first = events.recv_blocking().expect("Started");
    assert!(matches!(first, Event::Started { .. }));

    engine.cancel(job);

    // Aborting the task drops the sender, which closes the channel with no terminal
    // event. If cancel were a no-op we'd instead see Failed(Timeout) after 3s.
    let remaining = drain(&events);
    assert!(
        !remaining.iter().any(Event::is_terminal),
        "cancelled job still produced a terminal event: {remaining:?}"
    );
}

/// gzip of `"zuno " * 40` — **28 bytes on the wire for 200 decoded**, so the ratio is one a person
/// would want to see, which is the whole reason this limitation is worth pinning.
///
/// A literal rather than a compression dependency: `zuno-core` has no other reason to take one, and
/// the bytes are deterministic (`mtime` zeroed) so they can be checked in. Deliberately a
/// *compressible* payload — the first version of this test gzipped a 27-byte string, which came out
/// 47 bytes because gzip's header and trailer outweigh a tiny payload, and the test caught the
/// mistaken assumption rather than the other way round.
const GZIPPED_BODY: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xab, 0x2a, 0xcd, 0xcb, 0x57, 0xa8,
    0x1a, 0xfa, 0x04, 0x00, 0xe7, 0x23, 0x01, 0x42, 0xc8, 0x00, 0x00, 0x00,
];

/// What `GZIPPED_BODY` decodes to.
const GZIPPED_PLAIN_LEN: u64 = 200;

#[test]
fn a_non_utf8_header_value_is_readable_rather_than_a_byte_dump() {
    // Header values may carry `obs-text` (0x80-0xFF), and a latin-1 filename in
    // `Content-Disposition` is the case that actually turns up. These used to render through
    // `format!("{:?}", bytes)` — a decimal array — so the one piece of information in the header
    // was unreadable.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = read_request(&mut stream);
            // 0xe9 is `é` in latin-1 and invalid UTF-8 on its own.
            let mut response = b"HTTP/1.1 200 OK\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Disposition: attachment; filename=\"caf"
                .to_vec();
            response.push(0xe9);
            response.extend_from_slice(
                b".txt\"\r\n\
                  Content-Length: 2\r\n\
                  Connection: close\r\n\
                  \r\n\
                  ok",
            );
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });

    let engine = Engine::new().expect("engine");
    let (_, events) = engine.send(spec_for(format!("http://{addr}/download")));
    let events = drain(&events);

    let Some(Event::Done { response, .. }) = events.iter().find(|event| event.is_terminal()) else {
        panic!("expected a completed response, got {events:?}");
    };

    let disposition = response
        .header("content-disposition")
        .expect("the header must survive, not be dropped");
    assert!(
        disposition.contains("caf\u{fffd}.txt"),
        "the undecodable byte should become U+FFFD and leave the rest readable: {disposition:?}"
    );
    assert!(
        !disposition.contains('['),
        "a debug byte array is not a header value: {disposition:?}"
    );
}

#[test]
fn a_compressed_response_is_decoded_and_reports_no_declared_length() {
    // **This test exists to pin a dependency's behaviour that our own docs rest on.** reqwest 0.13
    // delegates decompression to `tower-http`, which removes `Content-Encoding` *and*
    // `Content-Length` when it decodes — so `content_length()` comes back `None` and the wire size
    // is unrecoverable, which is why `SizeInfo::declared` is an `Option` and why the response pane
    // cannot show a compression ratio.
    //
    // If a future reqwest keeps those headers, this test fails — and that failure is the signal
    // that the ratio has become showable, rather than a regression.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = read_request(&mut stream);
            let head = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Encoding: gzip\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                GZIPPED_BODY.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(GZIPPED_BODY);
            let _ = stream.flush();
        }
    });

    let engine = Engine::new().expect("engine");
    let (_, events) = engine.send(spec_for(format!("http://{addr}/gzipped")));
    let events = drain(&events);

    let Some(Event::Done { response, .. }) = events.iter().find(|event| event.is_terminal()) else {
        panic!("expected a completed response, got {events:?}");
    };

    // Decompression really happened: the decoded body is the plain text, longer than the wire bytes.
    assert_eq!(
        response.body_as_str(),
        Some("zuno ".repeat(40).as_str()),
        "the body should arrive decompressed"
    );
    assert_eq!(response.size.decoded, GZIPPED_PLAIN_LEN);
    assert!(
        response.size.decoded > GZIPPED_BODY.len() as u64 * 5,
        "the payload really was compressed, so a ratio would have been worth showing"
    );

    // And the declaration is gone, which is the whole point.
    assert_eq!(
        response.size.declared, None,
        "Content-Length does not survive decompression, so the wire size is unknowable here"
    );
    assert!(
        response.header("content-encoding").is_none(),
        "Content-Encoding is stripped alongside it: {:?}",
        response.headers
    );
}

#[test]
fn a_declared_length_over_the_limit_is_refused_before_the_body_transfers() {
    // The cheap half of the cap: a server that *claims* more than Zuno will hold is refused as
    // soon as the head arrives, without pulling the body down. The body is deliberately never
    // sent — if the check failed to fire, this would sit waiting for bytes that never come and
    // fail on the 3s timeout from `spec_for` rather than hanging.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut discard = [0u8; 1024];
            let _ = stream.read(&mut discard);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: application/octet-stream\r\n\
                  Content-Length: 209715200\r\n\
                  \r\n",
            );
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    let engine = Engine::new().expect("engine");
    let (_, events) = engine.send(spec_for(format!("http://{addr}/artifact")));
    let events = drain(&events);

    let failure = events
        .iter()
        .find_map(|event| match event {
            Event::Failed { error, .. } => Some(error),
            _ => None,
        })
        .expect("a Failed event");

    assert!(
        matches!(failure, EngineError::BodyTooLarge { .. }),
        "expected BodyTooLarge, got {failure:?}"
    );
    assert!(
        !failure.is_local(),
        "the request went out and the server answered, so a retry is not free"
    );
    // Emitted after `Head`, which is what makes the failure legible: you can see the 200 and the
    // Content-Length that caused it.
    assert!(
        events.iter().any(|event| matches!(event, Event::Head { .. })),
        "the head should still have been reported: {events:?}"
    );
}

/// Localhost tests never exercise DNS, rustls, ALPN, or content decoding. This one
/// does — and is `#[ignore]`d so CI never depends on the internet.
///
/// Run with: `cargo test -p zuno-core --test engine -- --ignored`
#[test]
#[ignore = "requires network access"]
fn a_real_https_request_works_end_to_end() {
    let engine = Engine::new().expect("engine");

    let mut spec = spec_for("https://example.com/".to_string());
    spec.settings.timeout = Some(Duration::from_secs(20));
    spec.headers = vec![Header::new("Accept", "text/html")];

    let (_, events) = engine.send(spec);
    let events = drain(&events);

    if let Some(Event::Failed { error, .. }) = events.iter().find(|e| e.is_terminal()) {
        panic!("live request failed: {error}");
    }

    let Some(Event::Done { response, .. }) = events.iter().find(|e| e.is_terminal()) else {
        panic!("no terminal event: {events:?}");
    };

    assert_eq!(response.status, 200);
    assert!(!response.body.is_empty(), "empty body over TLS");
    assert!(
        response.timing.ttfb > Duration::ZERO,
        "TTFB should be measurable over a real network"
    );
    eprintln!(
        "live: {} {} · {} · ttfb {:?} · total {:?} · {} bytes decoded",
        response.status,
        response.status_text,
        response.version.as_str(),
        response.timing.ttfb,
        response.timing.total,
        response.size.decoded
    );
}

#[test]
fn the_connection_pool_survives_a_resend() {
    // Two sends with identical settings must reuse one client — that reuse is what
    // makes hitting Send twice feel instant.
    let engine = Engine::new().expect("engine");

    for _ in 0..2 {
        let (base, server) = serve_once(OK_JSON);
        let (_, events) = engine.send(spec_for(format!("{base}/ping")));
        let events = drain(&events);

        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Done { .. })),
            "resend did not complete: {events:?}"
        );
        server.join().expect("server thread");
    }
}

/// A server that hands out a cookie on every response, and reports what it received.
///
/// **`Connection: close` is load-bearing, not politeness.** Without it the response is
/// HTTP/1.1 keep-alive, so reqwest pools the socket and the second request *may* reuse it —
/// leaving this server blocked in `accept` for a connection that never comes. That hung a CI
/// run for six hours until GitHub killed it, and never reproduced locally, because whether
/// the client notices the server's FIN before sending again is a race that only loses on a
/// slow, loaded runner. Closing each connection makes one request mean one connection.
///
/// Note this makes the cookie assertion *stronger*: the replay now has to survive a new TCP
/// connection rather than riding a single one.
///
/// Every wait is bounded for the same reason. A test that assumes a connection count the
/// client doesn't promise should fail in seconds with a message, not hang until a scheduler
/// gives up on it.
fn serve_twice_setting_a_cookie() -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let Some(mut stream) = accept_before(&listener, SERVER_DEADLINE) else {
                // Returning short makes the assertion fail with a useful message; panicking
                // here would only surface as a poisoned `join`.
                return seen;
            };
            seen.push(read_request(&mut stream));
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: application/json\r\n\
                  Set-Cookie: session=abc123; Path=/\r\n\
                  Content-Length: 2\r\n\
                  Connection: close\r\n\
                  \r\n\
                  {}",
            );
            let _ = stream.flush();
        }
        seen
    });

    (format!("http://{addr}"), handle)
}

/// How long a test server waits for something it expects. Generous enough for a loaded CI
/// runner, short enough that a wrong assumption fails rather than hangs.
const SERVER_DEADLINE: Duration = Duration::from_secs(20);

/// `TcpListener::accept` with a deadline.
///
/// `std` has no timeout for accept, so this polls a non-blocking listener. The returned
/// stream is put back into blocking mode, since `read_request` expects that.
fn accept_before(listener: &TcpListener, within: Duration) -> Option<TcpStream> {
    listener.set_nonblocking(true).expect("nonblocking");
    let deadline = std::time::Instant::now() + within;

    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break None,
        }
    }?;

    listener.set_nonblocking(false).expect("blocking");
    stream.set_nonblocking(false).expect("blocking");
    // Bounds the *other* unbounded wait: a connection the client opens and then abandons
    // would otherwise block `read_request` forever.
    stream
        .set_read_timeout(Some(within))
        .expect("read timeout");
    Some(stream)
}

#[test]
fn a_cookie_from_one_response_is_replayed_on_the_next_request() {
    // The behaviour that is on by default and invisible in the UI: consecutive requests
    // are *not* independent. Asserted here so the settings panel's indicator is describing
    // something real rather than something assumed.
    let engine = Engine::new().expect("engine");
    let (base, server) = serve_twice_setting_a_cookie();

    let (_, first) = engine.send(spec_for(format!("{base}/login")));
    drain(&first);
    let (_, second) = engine.send(spec_for(format!("{base}/me")));
    drain(&second);

    let seen = server.join().expect("server thread");
    assert!(!seen[0].to_lowercase().contains("cookie:"), "first: {}", seen[0]);
    assert!(
        seen[1].to_lowercase().contains("cookie: session=abc123"),
        "the second request should carry the first's cookie, got:\n{}",
        seen[1]
    );
}

#[test]
fn clearing_cookies_stops_them_being_replayed() {
    // Why `clear_cookies` exists at all: `cookie_store` is part of `ClientKey`, so toggling
    // it off and back on returns you to the same client with the same jar. Without this,
    // there would be no way to end a session from inside Zuno.
    let engine = Engine::new().expect("engine");
    let (base, server) = serve_twice_setting_a_cookie();

    let (_, first) = engine.send(spec_for(format!("{base}/login")));
    drain(&first);

    engine.clear_cookies();

    let (_, second) = engine.send(spec_for(format!("{base}/me")));
    drain(&second);

    let seen = server.join().expect("server thread");
    assert!(
        !seen[1].to_lowercase().contains("cookie:"),
        "the cookie survived a clear:\n{}",
        seen[1]
    );
}

#[test]
fn a_multipart_body_goes_out_with_a_boundary_and_both_part_kinds() {
    // The framing is what could silently be wrong: a boundary that doesn't match the header,
    // a file part without a filename, a missing terminator. Asserted against the bytes a
    // server actually received.
    let dir = std::env::temp_dir().join(format!("zuno-mp-wire-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let file = dir.join("avatar.png");
    std::fs::write(&file, b"PNGDATA").expect("write");

    let engine = Engine::new().expect("engine");
    let (base, server) = serve_once(OK_JSON);

    let mut spec = spec_for(format!("{base}/upload"));
    spec.method = Method::Post;
    spec.body = Body::Multipart(vec![
        MultipartField {
            enabled: true,
            name: "caption".into(),
            value: MultipartValue::Text("hello".into()),
        },
        MultipartField {
            enabled: true,
            name: "avatar".into(),
            value: MultipartValue::File(file.clone()),
        },
    ]);

    let (_, events) = engine.send(spec);
    let events = drain(&events);
    assert!(
        events.iter().any(|event| matches!(event, Event::Done { .. })),
        "the multipart send did not complete: {events:?}"
    );

    let request = server.join().expect("server thread");
    let lower = request.to_lowercase();

    // The generated boundary has to appear in the header *and* delimit the parts, or no
    // server can parse this.
    let boundary = lower
        .split("boundary=")
        .nth(1)
        .and_then(|rest| rest.split(['\r', '\n', ';']).next())
        .map(str::to_string)
        .expect("a boundary in the Content-Type");
    assert!(!boundary.is_empty(), "empty boundary:\n{request}");
    assert!(
        lower.contains("content-type: multipart/form-data"),
        "should declare multipart:\n{request}"
    );
    assert!(
        request.matches(boundary.as_str()).count() >= 3,
        "boundary should open both parts and close the body:\n{request}"
    );

    // A text part, and a file part carrying its filename.
    assert!(
        request.contains(r#"name="caption""#) && request.contains("hello"),
        "the text part is missing:\n{request}"
    );
    assert!(
        request.contains(r#"name="avatar""#) && request.contains(r#"filename="avatar.png""#),
        "the file part should carry its filename:\n{request}"
    );
    assert!(request.contains("PNGDATA"), "the file's bytes are missing:\n{request}");

    std::fs::remove_dir_all(&dir).ok();
}
