//! A gRPC call, end to end over a real HTTP/2 socket.
//!
//! `grpc_trailers.rs` proved the transport in isolation; this drives the whole engine path —
//! compile a `.proto`, encode JSON into protobuf, send, read the trailers, decode the reply.
//!
//! **The server checks what it received.** It decodes the request message and answers with the
//! name it was sent, so a client that framed the body wrongly, encoded the wrong field, or sent
//! nothing at all fails here rather than agreeing with itself. That is the difference between
//! this and a fake that echoes whatever arrives.
//!
//! The three cases are the three answers a gRPC server can give, and two of them are *200 OK*:
//! success, a non-zero `grpc-status`, and no status trailer at all. Believing the HTTP status
//! line would report the second as a success, which is the failure worth a test.

use std::convert::Infallible;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use zuno_core::engine::{Direction, Engine, Event};
use zuno_core::request::{GrpcRequest, RequestKind, RequestSpec};

const DEADLINE: Duration = Duration::from_secs(10);

const GREETER: &str = r#"
    syntax = "proto3";
    package helloworld;
    message HelloRequest { string name = 1; }
    message HelloReply { string message = 1; }
    service Greeter {
      rpc SayHello (HelloRequest) returns (HelloReply);
      rpc StreamHellos (HelloRequest) returns (stream HelloReply);
      rpc SendHellos (stream HelloRequest) returns (HelloReply);
      rpc Chat (stream HelloRequest) returns (stream HelloReply);
    }
"#;

/// A scratch `.proto`, named per test and per process so parallel runs cannot collide.
fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zuno-grpc-e2e-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("greeter.proto");
    let mut file = std::fs::File::create(&path).expect("create");
    file.write_all(GREETER.as_bytes()).expect("write");
    path
}

/// What the server should answer with.
#[derive(Clone, Copy)]
enum Answer {
    /// A reply plus `grpc-status: 0`.
    Ok,
    /// A non-zero status and a percent-encoded message, with no reply body — which is exactly
    /// what a real server sends on an error, and it is still a 200.
    Status(u16),
    /// A 200 with a body and **no status trailer**. What a plain HTTP server or a confused
    /// proxy on the port looks like.
    NoTrailer,
    /// Several messages, then a status. `Some(code)` makes it a failure *after* delivery, which
    /// is the case a status line can never express.
    Stream(usize, Option<u16>),
    /// Count every message the *client* sent and answer once with the total — a real
    /// client-streaming server, and the only way to prove every message arrived.
    Count,
    /// Echo one reply per message received, then a clean status. Bidirectional.
    Echo,
    /// **A Trailers-Only response**: the status in the response *head*, no body, no trailers.
    /// What grpc-go and grpc-java send for an error raised before any message exists, and the
    /// one shape every other variant here agreed with the client by never sending.
    TrailersOnly(u16),
}

