//! WebSocket, end to end over a real socket.
//!
//! The server here is `tungstenite`'s own synchronous half, which is the point: it performs a
//! genuine RFC 6455 handshake and genuine framing, so a client that only *looks* right does not
//! pass. A hand-written fake would agree with whatever this code happened to send — which is
//! exactly the shape of assertion CLAUDE.md's Lessons section is about.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use tokio_tungstenite::tungstenite;
use tungstenite::Message;
use zuno_core::engine::{Direction, Engine, Event, Frame};
use zuno_core::request::{RequestKind, RequestSpec, WebSocketRequest};

/// Every wait in this file is bounded. A WebSocket test that can hang is a CI outage — the
/// keep-alive incident in `tests/engine.rs` cost six hours of a runner once.
const DEADLINE: Duration = Duration::from_secs(10);

/// Pull events until `want` returns `Some`, or give up with what was seen.
fn wait_for<T>(
    events: &async_channel::Receiver<Event>,
    seen: &mut Vec<String>,
    what: &str,
    mut want: impl FnMut(&Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for {what}; saw {seen:?}");
        match events.recv_blocking() {
            Ok(event) => {
                seen.push(describe(&event));
                if let Some(found) = want(&event) {
                    return found;
                }
            }
            Err(_) => panic!("the event stream closed before {what}; saw {seen:?}"),
        }
    }
}

fn describe(event: &Event) -> String {
    match event {
        Event::Started { .. } => "Started".into(),
        Event::Opened { status, protocol, .. } => format!("Opened({status}, {protocol:?})"),
        Event::Frame { direction, frame, .. } => format!("Frame({direction:?}, {frame:?})"),
        Event::Closed { code, reason, .. } => format!("Closed({code:?}, {reason:?})"),
        Event::Failed { error, .. } => format!("Failed({error})"),
        other => format!("{other:?}"),
    }
}

fn socket_spec(url: String, socket: WebSocketRequest) -> RequestSpec {
    let mut spec = RequestSpec::default();
    spec.url = url;
    spec.kind = RequestKind::WebSocket(socket);
    spec
}

/// **The whole loop: handshake, both directions, close.**
///
/// Asserted on what the *server* received as well as on what the client reported, because a
/// client that never sent the frame and a server that never got it produce the same silence.
#[test]
fn a_socket_opens_carries_frames_both_ways_and_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");

        // `accept_hdr` is what makes the *request* observable — the upgrade headers and the
        // subprotocol offer are only provable from this side.
        let mut ws = tungstenite::accept_hdr(
            stream,
            |request: &tungstenite::handshake::server::Request, response| {
                let headers = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        format!("{}: {}", name.as_str(), value.to_str().unwrap_or_default())
                    })
                    .collect::<Vec<_>>();
                let _ = seen_tx.send(headers);
                Ok(response)
            },
        )
        .expect("handshake");

        let from_client = ws.read().expect("a frame from the client");
        ws.send(Message::Text(
            format!("echo:{}", from_client.to_text().expect("text")).into(),
        ))
        .expect("echo");
        ws.close(None).expect("close");
        // Drain until the peer's Close comes back, so the handshake completes rather than the
        // socket being torn down under it.
        while ws.read().is_ok() {}
    });

    let engine = Engine::new().expect("engine");
    let (job, events) = engine.send(socket_spec(
        format!("ws://127.0.0.1:{port}/chat"),
        WebSocketRequest {
            subprotocols: vec!["graphql-transport-ws".to_string()],
            messages: Vec::new(),
        },
    ));

    let mut seen = Vec::new();

    let status = wait_for(&events, &mut seen, "the socket to open", |event| match event {
        Event::Opened { status, .. } => Some(*status),
        Event::Failed { error, .. } => panic!("the handshake failed: {error}"),
        _ => None,
    });
    assert_eq!(status, 101);

    engine.send_frame(job, Frame::Text("hello".to_string()));

    let echoed = wait_for(&events, &mut seen, "the server's echo", |event| match event {
        Event::Frame {
            direction: Direction::Received,
            frame: Frame::Text(text),
            ..
        } => Some(text.clone()),
        Event::Failed { error, .. } => panic!("the socket failed: {error}"),
        _ => None,
    });
    assert_eq!(
        echoed, "echo:hello",
        "the server must have received exactly what was typed"
    );

    // The sent frame is reported too, and only after the write succeeded — a transcript that
    // shows a frame the socket refused is worse than one that shows nothing.
    assert!(
        seen.iter().any(|event| event.contains("Sent")),
        "the frame this side sent must appear in the transcript; saw {seen:?}"
    );

    wait_for(&events, &mut seen, "the close", |event| match event {
        Event::Closed { .. } => Some(()),
        _ => None,
    });

    let headers = seen_rx.recv_timeout(DEADLINE).expect("the handshake headers");
    let joined = headers.join("\n").to_lowercase();
    assert!(
        joined.contains("upgrade: websocket") && joined.contains("connection: upgrade"),
        "the upgrade headers must reach the server:\n{joined}"
    );
    assert!(
        joined.contains("sec-websocket-protocol: graphql-transport-ws"),
        "the subprotocol offer must reach the server:\n{joined}"
    );

    server.join().expect("server thread");
}

