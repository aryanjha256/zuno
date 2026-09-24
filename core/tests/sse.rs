//! Server-sent events, end to end over a real socket.
//!
//! `sse.rs`'s unit tests cover the framing. These cover the two things only the engine can
//! answer: that a `text/event-stream` response becomes a *session* rather than a body, and that
//! it is allowed to outlive the timeout — which is the whole reason the timeout's meaning
//! changed, and the bug that killed a GraphQL subscription at 30 seconds.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use zuno_core::engine::{Direction, Engine, Event, Frame};
use zuno_core::request::{RequestSpec, RequestSettings};

const DEADLINE: Duration = Duration::from_secs(15);

/// `accept` with a deadline.
///
/// **std has no timeout for accept, and a test server that waits forever hangs the runner
/// instead of failing it** — the rule `tests/engine.rs` carries after that cost six hours of CI,
/// and the one an app test broke the moment a reconnect it was waiting for stopped happening.
/// Every `accept` in this file goes through here.
fn accept_before(listener: &TcpListener, within: Duration) -> Option<TcpStream> {
    listener.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + within;

    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break None,
        }
    }?;

    listener.set_nonblocking(false).expect("blocking");
    stream.set_nonblocking(false).expect("blocking");
    stream.set_read_timeout(Some(within)).expect("read timeout");
    Some(stream)
}

/// The request head, as text, for asserting on what the client asked for.
fn read_request_head(stream: &mut TcpStream) -> String {
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&seen).into_owned()
}

fn read_request(stream: &mut TcpStream) {
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
}

fn head(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream; charset=utf-8\r\n\
          Cache-Control: no-cache\r\n\
          Connection: close\r\n\r\n",
    )?;
    stream.flush()
}

fn wait_for<T>(
    events: &async_channel::Receiver<Event>,
    seen: &mut Vec<String>,
    what: &str,
    mut want: impl FnMut(&Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}; saw {seen:?}");
        match events.recv_blocking() {
            Ok(event) => {
                seen.push(format!("{event:?}").chars().take(90).collect());
                if let Some(found) = want(&event) {
                    return found;
                }
            }
            Err(_) => panic!("the stream closed before {what}; saw {seen:?}"),
        }
    }
}

fn spec(url: String, timeout: Option<Duration>) -> RequestSpec {
    let mut spec = RequestSpec::default();
    spec.url = url;
    spec.settings = RequestSettings {
        timeout,
        ..RequestSettings::default()
    };
    spec
}

/// **An event stream becomes a transcript, and keep-alives do not become rows.**
///
/// The comment line is the half worth asserting end to end: servers send `:` every few seconds
/// to hold the connection open, and a parser that dispatched them would fill a subscription's
/// transcript with blank rows that look like the server saying nothing repeatedly.
#[test]
fn an_event_stream_arrives_as_frames_rather_than_a_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        let mut stream = accept_before(&listener, DEADLINE).expect("a connection");
        read_request(&mut stream);
        head(&mut stream).expect("head");
        let _ = stream.write_all(b": keep-alive\n\n");
        let _ = stream.write_all(b"event: tick\nid: 7\ndata: {\"n\": 1}\n\n");
        let _ = stream.write_all(b"data: no name\n\n");
        let _ = stream.flush();
        // Closing is how an SSE stream ends. The client must treat that as the end rather than
        // coming back for more — a completed subscription restarting is the bug this pins.
    });

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec(
        format!("http://127.0.0.1:{port}/events"),
        Some(Duration::from_secs(10)),
    ));

    let mut seen = Vec::new();
    let status = wait_for(&events, &mut seen, "the stream to open", |event| match event {
        Event::Opened { status, .. } => Some(status.clone()),
        Event::Failed { error, .. } => panic!("failed: {error}"),
        _ => None,
    });
    assert_eq!(
        status.map(|(code, _)| code),
        Some(200),
        "an SSE response is an ordinary 200"
    );

    let named = wait_for(&events, &mut seen, "the named event", |event| match event {
        Event::Frame {
            direction: Direction::Received,
            frame: Frame::Event { name, id, data },
            ..
        } => Some((name.clone(), id.clone(), data.clone())),
        Event::Failed { error, .. } => panic!("failed: {error}"),
        _ => None,
    });
    assert_eq!(named.0.as_deref(), Some("tick"), "the event name has to survive");
    assert_eq!(named.1.as_deref(), Some("7"));
    assert_eq!(named.2, "{\"n\": 1}");

    wait_for(&events, &mut seen, "the unnamed event", |event| match event {
        Event::Frame {
            frame: Frame::Event { name: None, data, .. },
            ..
        } if data == "no name" => Some(()),
        _ => None,
    });

    wait_for(&events, &mut seen, "the close", |event| match event {
        Event::Closed { .. } => Some(()),
        Event::Failed { error, .. } => panic!("ending a stream is a close, not a failure: {error}"),
        _ => None,
    });

    assert_eq!(
        seen.iter().filter(|line| line.contains("Frame")).count(),
        2,
        "the keep-alive comment must not have become a row: {seen:?}"
    );

    // **And it did not come back.** A clean close is the end of an SSE stream; reconnecting
    // there restarted every completed graphql-sse subscription.
    assert!(
        !seen.iter().any(|line| line.contains("Reconnecting")),
        "a stream the server closed must not be retried: {seen:?}"
    );

    server.join().expect("server thread");
}