/// An h2 server that speaks enough gRPC to be worth asserting against.
///
/// Returns the port and a channel carrying the `name` field it decoded out of each request, so a
/// test can assert on what actually arrived rather than only on what came back.
fn serve(answer: Answer) -> (u16, std::sync::mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let _ = port_tx.send(listener.local_addr().expect("addr").port());

            let Ok((stream, _)) = listener.accept().await else {
                return;
            };

            let service = hyper::service::service_fn(move |request: http::Request<hyper::body::Incoming>| {
                let seen_tx = seen_tx.clone();
                async move {
                    let body = request
                        .into_body()
                        .collect()
                        .await
                        .map(|collected| collected.to_bytes())
                        .unwrap_or_default();

                    // **Decoded, not echoed.** Field 1 is a length-delimited string, so a
                    // correctly framed and encoded `HelloRequest` looks like
                    // `[0][len:4][0x0a][n][name]`. Reading it by hand keeps the server honest
                    // without giving it a protobuf library of its own.
                    let name = zuno_core::grpc::unframe(&body)
                        .first()
                        .and_then(|message| {
                            let rest = message.strip_prefix(&[0x0a])?;
                            let (len, rest) = rest.split_first()?;
                            let bytes = rest.get(..*len as usize)?;
                            std::str::from_utf8(bytes).ok().map(str::to_string)
                        })
                        .unwrap_or_default();
                    let _ = seen_tx.send(name.clone());

                    let mut frames: Vec<Result<hyper::body::Frame<Bytes>, Infallible>> = Vec::new();
                    let mut trailers = http::HeaderMap::new();

                    match answer {
                        Answer::Ok | Answer::NoTrailer => {
                            // A `HelloReply` whose `message` field echoes the name back.
                            let text = format!("hello {name}");
                            let mut message = vec![0x0a, text.len() as u8];
                            message.extend_from_slice(text.as_bytes());
                            frames.push(Ok(hyper::body::Frame::data(Bytes::from(
                                zuno_core::grpc::frame(&message),
                            ))));
                            if matches!(answer, Answer::Ok) {
                                trailers
                                    .insert("grpc-status", http::HeaderValue::from_static("0"));
                            }
                        }
                        Answer::Echo => {
                            for (n, message) in
                                zuno_core::grpc::unframe(&body).into_iter().enumerate()
                            {
                                let name = message
                                    .strip_prefix(&[0x0a])
                                    .and_then(|rest| {
                                        let (len, rest) = rest.split_first()?;
                                        std::str::from_utf8(rest.get(..*len as usize)?).ok()
                                    })
                                    .unwrap_or("?");
                                let text = format!("re{n}:{name}");
                                let mut reply = vec![0x0a, text.len() as u8];
                                reply.extend_from_slice(text.as_bytes());
                                frames.push(Ok(hyper::body::Frame::data(Bytes::from(
                                    zuno_core::grpc::frame(&reply),
                                ))));
                            }
                            trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                        }
                        Answer::Count => {
                            let total = zuno_core::grpc::unframe(&body).len();
                            let text = format!("got {total}");
                            let mut message = vec![0x0a, text.len() as u8];
                            message.extend_from_slice(text.as_bytes());
                            frames.push(Ok(hyper::body::Frame::data(Bytes::from(
                                zuno_core::grpc::frame(&message),
                            ))));
                            trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                        }
                        Answer::Stream(count, ending) => {
                            for n in 0..count {
                                let text = format!("hello {n}");
                                let mut message = vec![0x0a, text.len() as u8];
                                message.extend_from_slice(text.as_bytes());
                                frames.push(Ok(hyper::body::Frame::data(Bytes::from(
                                    zuno_core::grpc::frame(&message),
                                ))));
                            }
                            trailers.insert(
                                "grpc-status",
                                http::HeaderValue::from_str(&ending.unwrap_or(0).to_string())
                                    .expect("status"),
                            );
                        }
                        Answer::Status(code) => {
                            trailers.insert(
                                "grpc-status",
                                http::HeaderValue::from_str(&code.to_string()).expect("status"),
                            );
                            // Percent-encoded, as the spec requires for anything outside
                            // printable ASCII — here a newline, so the decoding is exercised.
                            trailers.insert(
                                "grpc-message",
                                http::HeaderValue::from_static("no%20such%20greeter%0Atry%20again"),
                            );
                        }
                        Answer::TrailersOnly(_) => {}
                    }

                    if !trailers.is_empty() {
                        frames.push(Ok(hyper::body::Frame::trailers(trailers)));
                    }

                    let mut response = http::Response::builder()
                        .status(200)
                        .header("content-type", "application/grpc");
                    if let Answer::TrailersOnly(code) = answer {
                        response = response
                            .header("grpc-status", code.to_string())
                            .header("grpc-message", "method%20not%20found");
                    }

                    Ok::<_, Infallible>(
                        response
                            .body(http_body_util::StreamBody::new(
                                futures_util::stream::iter(frames),
                            ))
                            .expect("response"),
                    )
                }
            });

            let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
        });
    });

    let port = port_rx
        .recv_timeout(DEADLINE)
        .expect("the server must bind");
    (port, seen_rx, handle)
}

fn spec(port: u16, proto: &PathBuf, message: &str) -> RequestSpec {
    call_spec(port, proto, message, "SayHello", false)
}

fn call_spec(
    port: u16,
    proto: &PathBuf,
    message: &str,
    method: &str,
    server_streaming: bool,
) -> RequestSpec {
    client_spec(port, proto, message, method, false, server_streaming)
}

fn client_spec(
    port: u16,
    proto: &PathBuf,
    message: &str,
    method: &str,
    client_streaming: bool,
    server_streaming: bool,
) -> RequestSpec {
    let mut spec = RequestSpec::default();
    spec.url = format!("http://127.0.0.1:{port}");
    spec.kind = RequestKind::Grpc(GrpcRequest {
        proto: proto.display().to_string(),
        service: "helloworld.Greeter".to_string(),
        method: method.to_string(),
        message: message.to_string(),
        client_streaming,
        server_streaming,
    });
    spec
}

fn wait_for<T>(
    events: &async_channel::Receiver<Event>,
    what: &str,
    mut want: impl FnMut(&Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + DEADLINE;
    let mut seen = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; saw {seen:?}"
        );
        match events.recv_blocking() {
            Ok(event) => {
                seen.push(format!("{event:?}").chars().take(160).collect::<String>());
                if let Some(found) = want(&event) {
                    return found;
                }
            }
            Err(_) => panic!("the event stream closed before {what}; saw {seen:?}"),
        }
    }
}

