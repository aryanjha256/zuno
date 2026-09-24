//! **The transport spike for gRPC**, proving two claims before anything is built on them.
//!
//! ROADMAP's gRPC entry says the transport is the easy part and is already proven. It is not
//! proven until something runs, and two of its assumptions are load-bearing enough that being
//! wrong about either changes the whole approach:
//!
//! 1. **gRPC reports its status in HTTP/2 trailers**, and reqwest names trailers nowhere in its
//!    public API. `Response::headers()` is the *leading* headers only. The route out is the same
//!    one the WebSocket upgrade uses — `From<Response> for http::Response<Body>` — followed by
//!    `http_body::Body::poll_frame`, because `reqwest::Body` forwards hyper's frames verbatim
//!    and a `Frame` is either data or trailers. If that chain does not actually deliver them, a
//!    failed gRPC call is unreadable: `grpc-status` is the only place the error lives.
//!
//! 2. **Plaintext gRPC needs h2 with prior knowledge.** There is no ALPN on a cleartext socket
//!    and no Upgrade dance in gRPC, so a client that does not *insist* on h2 sends HTTP/1.1 and
//!    the server hangs up. This is the exact mirror of the `wss://` bug, where ALPN negotiated
//!    h2 and the 101 could never arrive — same trap, opposite direction, and worth a test for
//!    the same reason: the offline failure is silent and looks like a bad URL.
//!
//! The server is `hyper`'s own h2 server rather than a hand-written socket, for the reason
//! `tests/websocket.rs` uses tungstenite's: a fake would agree with whatever this code sent.
//! It is not a gRPC server — it speaks the framing and the trailers, which is the part under
//! test. A real service lands with the schema work.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;

/// The 5-byte prefix in front of every gRPC message: one compression flag, then a big-endian
/// u32 length. Deliberately written out here rather than imported — the point of the spike is
/// that the wire format is small enough to own, which is why `tonic` is not a dependency.
fn framed(payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(0); // not compressed
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

fn unframe(bytes: &[u8]) -> Option<&[u8]> {
    let (head, rest) = bytes.split_at_checked(5)?;
    let len = u32::from_be_bytes(head[1..5].try_into().ok()?) as usize;
    rest.get(..len)
}

/// An h2 server that answers one call the way a gRPC server does: a framed message, then
/// trailers carrying the status.
async fn serve(addr: SocketAddr, ready: tokio::sync::oneshot::Sender<u16>) {
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let _ = ready.send(port);

    let Ok((stream, _)) = listener.accept().await else {
        return;
    };

    let service = hyper::service::service_fn(|_request| async {
        // A data frame and then a trailers frame, which is exactly the shape a gRPC response
        // has: the message, then `grpc-status` telling you whether to believe it.
        let frames: Vec<Result<hyper::body::Frame<Bytes>, Infallible>> = vec![
            Ok(hyper::body::Frame::data(framed(b"pong"))),
            Ok(hyper::body::Frame::trailers({
                let mut trailers = http::HeaderMap::new();
                trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                trailers.insert("grpc-message", http::HeaderValue::from_static("OK"));
                trailers
            })),
        ];
        let body = http_body_util::StreamBody::new(futures_util::stream::iter(frames));

        Ok::<_, Infallible>(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/grpc")
                .body(body)
                .expect("response"),
        )
    });

    let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
        .await;
}

/// **The whole claim, end to end.**
///
/// Asserted on the trailers specifically, not merely on the call succeeding: a response with no
/// trailers at all still has a 200 and a readable body, so "the request worked" would pass
/// against exactly the failure this exists to rule out.
#[tokio::test]
async fn a_grpc_style_response_yields_its_trailers_through_reqwest() {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve("127.0.0.1:0".parse().expect("addr"), ready_tx));
    let port = tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("the server must bind")
        .expect("the port");

    // **`http2_prior_knowledge`, and the test fails without it.** No ALPN on cleartext, so
    // reqwest would otherwise speak HTTP/1.1 at an h2-only server.
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("client");

    let response = client
        .post(format!("http://127.0.0.1:{port}/pkg.Service/Method"))
        .header("content-type", "application/grpc")
        .body(framed(b"ping"))
        .send()
        .await
        .expect("the call must reach an h2 server");

    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.status(), 200);

    // The conversion the WebSocket upgrade already relies on, used here for the body rather
    // than for the extensions.
    let response: http::Response<reqwest::Body> = response.into();
    let collected = response
        .into_body()
        .collect()
        .await
        .expect("the body must collect");

    let trailers = collected
        .trailers()
        .cloned()
        .expect("gRPC puts its status in trailers, and they have to be reachable");
    let bytes = collected.to_bytes();

    assert_eq!(
        trailers.get("grpc-status").and_then(|v| v.to_str().ok()),
        Some("0"),
        "the status trailer is the only place a gRPC error lives"
    );
    assert_eq!(
        trailers.get("grpc-message").and_then(|v| v.to_str().ok()),
        Some("OK")
    );
    assert_eq!(
        unframe(&bytes),
        Some(&b"pong"[..]),
        "the 5-byte length prefix has to round-trip, or nothing above this can parse"
    );

    // **Drop the client before waiting on the server.** `serve_connection` returns when the
    // connection closes, and an h2 connection is *pooled* — so while the client is alive there
    // is nothing to wait for. Without this the test passed in 90 seconds instead of one,
    // sitting on an idle socket: the same unbounded-wait hazard as an unbounded `accept`, one
    // layer up, and it reads as a slow test rather than as a bug.
    drop(client);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server must finish once the client is gone")
        .expect("server task");
}