/// **Disconnect stops a stream**, which is not the same question as stopping a socket.
///
/// A WebSocket job is spawned with a way in because its kind promises a session. An HTTP request
/// becomes one only when the server answers `text/event-stream` — long after the job exists — so
/// it used to be spawned with `outbound: None` and `Engine::close` routed the request into
/// nothing: the status bar said "Disconnecting" and the events kept arriving. Asserted on the
/// server seeing the connection go away, because a client that merely stops *reporting* frames
/// and a client that actually hung up look identical from inside the app.
#[test]
fn disconnecting_a_stream_actually_stops_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (gone_tx, gone_rx) = std::sync::mpsc::channel();

    let server = std::thread::spawn(move || {
        let mut stream = accept_before(&listener, DEADLINE).expect("a connection");
        read_request(&mut stream);
        head(&mut stream).expect("head");
        // Write until the peer is gone. A dropped connection surfaces as a failed write.
        //
        // **Bounded**, because an unbounded server loop is how a test hangs a runner rather than
        // failing it — `tests/engine.rs` carries the same rule after that cost six hours of CI.
        // Running out sends `0`, which fails the assertion below with a message instead.
        const LIMIT: usize = 500;
        for n in 1..=LIMIT {
            if stream.write_all(format!("data: {n}\n\n").as_bytes()).is_err()
                || stream.flush().is_err()
            {
                let _ = gone_tx.send(n);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = gone_tx.send(0);
    });

    let engine = Engine::new().expect("engine");
    let (job, events) = engine.send(spec(
        format!("http://127.0.0.1:{port}/events"),
        Some(Duration::from_secs(10)),
    ));

    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the stream to start", |event| match event {
        Event::Frame { .. } => Some(()),
        Event::Failed { error, .. } => panic!("failed: {error}"),
        _ => None,
    });

    engine.close(job);

    wait_for(&events, &mut seen, "the close", |event| match event {
        Event::Closed { .. } => Some(()),
        _ => None,
    });

    // The load-bearing assertion is the `Closed` above — a `close` that routed nowhere never
    // produces one. This is the server's half: it stopped because the peer went away, not
    // because it ran out of things to say.
    let wrote = gone_rx
        .recv_timeout(DEADLINE)
        .expect("the server must notice the connection go away");
    assert_ne!(
        wrote, 0,
        "the server exhausted its own budget instead of being disconnected"
    );

    server.join().expect("server thread");
}

