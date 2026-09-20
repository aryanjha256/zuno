//! A WebSocket, run as a job on the same engine as every other request.
//!
//! **The connection rides the user's own `reqwest::Client`**, which is the entire reason this
//! file is shaped the way it is. `tokio_tungstenite::connect_async` would be four lines and
//! would open its own socket — ignoring the TLS settings, the client certificates, the proxy
//! and the cookie jar that the rest of the app spent panels on. So the handshake goes out as an
//! ordinary request, and only the *framing* is tungstenite's.
//!
//! Getting from a finished `reqwest::Response` to a duplex stream takes three steps that are
//! each somebody else's internals: `From<Response> for http::Response<Body>` preserves
//! `extensions`, `hyper::upgrade::on` reads hyper's `OnUpgrade` out of them, and
//! `hyper_util::rt::TokioIo` puts tokio's traits back on what that returns. reqwest exports an
//! `Upgraded` type and no way to reach one — there is no `Response::upgrade()` — so this route
//! is not a preference. `tests/websocket_upgrade.rs` exists because three layers agreeing is
//! not something to take on trust.

use std::time::Instant;

use async_channel::Sender;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::error::EngineError;
use super::run::collect_headers;
use super::{Direction, Event, Frame, JobId, Outbound};
use crate::request::RequestSpec;

