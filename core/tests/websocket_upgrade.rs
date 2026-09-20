//! **Spike: does a WebSocket upgrade survive reqwest?**
//!
//! The question this answers, and nothing more: can Zuno get a raw duplex stream out of the
//! *existing* `reqwest::Client` — the one that already carries the user's TLS settings, custom
//! certificates, proxy and cookie jar — or does WebSocket have to open its own socket and
//! re-implement all of that?
//!
//! reqwest 0.13.4 has **no `Response::upgrade()`**, despite exporting an `Upgraded` type, so the
//! route has to go around it: `From<Response> for http::Response<Body>` hands back the response
//! parts verbatim — extensions included — and `hyper::upgrade::on` reads hyper's `OnUpgrade` out
//! of those. Two things had to be true for that to work and both were read out of the vendored
//! sources rather than assumed: reqwest's `Response::new` rebuilds from `into_parts`, so it never
//! drops extensions, and hyper-util's pooled client calls `conn.with_upgrades()` on HTTP/1.1
//! connections. This test is here because *reading* both is not the same as it working.
//!
//! The handshake is deliberately not a real one — `Sec-WebSocket-Accept` is a fixed string. There
//! is no WebSocket client in this test to validate it, and inventing one would mean pulling in
//! sha1 and base64 to prove something else entirely. What is being proven is the transport: a
//! 101 reaches the caller, and bytes flow **both ways** afterwards.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// `TcpListener::accept` with a deadline, so a wrong assumption fails in seconds with a message
/// rather than hanging CI — the same reason `tests/engine.rs` has one.
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

#[test]
fn a_websocket_upgrade_survives_the_reqwest_client() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server = std::thread::spawn(move || {
        let mut stream = accept_before(&listener, Duration::from_secs(10)).expect("a connection");

        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        let handshake = String::from_utf8_lossy(&seen).into_owned();

        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Accept: spike\r\n\r\n",
            )
            .expect("101");
        // Past this point the connection is no longer HTTP, which is the whole point: these are
        // raw bytes, written before the client has said anything, so reading them proves the
        // stream is live rather than replayed out of some buffer hyper was still holding.
        stream.write_all(b"SRV").expect("server frame");
        stream.flush().expect("flush");

        let mut from_client = [0u8; 3];
        let echoed = stream.read_exact(&mut from_client).is_ok();

        (handshake, from_client, echoed)
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");

    runtime.block_on(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let client = reqwest::Client::builder()
            .user_agent("zuno-spike")
            .build()
            .expect("client");

        let response = client
            .get(format!("http://{addr}/socket"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .expect("the 101 must reach the caller rather than becoming an error");

        assert_eq!(
            response.status().as_u16(),
            101,
            "reqwest has to surface Switching Protocols as an ordinary response"
        );

        // The conversion that makes this possible at all. `into_parts`/`from_parts` on both
        // sides means `OnUpgrade` is still in `extensions` here.
        let response: http::Response<reqwest::Body> = response.into();
        let upgraded = hyper::upgrade::on(response)
            .await
            .expect("hyper must hand back the upgraded connection");

        let mut io = hyper_util::rt::TokioIo::new(upgraded);

        let mut from_server = [0u8; 3];
        io.read_exact(&mut from_server)
            .await
            .expect("reading past the upgrade");
        assert_eq!(&from_server, b"SRV", "server-to-client must flow");

        io.write_all(b"CLI").await.expect("writing past the upgrade");
        io.flush().await.expect("flush");
    });

    let (handshake, from_client, echoed) = server.join().expect("server thread");

    assert!(
        handshake.starts_with("GET /socket HTTP/1.1"),
        "the upgrade must go out over HTTP/1.1 — h2 has no 101. Got:\n{handshake}"
    );
    assert!(
        handshake.contains("sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ=="),
        "reqwest must pass the handshake headers through untouched. Got:\n{handshake}"
    );
    assert!(echoed, "the server never received the client's frame");
    assert_eq!(&from_client, b"CLI", "client-to-server must flow");
}