/// **The whole loop.** JSON in, protobuf on the wire, protobuf back, JSON in the viewer.
#[test]
fn a_unary_call_encodes_sends_and_decodes() {
    let proto = fixture("ok");
    let (port, seen, server) = serve(Answer::Ok);

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec(port, &proto, r#"{"name": "zuno"}"#));

    let body = wait_for(&events, "the reply", |event| match event {
        Event::Done { response, .. } => Some(response.body.clone()),
        Event::Failed { error, .. } => panic!("the call must succeed: {error}"),
        _ => None,
    });

    // **What the server received**, which is the half a self-agreeing fake could not check.
    assert_eq!(
        seen.recv_timeout(DEADLINE).expect("the decoded request"),
        "zuno",
        "the request has to reach the server correctly framed and encoded"
    );

    let text = String::from_utf8(body.to_vec()).expect("the body is decoded JSON");
    assert!(
        text.contains("\"message\"") && text.contains("hello zuno"),
        "the reply must arrive as readable JSON: {text}"
    );

    // **Drop the engine before joining the server.** `serve_connection` returns when the
    // connection closes, and the engine caches its client — so while it is alive the h2
    // connection is pooled, the server has nothing to finish, and the join sits there. Measured:
    // 90 seconds per test instead of one. The same unbounded wait an unbounded `accept` is, one
    // layer up, and it reads as a slow test rather than as a bug.
    drop(engine);
    server.join().expect("server thread");
}

/// **A failed gRPC call is still a 200**, and this is the assertion that catches believing it.
#[test]
fn a_non_zero_status_trailer_is_a_failure_despite_the_200() {
    let proto = fixture("status");
    let (port, _seen, server) = serve(Answer::Status(5));

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec(port, &proto, r#"{"name": "ghost"}"#));

    let reason = wait_for(&events, "the failure", |event| match event {
        Event::Failed { error, .. } => Some(error.to_string()),
        Event::Done { response, .. } => panic!(
            "a grpc-status of 5 is a failure, not a {} response",
            response.status
        ),
        _ => None,
    });

    assert!(
        reason.contains("NOT_FOUND"),
        "the code has to read as its name, or a number is all you get: {reason}"
    );
    assert!(
        reason.contains("no such greeter"),
        "the server's own message is the useful half: {reason}"
    );
    assert!(
        reason.contains('\n'),
        "grpc-message is percent-encoded and has to be decoded: {reason:?}"
    );

    // **Drop the engine before joining the server.** `serve_connection` returns when the
    // connection closes, and the engine caches its client — so while it is alive the h2
    // connection is pooled, the server has nothing to finish, and the join sits there. Measured:
    // 90 seconds per test instead of one. The same unbounded wait an unbounded `accept` is, one
    // layer up, and it reads as a slow test rather than as a bug.
    drop(engine);
    server.join().expect("server thread");
}

/// **A 200 with no status trailer is not a gRPC reply**, and saying so beats a decode error.
///
/// This is what a plain HTTP server, or a proxy that swallowed the trailers, looks like from
/// here — a likely misconfiguration, and one where the useful answer names the cause.
#[test]
fn a_reply_with_no_status_trailer_is_reported_as_not_grpc() {
    let proto = fixture("notrailer");
    let (port, _seen, server) = serve(Answer::NoTrailer);

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec(port, &proto, r#"{"name": "zuno"}"#));

    let reason = wait_for(&events, "the failure", |event| match event {
        Event::Failed { error, .. } => Some(error.to_string()),
        Event::Done { .. } => panic!("a reply with no grpc-status must not read as a success"),
        _ => None,
    });

    assert!(
        reason.contains("grpc-status"),
        "the message has to name what was missing: {reason}"
    );

    // **Drop the engine before joining the server.** See `a_unary_call_encodes_sends_and_decodes`.
    drop(engine);
    server.join().expect("server thread");
}

/// **A status in the response head is a status.** A Trailers-Only error — unknown method,
/// unauthenticated — carries `grpc-status` in the HEADERS frame and has no trailers at all.
/// Reading only the trailers called this server "not a gRPC endpoint", about a server that had
/// answered correctly. Both shapes, because the unary and streaming paths read the verdict
/// separately and each was wrong on its own.
#[test]
fn a_trailers_only_error_reads_as_its_status() {
    for (method, streaming) in [("SayHello", false), ("StreamHellos", true)] {
        let proto = fixture(&format!("trailersonly-{method}"));
        let (port, _seen, server) = serve(Answer::TrailersOnly(12));

        let engine = Engine::new().expect("engine");
        let (_job, events) =
            engine.send(call_spec(port, &proto, r#"{"name": "zuno"}"#, method, streaming));

        let reason = wait_for(&events, "the failure", |event| match event {
            Event::Failed { error, .. } => Some(error.to_string()),
            Event::Done { .. } | Event::Closed { .. } => {
                panic!("{method}: a non-zero status is a failure, wherever it was sent")
            }
            _ => None,
        });
        assert!(
            reason.contains("UNIMPLEMENTED") && reason.contains("method not found"),
            "{method}: the head's status and message are the answer: {reason}"
        );

        drop(engine);
        server.join().expect("server thread");
    }
}

/// **The wire follows the schema, not the stored copy of the method's shape.**
///
/// `GrpcRequest`'s streaming flags are a copy taken when the method was picked, and a `.proto`
/// edited or re-fetched since can leave them stale. Here they claim client-streaming for a
/// method the schema says is unary. When the engine read the copy for the request body and the
/// schema for the reply, it built a body that never closed, opened no transcript, and hung until
/// the read timeout.
#[test]
fn a_stale_stored_shape_does_not_change_the_call() {
    let proto = fixture("stale");
    let (port, seen, server) = serve(Answer::Ok);

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(client_spec(
        port,
        &proto,
        r#"{"name": "zuno"}"#,
        "SayHello",
        true,
        false,
    ));

    let body = wait_for(&events, "the reply", |event| match event {
        Event::Done { response, .. } => Some(response.body.clone()),
        Event::Failed { error, .. } => panic!("a unary method must be called as unary: {error}"),
        _ => None,
    });
    assert_eq!(seen.recv_timeout(DEADLINE).expect("the request"), "zuno");
    assert!(String::from_utf8_lossy(&body).contains("hello zuno"));

    // **Drop the engine before joining the server.** `serve_connection` returns when the
    // connection closes, and the engine caches its client — so while it is alive the h2
    // connection is pooled, the server has nothing to finish, and the join sits there. Measured:
    // 90 seconds per test instead of one. The same unbounded wait an unbounded `accept` is, one
    // layer up, and it reads as a slow test rather than as a bug.
    drop(engine);
    server.join().expect("server thread");
}

/// **A streaming call becomes a transcript**, on the same events a WebSocket and an SSE stream
/// produce — so the row list, the frame viewer, find and the eviction ring all work untouched.
///
/// Asserted on `Opened` naming gRPC as well as on the frames: without it the pane cannot tell a
/// streaming call from a socket, and what you may do next differs.
#[test]
fn a_server_streaming_call_becomes_a_transcript() {
    let proto = fixture("stream");
    let (port, _seen, server) = serve(Answer::Stream(3, None));

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(call_spec(
        port,
        &proto,
        r#"{"name": "zuno"}"#,
        "StreamHellos",
        true,
    ));

    let transport = wait_for(&events, "the stream to open", |event| match event {
        Event::Opened { transport, .. } => Some(*transport),
        Event::Failed { error, .. } => panic!("the stream must open: {error}"),
        _ => None,
    });
    assert_eq!(transport, zuno_core::engine::Transport::Grpc);

    let mut seen = Vec::new();
    loop {
        match events.recv_blocking().expect("events") {
            Event::Frame { frame, .. } => {
                if let zuno_core::engine::Frame::Text(text) = frame {
                    seen.push(text);
                }
            }
            Event::Closed { reason, .. } => {
                assert!(
                    reason.is_empty(),
                    "a clean end carries no reason, or the pane paints it as a failure: {reason}"
                );
                break;
            }
            Event::Failed { error, .. } => panic!("a completed stream is not a failure: {error}"),
            _ => {}
        }
    }

    assert_eq!(seen.len(), 3, "every message has to arrive: {seen:?}");
    assert!(
        seen[0].contains("hello 0") && seen[2].contains("hello 2"),
        "each message is decoded on its own: {seen:?}"
    );

    drop(engine);
    server.join().expect("server thread");
}

/// **A stream can fail *after* delivering messages**, and the status line cannot say so.
///
/// This is the case that makes trailers load-bearing rather than a detail: three replies arrive
/// under a 200, and only the trailer at the end says the call did not finish. A client that
/// judged by the status, or that treated the end of the body as success, reports a broken call
/// as a complete one — with the partial data on screen looking like the whole answer.
#[test]
fn a_stream_that_fails_after_delivering_is_still_a_failure() {
    let proto = fixture("streamfail");
    let (port, _seen, server) = serve(Answer::Stream(3, Some(14)));

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(call_spec(
        port,
        &proto,
        r#"{"name": "zuno"}"#,
        "StreamHellos",
        true,
    ));

    let mut frames = 0;
    let reason = loop {
        match events.recv_blocking().expect("events") {
            Event::Frame { .. } => frames += 1,
            Event::Failed { error, .. } => break error.to_string(),
            Event::Closed { .. } => panic!("a non-zero trailer is not a clean close"),
            _ => {}
        }
    };

    assert_eq!(frames, 3, "the messages before the failure still happened");
    assert!(
        reason.contains("UNAVAILABLE"),
        "the trailer's code has to name itself: {reason}"
    );

    drop(engine);
    server.join().expect("server thread");
}

/// **A client-streaming call keeps its request body open**, which is the mechanism nothing else
/// in Zuno has: every other request is a fixed run of bytes.
///
/// The server counts the messages it received and answers with the total, so this asserts that
/// all three arrived — the composed one sent on connect plus two sent afterwards — rather than
/// that the call merely succeeded. A body that closed after the first message, or one where the
/// later sends went nowhere, both answer `got 1` and would pass a weaker assertion.
///
/// **Half-close is what makes the reply possible.** A client-streaming server cannot answer
/// until the request ends, so if `Outbound::Close` did not drop the body sender this test would
/// hang rather than fail — which is why the deadline in `wait_for` is the load-bearing part of
/// its failure mode.
#[test]
fn a_client_streaming_call_sends_many_and_is_answered_once() {
    let proto = fixture("clientstream");
    let (port, _seen, server) = serve(Answer::Count);

    let engine = Engine::new().expect("engine");
    let (job, events) = engine.send(client_spec(
        port,
        &proto,
        r#"{"name": "first"}"#,
        "SendHellos",
        true,
        false,
    ));

    engine.send_frame(job, zuno_core::engine::Frame::Text(r#"{"name":"second"}"#.into()));
    engine.send_frame(job, zuno_core::engine::Frame::Text(r#"{"name":"third"}"#.into()));
    // The half-close. Without it the server is still waiting for more and never answers.
    engine.close(job);

    // **The transcript has to exist before the reply does**, which is the whole bug this test
    // was extended for. A client-streaming call answers once, so the reply only arrives after
    // the half-close — and the app refuses to send anything more without an open session. Wait
    // for `Opened` here and the send above would already have been impossible in the app.
    let opened = wait_for(&events, "the call to open for sending", |event| match event {
        Event::Opened { status, transport, .. } => Some((status.clone(), *transport)),
        Event::Failed { error, .. } => panic!("the call must open: {error}"),
        Event::Done { .. } => panic!("the transcript has to open before the reply arrives"),
        _ => None,
    });
    assert_eq!(opened.1, zuno_core::engine::Transport::Grpc);
    assert!(
        opened.0.is_none(),
        "the server has not answered yet, so there is no status to claim: {:?}",
        opened.0
    );

    let mut sent: Vec<String> = Vec::new();
    let body = loop {
        match events.recv_blocking().expect("events") {
            Event::Frame {
                direction: Direction::Sent,
                frame: zuno_core::engine::Frame::Text(text),
                ..
            } => sent.push(text),
            Event::Done { response, .. } => break response.body.clone(),
            Event::Failed { error, .. } => panic!("the call must succeed: {error}"),
            _ => {}
        }
    };

    // **Every message shows in the transcript**, which is a separate fact from every message
    // arriving. They did arrive — the server counted three — and none of them appeared: the
    // composer cleared, the row list stayed empty, and a successful send looked like a failed
    // one. Nothing but this assertion distinguishes the two.
    assert_eq!(
        sent.len(),
        3,
        "each message sent has to appear in the transcript: {sent:?}"
    );
    assert!(sent[0].contains("first") && sent[2].contains("third"), "{sent:?}");

    let text = String::from_utf8(body.to_vec()).expect("decoded JSON");
    assert!(
        text.contains("got 3"),
        "every message sent has to reach the server, not just the first: {text}"
    );

    drop(engine);
    server.join().expect("server thread");
}

/// **A port that is not speaking HTTP/2 is named as that**, rather than as h2's framing fault.
///
/// The commonest way to get here is a URL with no port — `http://host` is 80 — and h2's own
/// words, "frame with invalid size", point nowhere near it. The diagnosis lives in
/// `from_reqwest_grpc` rather than `from_reqwest`, because only for gRPC is it true.
#[test]
fn a_port_that_does_not_speak_http2_is_named() {
    use std::io::{Read as _, Write as _};

    let proto = fixture("http1");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // **Read everything the client sends before answering**, as a real server does.
            // Answering on the first byte raced the client's own dispatch: 7 runs in 20 the
            // connection failed before the request stream existed, and hyper reported a bare
            // "connection closed" — a flake about the fixture, not about the diagnosis.
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok();
            let mut buffer = [0u8; 4096];
            while matches!(stream.read(&mut buffer), Ok(n) if n > 0) {}
            stream.set_read_timeout(Some(DEADLINE)).ok();
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
            // Held open until the client gives up, as a real server on the wrong port does —
            // hanging up at once surfaces as a broken pipe instead, which is not the case here.
            while matches!(stream.read(&mut buffer), Ok(n) if n > 0) {}
        }
    });

    let engine = Engine::new().expect("engine");
    let (_job, events) = engine.send(spec(port, &proto, r#"{"name": "zuno"}"#));
    let reason = wait_for(&events, "the failure", |event| match event {
        Event::Failed { error, .. } => Some(error.to_string()),
        Event::Done { .. } => panic!("an HTTP/1.1 server cannot answer a gRPC call"),
        _ => None,
    });
    assert!(
        reason.contains("did not answer HTTP/2") && reason.contains("port"),
        "the fix is the port, and the message has to say so: {reason}"
    );

    drop(engine);
    server.join().expect("server thread");
}

/// **A message that does not encode is refused alone, and the call carries on.**
///
/// It used to end the stream: the feeder broke out of its loop, which half-closed the request, so
/// the server answered as if the messages before the typo were the whole conversation — `got 1`
/// here — and nothing said why. The server's count is the assertion, because a call that
/// silently stopped early looks exactly like one that finished.
#[test]
fn a_bad_message_is_refused_without_ending_the_call() {
    let proto = fixture("rejected");
    let (port, _seen, server) = serve(Answer::Count);

    let engine = Engine::new().expect("engine");
    let (job, events) = engine.send(client_spec(
        port,
        &proto,
        r#"{"name": "first"}"#,
        "SendHellos",
        true,
        false,
    ));

    engine.send_frame(job, zuno_core::engine::Frame::Text(r#"{"nmae": "typo"}"#.into()));
    engine.send_frame(job, zuno_core::engine::Frame::Text(r#"{"name": "fixed"}"#.into()));
    engine.close(job);

    let mut rejected = Vec::new();
    let body = loop {
        match events.recv_blocking().expect("events") {
            Event::Rejected { text, reason, .. } => rejected.push((text, reason)),
            Event::Done { response, .. } => break response.body.clone(),
            Event::Failed { error, .. } => panic!("a refused message must not fail the call: {error}"),
            _ => {}
        }
    };

    assert_eq!(rejected.len(), 1, "the typo is named, once: {rejected:?}");
    assert!(rejected[0].0.contains("nmae"), "the refused text is carried: {rejected:?}");
    assert!(
        String::from_utf8_lossy(&body).contains("got 2"),
        "the messages either side of the typo both reach the server: {}",
        String::from_utf8_lossy(&body)
    );

    drop(engine);
    server.join().expect("server thread");
}

/// **Bidirectional: both halves open at once**, which is the shape that needed nothing new —
/// the streaming request body and the transcript reply already existed, and bidi is simply
/// both switched on.
///
/// Asserted on one reply *per* message sent, which is what distinguishes a genuine bidirectional
/// call from a client-streaming one that happened to answer more than once.
///
/// **Not interleaved**, and worth saying so rather than implying otherwise: this server reads
/// the whole request before replying, so it proves the plumbing — body stays open, half-close
/// lands, replies stream back, trailers arrive — but not that a reply can overtake a request
/// still being sent. A server that interleaves needs to read its body incrementally, which is a
/// test fixture rather than a claim about this code.
#[test]
fn a_bidirectional_call_streams_in_both_directions() {
    let proto = fixture("bidi");
    let (port, _seen, server) = serve(Answer::Echo);

    let engine = Engine::new().expect("engine");
    let (job, events) = engine.send(client_spec(
        port,
        &proto,
        r#"{"name": "one"}"#,
        "Chat",
        true,
        true,
    ));

    // **`Opened` first, before anything more is sent — the order the app is held to.** The app
    // refuses to send without an open transcript, and Disconnect without one aborts rather than
    // half-closing. This server reads the whole request before it replies, so if `Opened` waited
    // for the response head this would deadlock: the head waits for the close, and the close
    // waits for the transcript. It did, and the test used to send first to step around it —
    // which is the one thing a person cannot do.
    let status = wait_for(&events, "the call to open for sending", |event| match event {
        Event::Opened { status, .. } => Some(status.clone()),
        Event::Frame { .. } => panic!("no frame may arrive before the transcript exists"),
        Event::Failed { error, .. } => panic!("the call must open: {error}"),
        _ => None,
    });
    assert!(status.is_none(), "the server has not answered yet: {status:?}");

    engine.send_frame(job, zuno_core::engine::Frame::Text(r#"{"name":"two"}"#.into()));
    engine.close(job);

    let mut sent = Vec::new();
    let mut received = Vec::new();
    loop {
        match events.recv_blocking().expect("events") {
            Event::Frame {
                direction,
                frame: zuno_core::engine::Frame::Text(text),
                ..
            } => match direction {
                Direction::Sent => sent.push(text),
                Direction::Received => received.push(text),
            },
            Event::Opened { .. } => panic!("a second `Opened` would restart the transcript"),
            Event::Closed { .. } => break,
            Event::Failed { error, .. } => panic!("a completed chat is not a failure: {error}"),
            _ => {}
        }
    }

    // **Split by direction.** Counted together, this passed only because the one `Sent` row
    // happened to land before `Opened` and be swallowed by the wait above.
    assert_eq!(sent.len(), 2, "both messages sent belong in the transcript: {sent:?}");
    assert!(sent[0].contains("one") && sent[1].contains("two"), "{sent:?}");
    assert_eq!(received.len(), 2, "one reply per message sent: {received:?}");
    assert!(received[0].contains("re0:one"), "{received:?}");
    assert!(received[1].contains("re1:two"), "{received:?}");

    drop(engine);
    server.join().expect("server thread");
}


/// **Reflection, end to end: ask a server what it offers and get a usable schema back.**
///
/// The server here plays the protocol properly, and the part that matters is that it
/// **interleaves** — it reads one request, answers it, and reads the next, all on one open call.
/// That is not decoration: reflection is a real bidirectional conversation, and a server that
/// waits for the whole request before replying *deadlocks* against a client waiting for the
/// service list before it can ask its next question. The first version of this fixture did
/// exactly that and hung.
///
/// **Asserted on the schema being usable**, not merely on bytes coming back: the descriptor set
/// is loaded and its methods listed, which is the only thing proving the round trip produced
/// something a call could actually be composed against.
#[test]
fn reflection_fetches_a_schema_the_client_never_had() {
    use prost::Message as _;

    // Real descriptors, compiled from the fixture the *client* never sees.
    let proto = fixture("reflect");
    let set = protox::compile([&proto], [proto.parent().expect("parent")]).expect("compile");
    let descriptors: Vec<Vec<u8>> = set.file.iter().map(|file| file.encode_to_vec()).collect();

    let (port, asked, server) = serve_reflection(descriptors, true);

    let engine = Engine::new().expect("engine");
    let mut spec = RequestSpec::default();
    spec.url = format!("http://127.0.0.1:{port}");

    let bytes = engine
        .reflect(spec)
        .recv_blocking()
        .expect("a reply")
        .expect("reflection must succeed");

    // **What the server was asked**, which is the half a self-agreeing fake cannot check.
    let mut questions = Vec::new();
    while let Ok(question) = asked.recv_timeout(std::time::Duration::from_millis(500)) {
        questions.push(question);
    }
    assert!(
        questions.contains(&"list_services".to_string()),
        "the conversation starts by asking what exists: {questions:?}"
    );
    assert!(
        questions.contains(&"helloworld.Greeter".to_string()),
        "then asks for the file containing each service by symbol: {questions:?}"
    );
    assert!(
        !questions.iter().any(|q| q.starts_with("grpc.reflection.")),
        "reflection must not ask for its own schema: {questions:?}"
    );

    let schema = zuno_core::grpc::Schema::from_descriptor_set(&bytes)
        .expect("what comes back has to load as a schema");
    let methods = schema.methods();
    assert!(
        methods.iter().any(|method| method.name == "SayHello"),
        "the schema has to carry the server's methods: {methods:?}"
    );
    assert!(
        methods
            .iter()
            .any(|method| method.name == "Chat"
                && method.shape() == zuno_core::grpc::Shape::BidiStreaming),
        "and their shapes, so a picked method knows whether it streams: {methods:?}"
    );

    drop(engine);
    server.join().expect("server thread");
}

/// **A server with reflection switched off is told apart from one that is broken.**
///
/// `UNIMPLEMENTED` is exactly how a server refuses a service it does not host, and it is the
/// ordinary case — plenty of production servers disable reflection deliberately. Reporting it as
/// a transport failure would send someone looking at their network instead of at a flag.
#[test]
fn a_server_without_reflection_says_so() {
    let (port, _asked, server) = serve_reflection(Vec::new(), false);

    let engine = Engine::new().expect("engine");
    let mut spec = RequestSpec::default();
    spec.url = format!("http://127.0.0.1:{port}");

    let error = engine
        .reflect(spec)
        .recv_blocking()
        .expect("a reply")
        .expect_err("a server with no reflection must not report success");

    assert!(
        error.to_string().contains("reflection"),
        "the message has to name what is missing: {error}"
    );

    drop(engine);
    server.join().expect("server thread");
}

/// **When a server refuses to describe a symbol, its reason is the answer.**
///
/// It says why inside a healthy call — `NOT_FOUND`, with a message — and that message was being
/// discarded, so the only thing reported was "returned no descriptors", which names no cause.
#[test]
fn a_refused_symbol_reports_the_servers_reason() {
    let (port, _asked, server) = serve_reflection(Vec::new(), true);

    let engine = Engine::new().expect("engine");
    let mut spec = RequestSpec::default();
    spec.url = format!("http://127.0.0.1:{port}");

    let error = engine
        .reflect(spec)
        .recv_blocking()
        .expect("a reply")
        .expect_err("nothing to load is not a success");

    assert!(
        error.to_string().contains("symbol not found: helloworld.Greeter"),
        "the server's own reason has to reach the person: {error}"
    );

    drop(engine);
    server.join().expect("server thread");
}

/// An interleaving reflection server.
///
/// Reads the request stream message by message and answers each as it arrives, which is what a
/// real one does and what this conversation requires. Returns the questions it was asked, so a
/// test can assert on the conversation rather than only on its result.
fn serve_reflection(
    descriptors: Vec<Vec<u8>>,
    supported: bool,
) -> (
    u16,
    std::sync::mpsc::Receiver<String>,
    std::thread::JoinHandle<()>,
) {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let _ = port_tx.send(listener.local_addr().expect("addr").port());

            // **One connection, two requests.** The client tries `v1` and falls back to
            // `v1alpha`, and reqwest multiplexes both onto the same h2 connection — so a server
            // that waits to `accept` a second one waits forever, and the join at the end of the
            // test hangs rather than failing. Measured: both requests arrived here on one
            // connection.
            {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let descriptors = descriptors.clone();
                let asked_tx = asked_tx.clone();

                let service = hyper::service::service_fn(
                    move |request: http::Request<hyper::body::Incoming>| {
                        let descriptors = descriptors.clone();
                        let asked_tx = asked_tx.clone();
                        async move {
                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
                                Result<hyper::body::Frame<Bytes>, std::convert::Infallible>,
                            >();

                            if !supported {
                                let mut trailers = http::HeaderMap::new();
                                trailers
                                    .insert("grpc-status", http::HeaderValue::from_static("12"));
                                trailers.insert(
                                    "grpc-message",
                                    http::HeaderValue::from_static("unknown service"),
                                );
                                let _ = tx.send(Ok(hyper::body::Frame::trailers(trailers)));
                            } else {
                                // **Reads and replies as it goes**, on its own task, so the
                                // response head can go out before the request has finished.
                                tokio::spawn(async move {
                                    let mut body = request.into_body();
                                    let mut reader = zuno_core::grpc::Reader::default();

                                    while let Some(Ok(frame)) = body.frame().await {
                                        let Some(chunk) = frame.data_ref() else {
                                            continue;
                                        };
                                        for message in reader.push(chunk) {
                                            // `list_services` is field 7, tag `0x3a`;
                                            // `file_containing_symbol` is field 4, tag `0x22`.
                                            match message.first() {
                                                Some(0x3a) => {
                                                    let _ = asked_tx
                                                        .send("list_services".to_string());
                                                    // **Reflection lists itself**, as every
                                                    // real server does — which is what makes
                                                    // the client's filter load-bearing rather
                                                    // than decoration.
                                                    let mut inner = Vec::new();
                                                    for name in [
                                                        "grpc.reflection.v1.ServerReflection",
                                                        "helloworld.Greeter",
                                                    ] {
                                                        let entry = length_delimited(
                                                            1,
                                                            name.as_bytes(),
                                                        );
                                                        inner.extend(length_delimited(1, &entry));
                                                    }
                                                    let _ = tx.send(Ok(
                                                        hyper::body::Frame::data(Bytes::from(
                                                            zuno_core::grpc::frame(
                                                                &length_delimited(6, &inner),
                                                            ),
                                                        )),
                                                    ));
                                                }
                                                Some(0x22) => {
                                                    let _ = asked_tx.send(
                                                        String::from_utf8_lossy(&message[2..])
                                                            .into_owned(),
                                                    );
                                                    // With nothing to describe, refuse the way a
                                                    // real server does: an `ErrorResponse`
                                                    // (field 7) inside a healthy call.
                                                    let reply = if descriptors.is_empty() {
                                                        let mut error = vec![1 << 3, 5];
                                                        error.extend(length_delimited(
                                                            2,
                                                            b"symbol not found: helloworld.Greeter",
                                                        ));
                                                        length_delimited(7, &error)
                                                    } else {
                                                        let mut inner = Vec::new();
                                                        for file in &descriptors {
                                                            inner.extend(length_delimited(1, file));
                                                        }
                                                        length_delimited(4, &inner)
                                                    };
                                                    let _ = tx.send(Ok(
                                                        hyper::body::Frame::data(Bytes::from(
                                                            zuno_core::grpc::frame(&reply),
                                                        )),
                                                    ));
                                                }
                                                _ => {}
                                            }
                                        }
                                    }

                                    let mut trailers = http::HeaderMap::new();
                                    trailers.insert(
                                        "grpc-status",
                                        http::HeaderValue::from_static("0"),
                                    );
                                    let _ = tx.send(Ok(hyper::body::Frame::trailers(trailers)));
                                });
                            }

                            Ok::<_, std::convert::Infallible>(
                                http::Response::builder()
                                    .status(200)
                                    .header("content-type", "application/grpc")
                                    .body(http_body_util::StreamBody::new(
                                        futures_util::stream::unfold(rx, |mut rx| async move {
                                            rx.recv().await.map(|frame| (frame, rx))
                                        }),
                                    ))
                                    .expect("response"),
                            )
                        }
                    },
                );

                let _ =
                    hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
            }
        });
    });

    let port = port_rx
        .recv_timeout(DEADLINE)
        .expect("the server must bind");
    (port, asked_rx, handle)
}

/// One length-delimited protobuf field, for building fixtures the way a server would.
fn length_delimited(number: u64, value: &[u8]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    let mut out = Vec::new();
    varint(&mut out, (number << 3) | 2);
    varint(&mut out, value.len() as u64);
    out.extend_from_slice(value);
    out
}

/// **Against the real world**, and `#[ignore]`d so CI never depends on the network.
///
/// This test is the one that should have existed first. Every other test in this file runs
/// against a fixture I wrote, which agrees with my implementation by construction — and three
/// real bugs survived all of them: `list_services` sent an empty value that a real server
/// ignores, the conversation was modelled as one held-open stream that a real server deadlocks
/// against, and a scheme-less address defaulted to TLS where the server speaks cleartext.
/// CLAUDE.md says the live `wss://` check is not optional for exactly this reason; I skipped the
/// equivalent here and it cost a round of "it does not work" that a fixture could never catch.
///
/// `ZUNO_GRPC_URL` overrides the endpoint. The default is Postman's public echo server, which
/// is **cleartext HTTP/2 on port 443** — unusual enough to be worth knowing, and the reason the
/// scheme is spelled out rather than left to the default.
///
/// ```text
/// cargo test -p zuno-core --test grpc -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs the network"]
fn a_real_server_describes_itself() {
    let url = std::env::var("ZUNO_GRPC_URL")
        .unwrap_or_else(|_| "http://grpc.postman-echo.com:443".to_string());

    let engine = Engine::new().expect("engine");
    let mut spec = RequestSpec::default();
    spec.url = url.clone();

    let bytes = engine
        .reflect(spec)
        .recv_blocking()
        .expect("a reply")
        .unwrap_or_else(|error| panic!("reflection against {url} failed: {error}"));

    let schema =
        zuno_core::grpc::Schema::from_descriptor_set(&bytes).expect("the schema must load");
    let methods = schema.methods();

    eprintln!("{url} offers {} method(s):", methods.len());
    for method in &methods {
        eprintln!("  {}/{}  [{}]", method.service, method.name, method.shape().label());
    }

    assert!(
        !methods.is_empty(),
        "a server that answers reflection has to describe at least one method"
    );
    // Its own reflection service is filtered out, so anything left is a real service.
    assert!(
        !methods
            .iter()
            .any(|method| method.service.starts_with("grpc.reflection.")),
        "reflection must not describe itself: {methods:?}"
    );
}
