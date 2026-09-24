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

/// The subprotocol every current GraphQL-over-WebSocket server speaks.
///
/// **Not `graphql-ws`**, which is the string the *deprecated* `subscriptions-transport-ws`
/// library uses — the modern library is named `graphql-ws` and announces itself as this. Two
/// different things share one name, and offering the wrong one gets a socket that opens and then
/// ignores everything sent down it.
const GRAPHQL_SUBPROTOCOL: &str = "graphql-transport-ws";

/// The largest single frame that will be accepted.
///
/// Half the transcript's whole byte budget, deliberately: one frame that could evict every
/// other one is a frame worth refusing.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// How long to wait for the peer's answering Close before dropping the connection.
///
/// **RFC 6455 says to wait a reasonable time and then close the socket**, and nothing did — the
/// loop went on reading after `stream.close(None)`, so a server that neither answers the Close
/// nor drops the TCP connection left the task alive and the strip reading `open` forever. A
/// fixed few seconds rather than `settings.timeout`, because this is the wait *after* you press
/// Disconnect: a 30-second request timeout is a reasonable deadline for an answer and an
/// unreasonable one for a goodbye.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Open the socket and pump it until either side closes.
pub async fn connect(
    job: JobId,
    client: reqwest::Client,
    spec: RequestSpec,
    events: Sender<Event>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
) {
    // A GraphQL subscription borrows the WebSocket path: same handshake, same framing, with
    // `graphql-transport-ws` offered and a protocol spoken on top. Modelling it as a synthetic
    // `WebSocketRequest` rather than branching the handshake keeps one code path for the part
    // that is genuinely identical.
    let (socket, graphql) = match &spec.kind {
        crate::request::RequestKind::WebSocket(socket) => (socket.clone(), None),
        crate::request::RequestKind::GraphQl(graphql) => (
            crate::request::WebSocketRequest {
                subprotocols: vec![GRAPHQL_SUBPROTOCOL.to_string()],
                messages: Vec::new(),
            },
            Some(graphql.clone()),
        ),
        // **gRPC streams over HTTP/2, not over a WebSocket.** It reaches a transcript by the
        // same events this file emits, but through `run`'s h2 path — there is no handshake to
        // share, so routing one here would open a socket nothing on the other end expects.
        crate::request::RequestKind::Http(_) | crate::request::RequestKind::Grpc(_) => {
            let _ = events
                .send(Event::Failed {
                    job,
                    error: EngineError::Other {
                        reason: "not a WebSocket request".to_string(),
                    },
                })
                .await;
            return;
        }
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

    // **8 MiB, not tungstenite's 64.** The default is four times the whole transcript budget,
    // so a single frame could evict every other one and still not fit — and a server that sends
    // one is either broken or hostile. A message past this fails the connection loudly, which
    // is the behaviour worth having: quietly consuming memory is how the process dies with no
    // explanation.
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES));
    let mut stream = WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        Role::Client,
        Some(config),
    )
    .await;

    if events
        .send(Event::Opened {
            job,
            transport: super::Transport::WebSocket,
            // Always known: the 101 is what opened the socket.
            status: Some((status, status_text)),
            headers,
            protocol,
            elapsed: started.elapsed(),
        })
        .await
        .is_err()
    {
        return;
    }

    // **`connection_init` first, and nothing may be sent before the ack.** The protocol is
    // explicit that a client which subscribes early gets the connection closed, so the
    // subscribe below waits for `connection_ack` rather than pipelining.
    if graphql.is_some()
        && stream
            .send(Message::Text(r#"{"type":"connection_init"}"#.into()))
            .await
            .is_err()
    {
        let _ = events
            .send(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: "the socket closed before the GraphQL handshake".to_string(),
                },
            })
            .await;
        return;
    }

    // Set once the close handshake has been started from this side. The loop keeps *reading*
    // afterwards rather than returning: RFC 6455's close is an exchange, and dropping the
    // stream on the outgoing Close would deny the server the chance to answer — which is the
    // difference between a clean close and a reset that shows up in the server's logs.
    //
    // **But the wait is bounded.** Reading on after the Close is only correct while the peer
    // might still answer; a peer that never does used to keep this task and the socket alive
    // for the life of the process. `close_by` is the deadline, armed at the moment we ask.
    let mut closing = false;
    let mut close_by = tokio::time::Instant::now();

    // **The ack deadline, and the reason it is `settings.timeout`.** A graphql-transport-ws
    // server that accepts the socket and never answers `connection_init` is indistinguishable
    // from a slow one, and a protocol-layer auth rejection looks exactly like it — so without
    // this the transcript reads `open` forever against a server that has already refused.
    // `timeout` now means "answer within N", and an ack is the answer; a second number here
    // would be one more thing to explain and to get wrong.
    //
    // Starts `true` for a plain socket, which never sends `connection_init` and so must never
    // arm the branch.
    let ack_limit = spec.settings.timeout;
    let ack_by = tokio::time::Instant::now() + ack_limit.unwrap_or_default();
    let mut acked = graphql.is_none();

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
                        // A GraphQL socket carries envelopes, not payloads: what the person
                        // wants in the transcript is the `next` data, and the init/ack
                        // handshake is plumbing they did not ask for and cannot act on.
                        Message::Text(text) if graphql.is_some() => {
                            match step(&text) {
                                Step::Subscribe => {
                                    acked = true;
                                    let envelope = graphql
                                        .as_ref()
                                        .map(super::build::graphql_envelope)
                                        .transpose();
                                    let payload = match envelope {
                                        Ok(Some(payload)) => payload,
                                        Ok(None) | Err(_) => {
                                            let _ = events.send(Event::Failed {
                                                job,
                                                error: EngineError::Other {
                                                    reason: "the GraphQL operation could not be built".to_string(),
                                                },
                                            }).await;
                                            return;
                                        }
                                    };
                                    let subscribe = serde_json::json!({
                                        "id": "1",
                                        "type": "subscribe",
                                        "payload": payload,
                                    });
                                    if stream
                                        .send(Message::Text(subscribe.to_string().into()))
                                        .await
                                        .is_err()
                                    {
                                        let _ = events.send(Event::Closed {
                                            job,
                                            code: None,
                                            reason: String::new(),
                                        }).await;
                                        return;
                                    }
                                }
                                // The server's own keep-alive, which is not tungstenite's — it
                                // is a JSON envelope and has to be answered in kind.
                                Step::Pong => {
                                    if stream
                                        .send(Message::Text(r#"{"type":"pong"}"#.into()))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Step::Data(data) => {
                                    if events
                                        .send(Event::Frame {
                                            job,
                                            at: started.elapsed(),
                                            direction: Direction::Received,
                                            frame: Frame::Text(data),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Step::Done => {
                                    let _ = events.send(Event::Closed {
                                        job,
                                        code: None,
                                        reason: String::new(),
                                    }).await;
                                    return;
                                }
                                Step::Failed(reason) => {
                                    let _ = events.send(Event::Failed {
                                        job,
                                        error: EngineError::Other { reason },
                                    }).await;
                                    return;
                                }
                                Step::Ignore => {}
                            }
                        }
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
                    close_by = tokio::time::Instant::now() + CLOSE_GRACE;
                    if stream.close(None).await.is_err() {
                        let _ = events.send(Event::Closed { job, code: None, reason: String::new() }).await;
                        return;
                    }
                }
            },

            // **The peer had its chance to answer and did not take it.** Reported as an
            // ordinary close, not a failure: we asked to hang up and we have hung up, which is
            // exactly what was wanted — the peer's silence is its problem, not an error to put
            // a red pane over. `sleep_until` and not `sleep` because the branch is rebuilt on
            // every pass of the loop, so a relative delay would restart on each frame that
            // arrived during the wait and never fire.
            _ = tokio::time::sleep_until(close_by), if closing => {
                let _ = events.send(Event::Closed { job, code: None, reason: String::new() }).await;
                return;
            }

            _ = tokio::time::sleep_until(ack_by), if !acked && ack_limit.is_some() => {
                let _ = events.send(Event::Failed {
                    job,
                    // The same error a request that never answered reports, because it is the
                    // same thing: the socket opened, the question went out, nothing came back.
                    error: EngineError::Timeout { after: ack_limit.unwrap_or_default() },
                }).await;
                return;
            }
        }
    }
}