/// **A stream outlives the timeout, as long as it keeps speaking.**
///
/// This is the semantics change the feature needed. `timeout` used to be reqwest's deadline on
/// the whole exchange, body included — which a stream can never meet, so a subscription was
/// killed at whatever the setting said with no close and nothing on screen saying why. It now
/// means "answer within N, and do not go silent for N": the head is deadlined here, each read
/// is deadlined by `read_timeout`, and elapsed total is nobody's business.
///
/// The server below takes four times the timeout to finish while never pausing for more than a
/// fifth of it.
#[test]
fn a_stream_that_keeps_speaking_outlives_the_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let timeout = Duration::from_millis(500);

    let server = std::thread::spawn(move || {
        let mut stream = accept_before(&listener, DEADLINE).expect("a connection");
        read_request(&mut stream);
        head(&mut stream).expect("head");
        for n in 0..20 {
            if stream.write_all(format!("data: {n}\n\n").as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    let engine = Engine::new().expect("engine");
    let started = Instant::now();
    let (_job, events) = engine.send(spec(
        format!("http://127.0.0.1:{port}/events"),
        Some(timeout),
    ));

    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the last event", |event| match event {
        Event::Frame {
            frame: Frame::Event { data, .. },
            ..
        } if data == "19" => Some(()),
        Event::Failed { error, .. } => {
            panic!("a stream that keeps speaking must not time out: {error}")
        }
        _ => None,
    });

    assert!(
        started.elapsed() > timeout * 3,
        "the test is only meaningful if it ran well past the timeout, took {:?}",
        started.elapsed()
    );

    server.join().expect("server thread");
}

/// **A broken connection resumes; a closed one does not.**
///
/// The pair matters more than either half. `an_event_stream_arrives_as_frames_rather_than_a_body`
/// pins that a server which closes is finished — the bug where a completed graphql-sse
/// subscription restarted forever. This pins the other side: a connection that *fails* mid-stream
/// is picked back up, and the retry carries `Last-Event-ID` so the server can replay the gap.
///
/// The break is an idle timeout rather than a reset, because that is reproducible: the server
/// simply stops writing without closing, and the client's read deadline expires.
#[test]
fn a_broken_connection_is_resumed_from_the_last_id() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (head_tx, head_rx) = std::sync::mpsc::channel();

    let server = std::thread::spawn(move || {
        let mut first = accept_before(&listener, DEADLINE).expect("a connection");
        read_request(&mut first);
        head(&mut first).expect("head");
        let _ = first.write_all(b"id: 5\ndata: before\n\n");
        let _ = first.flush();

        // Neither write nor close: the client's idle deadline is what ends this.
        let mut second = accept_before(&listener, DEADLINE).expect("the client must come back");
        let asked = read_request_head(&mut second);
        head(&mut second).expect("head");
        let _ = second.write_all(b"data: after\n\n");
        let _ = second.flush();
        let _ = head_tx.send(asked);
        // Hold the first connection open until the end, so it is the timeout that broke it.
        drop(first);
    });

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec(
        format!("http://127.0.0.1:{port}/events"),
        Some(Duration::from_millis(300)),
    ));

    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the first event", |event| match event {
        Event::Frame { frame: Frame::Event { data, .. }, .. } if data == "before" => Some(()),
        Event::Failed { error, .. } => panic!("failed: {error}"),
        _ => None,
    });

    wait_for(&events, &mut seen, "the resumed event", |event| match event {
        Event::Frame { frame: Frame::Event { data, .. }, .. } if data == "after" => Some(()),
        Event::Failed { error, .. } => panic!("a broken stream must resume, not fail: {error}"),
        _ => None,
    });

    assert!(
        seen.iter().any(|line| line.contains("Reconnecting")),
        "the drop has to be announced, or the gap is invisible: {seen:?}"
    );

    let asked = head_rx.recv_timeout(DEADLINE).expect("the retry's head");
    assert!(
        asked.to_lowercase().contains("last-event-id: 5"),
        "the retry must ask the server to replay from where it stopped:\n{asked}"
    );

    server.join().expect("server thread");
}

/// **A `204` is the most orderly end SSE has, so it must not be reported as a failure.**
///
/// The assertion is on `Closed` carrying an *empty* reason, which looks like a detail and is
/// the whole bug. `session_line` reads a `Closed` with no code but a non-empty reason as a
/// failure and paints it red — that is where `Event::Failed` puts its error text — so wording
/// this close as "the server ended the stream" made a server politely saying *do not come back*
/// render as `failed — the server ended the stream`. Nothing else on screen contradicts it, so
/// a completed subscription simply looked broken.
///
/// Worth pinning here rather than at the pane, because the pane returns a `Div` and gpui's
/// headless platform cannot read a colour off one. The reason string is the observable half.
#[test]
fn a_server_ending_the_stream_with_204_is_not_a_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        let mut first = accept_before(&listener, DEADLINE).expect("a connection");
        read_request(&mut first);
        head(&mut first).expect("head");
        let _ = first.write_all(b"id: 9\ndata: last\n\n");
        let _ = first.flush();
        // Stop writing without closing: the idle deadline breaks it, which is a *drop* and so
        // the one case that reconnects. A clean close would end the stream before the 204.
        let mut second = accept_before(&listener, DEADLINE).expect("the client must come back");
        read_request(&mut second);
        let _ = second.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        let _ = second.flush();
        drop(first);
    });

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec(
        format!("http://127.0.0.1:{port}/events"),
        Some(Duration::from_millis(300)),
    ));

    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the event", |event| match event {
        Event::Frame { frame: Frame::Event { data, .. }, .. } if data == "last" => Some(()),
        Event::Failed { error, .. } => panic!("failed: {error}"),
        _ => None,
    });

    let reason = wait_for(&events, &mut seen, "the close", |event| match event {
        Event::Closed { reason, code, .. } => Some((reason.clone(), *code)),
        Event::Failed { error, .. } => {
            panic!("a 204 is how SSE says it is over, not a failure: {error}")
        }
        _ => None,
    });

    assert_eq!(
        reason,
        (String::new(), None),
        "a 204 close must carry no reason, or the pane paints it as a failure"
    );

    server.join().expect("server thread");
}