/// **The accept key is checked, and that is a security property rather than a nicety.**
///
/// RFC 6455 requires the client to fail the connection when `Sec-WebSocket-Accept` does not
/// match the key it sent. Without it, anything that can answer 101 — a confused proxy, a
/// cache, an attacker who can reach the port — is talking to you as if it understood the
/// handshake. The server here answers a *well-formed* 101 with a wrong accept value, which is
/// precisely the case a status check alone would wave through.
#[test]
fn a_wrong_accept_key_is_refused() {
    use std::io::{Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        let _ = stream.write_all(
            b"HTTP/1.1 101 Switching Protocols\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Accept: totally-wrong\r\n\r\n",
        );
        let _ = stream.flush();
    });

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(socket_spec(
        format!("ws://127.0.0.1:{port}/"),
        WebSocketRequest::default(),
    ));

    let mut seen = Vec::new();
    let reason = wait_for(&events, &mut seen, "the refusal", |event| match event {
        Event::Failed { error, .. } => Some(error.to_string()),
        Event::Opened { .. } => panic!("a wrong accept key must never open the socket"),
        _ => None,
    });
    assert!(
        reason.to_lowercase().contains("accept"),
        "the error has to name what was wrong, got {reason:?}"
    );

    server.join().expect("server thread");
}

/// An ordinary endpoint answering an upgrade says so in words, not in a framing error.
#[test]
fn a_plain_http_endpoint_is_reported_as_not_a_websocket() {
    use std::io::{Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
        );
        let _ = stream.flush();
    });

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(socket_spec(
        format!("ws://127.0.0.1:{port}/"),
        WebSocketRequest::default(),
    ));

    let mut seen = Vec::new();
    let reason = wait_for(&events, &mut seen, "the refusal", |event| match event {
        Event::Failed { error, .. } => Some(error.to_string()),
        _ => None,
    });
    assert!(
        reason.contains("200") && reason.contains("101"),
        "the message has to say what happened instead, got {reason:?}"
    );

    server.join().expect("server thread");
}

/// **A real `wss://` endpoint, over TLS.** `#[ignore]`d so CI never depends on the network —
/// the same rule `tests/engine.rs` follows.
///
/// This covers the half the local tests cannot: a plaintext `ws://` never negotiates ALPN, so
/// every test above proves the framing and none of them prove that a TLS connection still
/// arrives as HTTP/1.1. There is no 101 in HTTP/2.
#[test]
#[ignore]
fn a_real_wss_endpoint_opens() {
    let engine = Engine::new().expect("engine");
    let (job, events) = engine.send(socket_spec(
        std::env::var("ZUNO_WS_URL").unwrap_or_else(|_| "wss://echo.websocket.org".to_string()),
        WebSocketRequest::default(),
    ));

    let mut seen = Vec::new();
    let status = wait_for(&events, &mut seen, "the socket to open", |event| match event {
        Event::Opened { status, .. } => Some(*status),
        Event::Failed { error, .. } => panic!("handshake failed: {error}"),
        _ => None,
    });
    assert_eq!(status, 101);

    engine.send_frame(job, Frame::Text("hello".to_string()));
    // Skipped rather than asserted on: a public echo server is entitled to greet you first,
    // and `echo.websocket.org` does. What is being proven here is the TLS handshake and the
    // round trip, not the server's manners.
    let echoed = wait_for(&events, &mut seen, "the echo", |event| match event {
        Event::Frame {
            direction: Direction::Received,
            frame: Frame::Text(text),
            ..
        } if text == "hello" => Some(text.clone()),
        Event::Failed { error, .. } => panic!("socket failed: {error}"),
        _ => None,
    });
    assert_eq!(echoed, "hello");
    println!("wss:// works — saw {seen:?}");
}