/// What one graphql-transport-ws envelope asks of the client.
enum Step {
    Subscribe,
    Pong,
    Data(String),
    Done,
    Failed(String),
    Ignore,
}

/// Read a server envelope.
///
/// Unknown types are ignored rather than refused: the protocol has grown types before and a
/// client that dies on one it has not heard of is a client that breaks when the server upgrades.
fn step(text: &str) -> Step {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Step::Ignore;
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("connection_ack") => Step::Subscribe,
        Some("ping") => Step::Pong,
        // The payload alone — `{"data": …}` — because the envelope is addressed to the client
        // and the data is addressed to the person.
        Some("next") => match value.get("payload") {
            Some(payload) => Step::Data(
                serde_json::to_string(payload).unwrap_or_else(|_| payload.to_string()),
            ),
            None => Step::Ignore,
        },
        Some("complete") => Step::Done,
        Some("error") => Step::Failed(
            value
                .get("payload")
                .map(|payload| payload.to_string())
                .unwrap_or_else(|| "the server rejected the operation".to_string()),
        ),
        _ => Step::Ignore,
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
        // Not reachable from the composer, which builds `Text`, and an SSE stream is
        // receive-only so nothing else produces one here. Mapped to its data rather than
        // refused so this stays total and a future "send this frame again" has an obvious
        // meaning, rather than a variant that silently cannot be resent.
        Frame::Event { data, .. } => Message::Text(data.into()),
    }
}