/// Open the socket and pump it until either side closes.
pub async fn connect(
    job: JobId,
    client: reqwest::Client,
    spec: RequestSpec,
    events: Sender<Event>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
) {
    let Some(socket) = spec.websocket().cloned() else {
        let _ = events
            .send(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: "not a WebSocket request".to_string(),
                },
            })
            .await;
        return;
    };

    let started = Instant::now();
    let _ = events.send(Event::Started { job }).await;

    // Kept, because it is the only thing that can verify the server's reply.
    let key = generate_key();

    let request = match super::build::build_websocket(&spec, &socket, &key) {
        Ok(request) => request,
        Err(error) => {
            let _ = events.send(Event::Failed { job, error }).await;
            return;
        }
    };

    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            let _ = events
                .send(Event::Failed {
                    job,
                    error: EngineError::from_reqwest(&error, spec.settings.timeout),
                })
                .await;
            return;
        }
    };

    let status = response.status().as_u16();
    let status_text = response
        .status()
        .canonical_reason()
        .unwrap_or_default()
        .to_string();
    let headers = collect_headers(response.headers());

    if status != 101 {
        // A server that answers 200 or 404 to an upgrade is the ordinary case of a wrong URL,
        // and saying so beats a parse error from further down.
        let _ = events
            .send(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: format!(
                        "the server answered {status} instead of 101 Switching Protocols — \
                         this endpoint is not a WebSocket"
                    ),
                },
            })
            .await;
        return;
    }

    // **Checked, not assumed.** The accept key is the only proof that what answered understood
    // the handshake rather than echoing a 101 it did not compute — RFC 6455 requires the client
    // to fail the connection when it does not match.
    let expected = derive_accept_key(key.as_bytes());
    let given = response
        .headers()
        .get(http::header::SEC_WEBSOCKET_ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if given != expected {
        let _ = events
            .send(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: "the server's Sec-WebSocket-Accept does not match the key we sent"
                        .to_string(),
                },
            })
            .await;
        return;
    }

    let protocol = response
        .headers()
        .get(http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let response: http::Response<reqwest::Body> = response.into();
    let upgraded = match hyper::upgrade::on(response).await {
        Ok(upgraded) => upgraded,
        Err(error) => {
            let _ = events
                .send(Event::Failed {
                    job,
                    error: EngineError::Other {
                        reason: format!("the connection could not be upgraded: {error}"),
                    },
                })
                .await;
            return;
        }
    };

    // `None` config: tungstenite's defaults cap a message at 64 MiB, which is far past
    // anything a person reads in a transcript and well short of letting a server exhaust us.
    let mut stream = WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        Role::Client,
        None,
    )
    .await;

    if events
        .send(Event::Opened {
            job,
            status,
            status_text,
            headers,
            protocol,
            elapsed: started.elapsed(),
        })
        .await
        .is_err()
    {
        return;
    }

    // Set once the close handshake has been started from this side. The loop keeps *reading*
    // afterwards rather than returning: RFC 6455's close is an exchange, and dropping the
    // stream on the outgoing Close would deny the server the chance to answer — which is the
    // difference between a clean close and a reset that shows up in the server's logs.
    let mut closing = false;

    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(message)) => {
                    match message {
                        Message::Close(frame) => {
                            let (code, reason) = frame
                                .map(|frame| (Some(u16::from(frame.code)), frame.reason.to_string()))
                                .unwrap_or((None, String::new()));
                            let _ = events.send(Event::Closed { job, code, reason }).await;
                            return;
                        }
                        // `Frame` is only produced by the raw-frame API, which this never uses.
                        Message::Frame(_) => {}
                        other => {
                            if let Some(frame) = into_frame(other)
                                && events
                                    .send(Event::Frame {
                                        job,
                                        at: started.elapsed(),
                                        direction: Direction::Received,
                                        frame,
                                    })
                                    .await
                                    .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                // **A peer that vanishes is a close, not a failure — and especially so once
                // we have asked to close.** RFC 6455's close is an exchange, and plenty of
                // servers answer it by simply dropping the TCP connection rather than
                // returning a Close frame. Reporting that as an error would put a red error
                // pane over a conversation that ended exactly as asked, and would lose the
                // transcript's "closed" state, which is the only thing saying the run is over.
                Some(Err(error)) => {
                    let ended = closing
                        || matches!(
                            error,
                            tungstenite::Error::ConnectionClosed
                                | tungstenite::Error::AlreadyClosed
                        );
                    let _ = if ended {
                        events.send(Event::Closed { job, code: None, reason: String::new() }).await
                    } else {
                        events.send(Event::Failed {
                            job,
                            error: EngineError::Other { reason: error.to_string() },
                        }).await
                    };
                    return;
                }
                // The stream ended without a Close frame: the peer went away.
                None => {
                    let _ = events.send(Event::Closed { job, code: None, reason: String::new() }).await;
                    return;
                }
            },

            command = outbound.recv(), if !closing => match command {
                Some(Outbound::Frame(frame)) => {
                    if let Err(error) = stream.send(into_message(frame.clone())).await {
                        let _ = events.send(Event::Failed {
                            job,
                            error: EngineError::Other { reason: error.to_string() },
                        }).await;
                        return;
                    }
                    // Emitted *after* the write succeeds, so the transcript never shows a frame
                    // as sent that the socket refused.
                    if events.send(Event::Frame {
                        job,
                        at: started.elapsed(),
                        direction: Direction::Sent,
                        frame,
                    }).await.is_err() {
                        return;
                    }
                }
                // `None` is the engine dropping the sender, which happens when the job is
                // forgotten — treated as a close request rather than an abort so the peer
                // still gets its Close frame.
                Some(Outbound::Close) | None => {
                    closing = true;
                    if stream.close(None).await.is_err() {
                        let _ = events.send(Event::Closed { job, code: None, reason: String::new() }).await;
                        return;
                    }
                }
            },
        }
    }
}

fn into_frame(message: Message) -> Option<Frame> {
    match message {
        Message::Text(text) => Some(Frame::Text(text.to_string())),
        Message::Binary(bytes) => Some(Frame::Binary(bytes)),
        Message::Ping(bytes) => Some(Frame::Ping(bytes)),
        Message::Pong(bytes) => Some(Frame::Pong(bytes)),
        Message::Close(_) | Message::Frame(_) => None,
    }
}

fn into_message(frame: Frame) -> Message {
    match frame {
        Frame::Text(text) => Message::Text(text.into()),
        Frame::Binary(bytes) => Message::Binary(bytes),
        Frame::Ping(bytes) => Message::Ping(bytes),
        Frame::Pong(bytes) => Message::Pong(bytes),
    }
}