/// **A GraphQL subscription opens a socket and speaks graphql-transport-ws**, with nothing
/// configured — `Auto` reads the document, sees `subscription`, and routes it.
///
/// The server here plays the protocol properly: it checks the subprotocol offer, answers
/// `connection_init` with `connection_ack`, waits for `subscribe`, and only then sends data.
/// A client that pipelined the subscribe, offered the wrong subprotocol string, or handed the
/// raw envelopes to the transcript would all fail different assertions here.
#[test]
fn a_graphql_subscription_rides_a_socket_and_unwraps_its_payloads() {
    use std::sync::mpsc;
    use zuno_core::{GraphQlRequest, GraphQlTransport, RequestKind};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (offered_tx, offered_rx) = mpsc::channel();
    let (subscribed_tx, subscribed_rx) = mpsc::channel();

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");

        let mut ws = tungstenite::accept_hdr(
            stream,
            |request: &tungstenite::handshake::server::Request, mut response: tungstenite::handshake::server::Response| {
                let offered = request
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let _ = offered_tx.send(offered);
                response.headers_mut().insert(
                    "sec-websocket-protocol",
                    "graphql-transport-ws".parse().expect("header"),
                );
                Ok(response)
            },
        )
        .expect("handshake");

        // `connection_init` must arrive before anything else.
        let init = ws.read().expect("connection_init");
        assert!(
            init.to_text().unwrap_or_default().contains("connection_init"),
            "the client must open with connection_init, got {init:?}"
        );
        ws.send(tungstenite::Message::Text(
            r#"{"type":"connection_ack"}"#.into(),
        ))
        .expect("ack");

        let subscribe = ws.read().expect("subscribe");
        let _ = subscribed_tx.send(subscribe.to_text().unwrap_or_default().to_string());

        for n in 0..2 {
            ws.send(tungstenite::Message::Text(
                format!(r#"{{"id":"1","type":"next","payload":{{"data":{{"n":{n}}}}}}}"#).into(),
            ))
            .expect("next");
        }
        ws.send(tungstenite::Message::Text(
            r#"{"id":"1","type":"complete"}"#.into(),
        ))
        .expect("complete");
        while ws.read().is_ok() {}
    });

    let mut spec = RequestSpec::default();
    spec.url = format!("ws://127.0.0.1:{port}/graphql");
    spec.kind = RequestKind::GraphQl(GraphQlRequest {
        query: "subscription { greetings }".to_string(),
        transport: GraphQlTransport::Auto,
        ..GraphQlRequest::default()
    });

    assert!(
        spec.kind.is_session(),
        "Auto has to route a subscription to a socket before anything is sent"
    );

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec);

    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the socket to open", |event| match event {
        Event::Opened { .. } => Some(()),
        Event::Failed { error, .. } => panic!("the handshake failed: {error}"),
        _ => None,
    });

    let first = wait_for(&events, &mut seen, "the first payload", |event| match event {
        Event::Frame {
            frame: Frame::Text(text),
            ..
        } => Some(text.clone()),
        Event::Failed { error, .. } => panic!("the subscription failed: {error}"),
        _ => None,
    });
    assert_eq!(
        first, r#"{"data":{"n":0}}"#,
        "the transcript gets the payload, not the envelope around it"
    );

    wait_for(&events, &mut seen, "the complete", |event| match event {
        Event::Closed { .. } => Some(()),
        _ => None,
    });

    let offered = offered_rx.recv_timeout(DEADLINE).expect("the offer");
    assert!(
        offered.contains("graphql-transport-ws"),
        "the modern subprotocol must be offered — `graphql-ws` is the deprecated one. Got {offered:?}"
    );
    let subscribe = subscribed_rx.recv_timeout(DEADLINE).expect("the subscribe");
    assert!(
        subscribe.contains("subscription { greetings }"),
        "the document has to reach the server: {subscribe}"
    );

    server.join().expect("server thread");
}

/// **A peer that never answers the Close still ends the session.**
///
/// RFC 6455 makes closing an exchange, so the loop goes on reading after `stream.close(None)` —
/// dropping the socket there would reach the server as a reset. But "go on reading" was
/// unbounded: a server that neither answers the Close nor drops the connection left the task
/// alive and the strip reading `open` for the life of the process. There is no protocol event
/// that ends this; only a deadline does.
///
/// The server here is deliberately rude in the one way that reproduces it — it completes the
/// handshake and then never reads again, so tungstenite never gets the chance to auto-reply to
/// the Close. Reported as `Closed` rather than `Failed`: we asked to hang up and we have.
#[test]
fn a_close_the_peer_ignores_still_ends_the_session() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");
        let ws = tungstenite::accept(stream).expect("handshake");
        // **Never read again.** `read` is what makes tungstenite answer a Close, so calling it
        // even once would defeat the test. Hold the socket open instead, well past the grace.
        std::thread::sleep(DEADLINE);
        drop(ws);
    });

    let engine = Engine::new().expect("engine");
    let (job, events) = engine.send(socket_spec(
        format!("ws://127.0.0.1:{port}/"),
        WebSocketRequest::default(),
    ));

    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the socket to open", |event| {
        matches!(event, Event::Opened { .. }).then_some(())
    });

    engine.close(job);

    let started = Instant::now();
    wait_for(&events, &mut seen, "the close to give up", |event| match event {
        Event::Closed { .. } => Some(()),
        Event::Failed { error, .. } => {
            panic!("asking to close is not a failure: {error}")
        }
        _ => None,
    });

    // The grace is five seconds; anything under the file's whole deadline proves a bound
    // exists, and pinning the exact number here would make tuning it a test edit.
    assert!(
        started.elapsed() < DEADLINE,
        "the close must give up on its own, not wait for the peer forever"
    );

    server.join().expect("server thread");
}

/// **A graphql-transport-ws server that never acknowledges is a timeout, not an open session.**
///
/// The protocol says the client sends `connection_init` and may send nothing else until
/// `connection_ack` comes back. Nothing bounded that wait, so a server that accepts the socket
/// and then says nothing left the transcript reading `open` forever — and a protocol-layer auth
/// rejection, where the server holds the socket instead of closing it, looks exactly like this.
///
/// `settings.timeout` is the deadline rather than a constant of its own: it now means "answer
/// within N", and an ack is the answer.
#[test]
fn a_graphql_socket_that_never_acknowledges_times_out() {
    use zuno_core::{GraphQlRequest, GraphQlTransport, RequestKind};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");
        let mut ws = tungstenite::accept_hdr(
            stream,
            |_: &tungstenite::handshake::server::Request,
             mut response: tungstenite::handshake::server::Response| {
                response.headers_mut().insert(
                    "sec-websocket-protocol",
                    "graphql-transport-ws".parse().expect("header"),
                );
                Ok(response)
            },
        )
        .expect("handshake");
        // Read the `connection_init` and answer nothing — the case the deadline is for.
        let _ = ws.read();
        std::thread::sleep(Duration::from_secs(2));
    });

    let mut spec = RequestSpec::default();
    spec.url = format!("ws://127.0.0.1:{port}/graphql");
    spec.settings.timeout = Some(Duration::from_millis(300));
    spec.kind = RequestKind::GraphQl(GraphQlRequest {
        query: "subscription Greetings { greetings }".to_string(),
        transport: GraphQlTransport::WebSocket,
        ..GraphQlRequest::default()
    });

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec);

    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the handshake to time out", |event| {
        match event {
            Event::Failed { error, .. } => {
                assert!(
                    error.to_string().to_lowercase().contains("timed out")
                        || error.to_string().to_lowercase().contains("timeout"),
                    "an unacknowledged handshake must report as a timeout, not as {error}"
                );
                Some(())
            }
            Event::Frame { .. } => panic!("nothing may arrive before the ack"),
            _ => None,
        }
    });

    server.join().expect("server thread");
}
