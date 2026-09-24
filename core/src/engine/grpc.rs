//! Making a gRPC call, as a job on the same engine as every other request.
//!
//! **The schema is read here, not in `build`.** Encoding a request message needs the compiled
//! `.proto`, which is a file on disk shared between requests — so `build::build_grpc` takes
//! already-encoded bytes and this module is what compiles, encodes and decodes around it. That
//! split is what keeps every builder in `build.rs` pure and unit-testable.
//!
//! **The status is in the trailers, and that is the whole shape of this file.** gRPC answers
//! *200 OK* to a call that failed — the real verdict is `grpc-status` in the HTTP/2 trailers,
//! which reqwest names nowhere in its public API. `core/tests/grpc_trailers.rs` proves the route
//! out: `From<Response> for http::Response<Body>`, then `http_body::Body`, whose frames are
//! either data or trailers. Reading the status off the HTTP status line instead would report
//! every failed call as a success, which is the one mistake here that produces a *confident*
//! wrong answer rather than an error.

use std::path::PathBuf;
use std::time::Instant;

use async_channel::Sender;
use bytes::Bytes;
use http_body_util::BodyExt;

use super::error::EngineError;
use super::run::collect_headers;
use super::{Direction, Event, Frame, JobId, Outbound};
use crate::grpc::{Method, Schema, reflection};
use crate::request::RequestSpec;
use crate::response::{ResponseData, SizeInfo, Timing};

/// Run one unary call.
///
/// Wrapped in a `Probe` scope for `run::execute`'s reason: the client — and with it the resolver
/// and the connector layer — is shared across jobs, so a task-local is what makes a measurement
/// belong to *this* call. Without it the timing would have to claim `Connection::Pooled`, which
/// is not "unknown" but a positive claim that a connection was reused.
pub async fn call(
    job: JobId,
    client: reqwest::Client,
    spec: RequestSpec,
    collection: Option<PathBuf>,
    events: Sender<Event>,
    outbound: tokio::sync::mpsc::UnboundedReceiver<Outbound>,
) {
    let probe = super::probe::Probe::new();
    super::probe::Probe::scope(
        probe.clone(),
        run_call(job, client, spec, collection, events, probe.clone(), outbound),
    )
    .await
}

async fn run_call(
    job: JobId,
    client: reqwest::Client,
    spec: RequestSpec,
    collection: Option<PathBuf>,
    events: Sender<Event>,
    probe: std::sync::Arc<super::probe::Probe>,
    mut outbound: tokio::sync::mpsc::UnboundedReceiver<Outbound>,
) {
    let started = Instant::now();
    let emit = |event: Event| {
        let _ = events.try_send(event);
    };

    emit(Event::Started { job });

    let Some(grpc) = spec.grpc() else {
        emit(Event::Failed {
            job,
            error: EngineError::Other {
                reason: "not a gRPC request".to_string(),
            },
        });
        return;
    };

    // **Compiled on every send**, deliberately. A `.proto` edited between two sends must change
    // what the next one encodes, and caching it would make the obvious debugging loop — fix the
    // schema, press Send — silently use the old one.
    let (schema, method) = match crate::grpc::prepare(grpc, collection.as_deref()) {
        Ok(prepared) => prepared,
        Err(error) => {
            emit(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: error.to_string(),
                },
            });
            return;
        }
    };

    let message = match schema.encode(&method, &grpc.message) {
        Ok(bytes) => bytes,
        Err(error) => {
            emit(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: error.to_string(),
                },
            });
            return;
        }
    };

    // **One owner for `outbound`, and one signal out of it.** Only a client-streaming call has
    // messages still to send, so only it needs `outbound` drained into the request body — but
    // *every* shape needs Disconnect to reach the reply loop. Handing the receiver to one task
    // and giving the loop a `stop` signal is what keeps those from competing for it; the reply
    // loop never sees `outbound` at all.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    // **Whether a half-close should also stop reading**, which is only a question for a
    // bidirectional call and is the one place these two halves are not independent.
    //
    // For a client-streaming call there is no reply stream to stop. For a server-streaming one
    // there is nothing to half-close, so Disconnect can only mean hang up. For bidirectional,
    // `Close` means *I have finished sending* — and stopping the read there silently truncates
    // whatever the server was still sending, which is a wrong answer on screen rather than an
    // inconvenience. So it half-closes and keeps listening; the server ends the stream with its
    // trailers, as the protocol says it will.
    //
    // The cost is that a bidirectional call has no hang-up short of closing the tab, which
    // `Engine::cancel` handles. A separate "done sending" verb would let Disconnect mean
    // Disconnect again; it is not worth inventing one until somebody wants it.
    //
    // **Every shape question below is asked of `method`, never of `grpc`.** `method` comes from
    // the schema compiled a few lines up; `grpc`'s two booleans are a copy taken when the method
    // was picked, kept so the UI can label its button without compiling. A `.proto` edited or
    // re-fetched since leaves that copy stale, and mixing the two sources in one function built a
    // streaming body for a unary method and hung until the read timeout. The copy is the UI's;
    // the wire follows the schema.
    let half_close_only = method.client_streaming && method.server_streaming;

    // **Dropping a oneshot sender resolves its receiver, and that counts as a signal.** The
    // feeder task ends the moment it has half-closed, so handing it `stop_tx` unconditionally
    // meant the reply loop was told to stop by the *end of the feeder* rather than by anything
    // asking it to — a bidirectional call read zero frames about half the time, and looked like
    // a clean close rather than a bug. Keeping the sender alive here for as long as the call is
    // what makes "nobody asked to stop" expressible at all.
    // Named `_keep_alive` because holding it *is* its only job: the value is never read, and
    // the compiler is right that it looks pointless. Dropping it early is the bug.
    let (_keep_alive, feeder_stop) = if half_close_only {
        (Some(stop_tx), None)
    } else {
        (None, Some(stop_tx))
    };

    let (request, _feeding) = if method.client_streaming {
        let (bytes_tx, bytes_rx) = tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();

        // The composed message goes first: pressing Send with something typed and having it
        // *not* be sent would be the surprising reading. This is the first message of the
        // conversation, not a draft.
        let _ = bytes_tx.send(bytes::Bytes::from(crate::grpc::frame(&message)));

        // `unfold` rather than a `tokio-stream` wrapper, which would be a dependency for one
        // adapter. `reqwest::Body::wrap_stream` wants a `TryStream`, hence the `Ok`.
        let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(
            bytes_rx,
            |mut rx| async move {
                rx.recv()
                    .await
                    .map(|chunk| (Ok::<bytes::Bytes, std::io::Error>(chunk), rx))
            },
        ));
        let request = match super::build::build_grpc_body(&client, &spec, grpc, body) {
            Ok(request) => request,
            Err(error) => {
                emit(Event::Failed { job, error });
                return;
            }
        };

        // **Opened *before* the request is sent, for either shape whose request stays open.**
        //
        // The connection is open for sending from this moment — that is what a streaming
        // request body means — but the response head may not arrive until the request *ends*,
        // because many servers answer only once they have the whole thing. Waiting for a status
        // before opening the transcript is waiting for something that will not come until the
        // person has finished, and they cannot finish: `send_frame` refuses without an open
        // session, and Disconnect without one *aborts* rather than half-closing. The call sat in
        // flight until the read timeout.
        //
        // This was first fixed for client-streaming alone, and bidirectional was left out on the
        // reasoning that its reply streams — but a streaming reply still cannot start before the
        // server has read enough, and against a buffering server that is all of it.
        //
        // Emitted before the feeder is spawned, not after: on a multi-threaded runtime the
        // feeder could otherwise report a `Sent` row first, and the app drops a row that arrives
        // with no transcript to put it in.
        //
        // `status: None` is not a missing value, it is the honest answer at this point; `Head`
        // fills it in when the server replies.
        emit(Event::Opened {
            job,
            transport: super::Transport::Grpc,
            status: None,
            headers: Vec::new(),
            protocol: None,
            elapsed: started.elapsed(),
        });

        // The composed message went into the body before the request was built, so its row is
        // reported here — after `Opened`, because the app has nowhere to put a frame until the
        // transcript exists.
        if !grpc.message.trim().is_empty() {
            emit(Event::Frame {
                job,
                at: started.elapsed(),
                direction: Direction::Sent,
                frame: Frame::Text(grpc.message.clone()),
            });
        }

        let schema_for_feed = schema.clone();
        let method_for_feed = method.clone();
        // **The transcript has to be told what went out, and nothing was telling it.** A
        // WebSocket reports every write as `Direction::Sent`; this path reported nothing, so a
        // message typed into the composer vanished — cleared from the box, encoded, sent down a
        // live request body, and invisible. It looked like the send had failed when it had
        // worked. The app also clears its outstanding-send marker on this event, so without it
        // every successful send was later announced as undelivered.
        let sent_events = events.clone();
        (
            request,
            Feeder(tokio::spawn(async move {
                while let Some(command) = outbound.recv().await {
                    match command {
                        Outbound::Frame(Frame::Text(json)) => {
                            // Encoded here because this is where the schema is. **A message
                            // that does not encode is refused alone**, not treated as the end of
                            // the conversation — see `Event::Rejected`.
                            let bytes = match schema_for_feed.encode(&method_for_feed, &json) {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    let _ = sent_events.try_send(Event::Rejected {
                                        job,
                                        text: json,
                                        reason: error.to_string(),
                                    });
                                    continue;
                                }
                            };
                            if bytes_tx
                                .send(bytes::Bytes::from(crate::grpc::frame(&bytes)))
                                .is_err()
                            {
                                break;
                            }
                            // After the write, never before — the same rule the socket follows,
                            // so a transcript cannot show a frame the connection refused.
                            let _ = sent_events.try_send(Event::Frame {
                                job,
                                at: started.elapsed(),
                                direction: Direction::Sent,
                                frame: Frame::Text(json),
                            });
                        }
                        Outbound::Close => break,
                        Outbound::Frame(_) => {}
                    }
                }
                // **Dropping the sender is the half-close**, which is how a client-streaming
                // call says "that was the last one" and the only thing that lets the server
                // answer. Then the reply loop is told, for a bidirectional call where it is
                // still reading.
                drop(bytes_tx);
                if let Some(stop_tx) = feeder_stop {
                    let _ = stop_tx.send(());
                }
            })),
        )
    } else {
        let request = match super::build::build_grpc(&client, &spec, grpc, &message) {
            Ok(request) => request,
            Err(error) => {
                emit(Event::Failed { job, error });
                return;
            }
        };
        // Nothing to send, so this task exists only to turn a Disconnect into the stop signal.
        (
            request,
            Feeder(tokio::spawn(async move {
                let _ = outbound.recv().await;
                if let Some(stop_tx) = feeder_stop {
                    let _ = stop_tx.send(());
                }
            })),
        )
    };

    let timeout = spec.settings.timeout;
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            emit(Event::Failed {
                job,
                error: EngineError::from_reqwest_grpc(&error, timeout),
            });
            return;
        }
    };

    let ttfb = started.elapsed();
    let status = response.status();
    let version = super::run::http_version(response.version());
    let headers = collect_headers(response.headers());

    emit(Event::Head {
        job,
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_string(),
        version,
        headers: headers.clone(),
        ttfb,
    });

    // The conversion the WebSocket upgrade uses for its extensions, used here for the body:
    // it is the only way to reach frames, and frames are the only way to reach trailers.
    let response: http::Response<reqwest::Body> = response.into();
    // Kept as a map for `verdict`: a Trailers-Only error carries its status here.
    let head = response.headers().clone();

    // **The `server` half decides this, not `is_session`.** A client-streaming call is a
    // session — its request body stays open for as long as you keep sending — and it still
    // answers exactly once, so reading it as a transcript would report a finished call as an
    // empty stream. Third time "is this a session" and "does this stream" have been the same
    // question in the code and different questions in fact; the other two were the ALPN choice
    // and the dispatch.
    //
    // Everything above — compile, encode, build, send, `Head` — is identical either way,
    // because the shape decides what to do with the *reply*, not how to ask.
    //
    if method.server_streaming {
        // A streaming request body opened the transcript already, above.
        let already_open = method.client_streaming;
        stream(
            job,
            response,
            schema,
            method,
            started,
            status.as_u16(),
            status_text_of(status),
            headers,
            already_open,
            &emit,
            stop_rx,
        )
        .await;
        return;
    }
    let collected = match response.into_body().collect().await {
        Ok(collected) => collected,
        Err(error) => {
            emit(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: format!("the reply could not be read: {error}"),
                },
            });
            return;
        }
    };

    let trailers = collected.trailers().cloned().unwrap_or_default();
    let body = collected.to_bytes();

    // **`grpc-status` outranks the HTTP status, always.** A gRPC error is *200 OK* with a
    // non-zero status trailer, so believing the status line reports every failure as a success.
    // Its absence from both the trailers and the head is itself a failure: that is not a gRPC
    // reply at all — most often a proxy or a plain HTTP server answering on the port.
    let (code, detail) = verdict(&trailers, &head);

    match code {
        Some(0) => {}
        Some(code) => {
            let name = status_name(code);
            emit(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: if detail.is_empty() {
                        format!("the call failed: {name} ({code})")
                    } else {
                        format!("the call failed: {name} ({code}) — {detail}")
                    },
                },
            });
            return;
        }
        None => {
            emit(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: format!(
                        "the server answered {} with no grpc-status trailer, so this is not a \
                         gRPC endpoint",
                        status.as_u16()
                    ),
                },
            });
            return;
        }
    }

    let messages = crate::grpc::unframe(&body);
    let Some(first) = messages.first() else {
        emit(Event::Failed {
            job,
            error: EngineError::Other {
                reason: "the call succeeded but carried no message".to_string(),
            },
        });
        return;
    };

    let decoded = match schema.decode(&method, first) {
        Ok(json) => json,
        Err(error) => {
            emit(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: error.to_string(),
                },
            });
            return;
        }
    };

    let total = started.elapsed();
    let decoded_len = decoded.len();
    emit(Event::Done {
        job,
        // **The decoded JSON is the body, not the protobuf bytes.** The response viewer's whole
        // value — folding, search, row selection, copy-as-path — is over a structure, and raw
        // protobuf is unreadable. The wire bytes are recoverable from `size.declared` being the
        // framed length; nobody has yet wanted them.
        response: Box::new(ResponseData {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or_default().to_string(),
            version,
            headers,
            // **The second metadata block, kept rather than dropped.** `grpc-status` was read
            // out of these above to decide whether the call succeeded; everything else was
            // being discarded, which threw away any trailing metadata the server sent. Passed
            // through whole, status keys included — a Trailers tab that omitted the keys we
            // happen to read would misrepresent the response.
            trailers: collect_headers(&trailers),
            body: Bytes::from(decoded),
            timing: Timing {
                connection: probe.connection(),
                ttfb,
                total,
            },
            size: SizeInfo {
                declared: Some(body.len() as u64),
                decoded: decoded_len as u64,
            },
        }),
    });
}

/// gRPC's canonical status codes, so a failure reads as a reason rather than a number.
fn status_name(code: i32) -> &'static str {
    match code {
        1 => "CANCELLED",
        2 => "UNKNOWN",
        3 => "INVALID_ARGUMENT",
        4 => "DEADLINE_EXCEEDED",
        5 => "NOT_FOUND",
        6 => "ALREADY_EXISTS",
        7 => "PERMISSION_DENIED",
        8 => "RESOURCE_EXHAUSTED",
        9 => "FAILED_PRECONDITION",
        10 => "ABORTED",
        11 => "OUT_OF_RANGE",
        12 => "UNIMPLEMENTED",
        13 => "INTERNAL",
        14 => "UNAVAILABLE",
        15 => "DATA_LOSS",
        16 => "UNAUTHENTICATED",
        _ => "UNKNOWN",
    }
}

/// The call's status and message, from wherever the server put them.
///
/// **Usually the trailers, but not always.** An error raised before any message exists —
/// unknown method, unauthenticated, permission denied — is sent as a *Trailers-Only* response:
/// one HEADERS frame carrying `grpc-status`, and no body. grpc-go and grpc-java both do this.
/// Reading only the trailers reported those as "not a gRPC endpoint" or "cut short", which is a
/// confident wrong answer about a server that answered correctly. The trailers win when both
/// carry a status, because they are the later and final word.
fn verdict(trailers: &http::HeaderMap, head: &http::HeaderMap) -> (Option<i32>, String) {
    let source = if trailers.contains_key("grpc-status") {
        trailers
    } else {
        head
    };
    let code = source
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i32>().ok());
    let detail = source
        .get("grpc-message")
        .and_then(|value| value.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    (code, detail)
}

/// Undo gRPC's percent-encoding of `grpc-message`.
///
/// **Not a URL escape, despite looking like one.** The spec restricts the trailer to printable
/// ASCII and percent-encodes everything else, so a server's message containing a newline or any
/// non-ASCII arrives as `%0A` or a run of `%C3%A9`. Decoding bytes rather than characters is what
/// makes multi-byte UTF-8 come back whole; a lossy conversion covers a server that encodes
/// something that is not UTF-8 at all.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`grpc-message` is percent-encoded**, which is easy to miss because it usually contains
    /// nothing that needs it — so the bug only appears on the errors that are hardest to read.
    #[test]
    fn a_status_message_is_percent_decoded() {
        assert_eq!(percent_decode("all good"), "all good");
        assert_eq!(percent_decode("line%0Abreak"), "line\nbreak");
        // Multi-byte UTF-8 arrives as a run of escapes and has to be decoded as *bytes*.
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        // A stray `%` that is not an escape is left alone rather than eating the next character.
        assert_eq!(percent_decode("100% sure"), "100% sure");
        assert_eq!(percent_decode("trailing%"), "trailing%");
    }

    #[test]
    fn a_status_code_reads_as_a_name() {
        assert_eq!(status_name(5), "NOT_FOUND");
        assert_eq!(status_name(12), "UNIMPLEMENTED");
        assert_eq!(status_name(16), "UNAUTHENTICATED");
        assert_eq!(status_name(99), "UNKNOWN");
    }
}

/// Stops the task that owns `outbound` when the call ends, whichever way it ends.
///
/// **A guard rather than an `abort()` at the end**, because `run_call` returns from a dozen
/// places — every failure arm is one — and a task left behind holds the command receiver for as
/// long as the job is remembered. Dropping a `JoinHandle` detaches rather than cancels, so
/// without this the tidying simply would not happen.
struct Feeder(tokio::task::JoinHandle<()>);

impl Drop for Feeder {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn status_text_of(status: reqwest::StatusCode) -> String {
    status.canonical_reason().unwrap_or_default().to_string()
}

/// Read a server-streaming call as a transcript.
///
/// **The same events a WebSocket and an SSE stream emit**, so the transcript — the row list, the
/// frame detail with its JSON viewer, copy, find, the eviction ring — works with nothing added.
/// That reuse is the third time it has paid off, and it is why the lifecycle was kept off the
/// kind: past `Opened`, the app cannot tell which transport produced a row.
///
/// **Frames are polled rather than streamed.** `Response::bytes_stream` would be the obvious
/// way and it silently drops the trailers, which is where gRPC's status lives — so a call that
/// failed halfway would end looking like a clean close. `BodyExt::frame` yields data *and*
/// trailer frames, which is the only shape that can report both.
#[allow(clippy::too_many_arguments)]
async fn stream(
    job: JobId,
    response: http::Response<reqwest::Body>,
    schema: Schema,
    method: Method,
    started: Instant,
    status: u16,
    status_text: String,
    headers: Vec<crate::request::Header>,
    already_open: bool,
    emit: &impl Fn(Event),
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    // **Not a second time.** A bidirectional call opened its transcript before sending, and a
    // second `Opened` would make the app start a fresh transcript — throwing away every message
    // already sent into it. `Head` has carried the status and headers there instead.
    if !already_open {
        emit(Event::Opened {
            job,
            transport: super::Transport::Grpc,
            // Known here: a streaming *reply* means the head has already arrived.
            status: Some((status, status_text)),
            headers,
            // gRPC negotiates no subprotocol; the field exists for a WebSocket's.
            protocol: None,
            elapsed: started.elapsed(),
        });
    }

    let head = response.headers().clone();
    let mut body = response.into_body();
    let mut reader = crate::grpc::Reader::default();
    let mut trailers: Option<http::HeaderMap> = None;

    loop {
        let next = tokio::select! {
            next = body.frame() => next,
            // **Disconnect.** Dropping the body is what actually stops the transfer; without a
            // path here the call carried on while the strip said it was disconnecting, which is
            // the bug SSE had before its own arm existed.
            //
            // A oneshot rather than the command channel, because on a client-streaming call
            // that channel is owned by the task feeding the request body — see `run_call`.
            _ = &mut stop => {
                emit(Event::Closed { job, code: None, reason: String::new() });
                return;
            }
        };

        let Some(next) = next else { break };

        let frame = match next {
            Ok(frame) => frame,
            Err(error) => {
                emit(Event::Failed {
                    job,
                    error: EngineError::Other {
                        reason: format!("the stream broke: {error}"),
                    },
                });
                return;
            }
        };

        // A frame is data or trailers, never both, and the trailers are always last.
        if let Some(chunk) = frame.data_ref() {
            for message in reader.push(chunk) {
                match schema.decode(&method, &message) {
                    Ok(json) => emit(Event::Frame {
                        job,
                        at: started.elapsed(),
                        direction: Direction::Received,
                        frame: Frame::Text(json),
                    }),
                    Err(error) => {
                        emit(Event::Failed {
                            job,
                            error: EngineError::Other {
                                reason: error.to_string(),
                            },
                        });
                        return;
                    }
                }
            }

            // Checked every push, because the reader reports a refusal through this rather than
            // through its return — which is the messages a chunk completed.
            if let Some(refused) = reader.refused() {
                emit(Event::Failed {
                    job,
                    error: EngineError::Other {
                        reason: refused.reason(),
                    },
                });
                return;
            }
        } else if let Some(map) = frame.trailers_ref() {
            trailers = Some(map.clone());
        }
    }

    // **The verdict arrives after the last message**, which is the whole reason a streaming call
    // cannot be judged by its status line: every frame above was delivered under a 200 that says
    // nothing about whether the call as a whole succeeded.
    let (code, detail) = verdict(&trailers.unwrap_or_default(), &head);

    match code {
        Some(0) => emit(Event::Closed {
            job,
            code: None,
            // Empty, because `session_line` reads a `None` code with a reason as a failure and
            // paints it red — the bug a `204` on an SSE stream had.
            reason: String::new(),
        }),
        Some(code) => emit(Event::Failed {
            job,
            error: EngineError::Other {
                reason: if detail.is_empty() {
                    format!("the call failed: {} ({code})", status_name(code))
                } else {
                    format!("the call failed: {} ({code}) — {detail}", status_name(code))
                },
            },
        }),
        None => emit(Event::Failed {
            job,
            error: EngineError::Other {
                reason: "the stream ended with no grpc-status trailer, so it was cut short \
                         rather than finished"
                    .to_string(),
            },
        }),
    }
}

/// Ask a server to describe itself, and return the schema as a `FileDescriptorSet`.
///
/// **One call per question, not one conversation.** Reflection is declared as a bidirectional
/// streaming RPC, so the obvious implementation is a single stream with questions and answers
/// interleaved — and that is what this used to be. It deadlocks: a server that buffers the whole
/// request before replying, which `grpc.postman-echo.com` does, waits for us to finish asking
/// while we wait for it to answer, and the read timeout fires thirty seconds later.
///
/// Sending one message, half-closing immediately and reading the reply works against both kinds
/// of server, and is what the clients that do work against that endpoint evidently do. It costs
/// one round trip per service, which for a schema fetched once is not worth a deadlock.
///
/// Returns encoded bytes rather than a `Schema` so the caller can write them into the
/// collection's `protos/`: reflection is then a one-time import, and the request works offline
/// afterwards.
pub async fn reflect(
    client: reqwest::Client,
    spec: RequestSpec,
    timeout: Option<std::time::Duration>,
) -> Result<Vec<u8>, EngineError> {
    // `v1` first, then the original name. A server speaking only the older one refuses with
    // `UNIMPLEMENTED`, which is a clean answer rather than a guess — verified against a real
    // server, which answers 12 on `v1` and 0 on `v1alpha`.
    let mut last: Option<EngineError> = None;

    'versions: for path in [reflection::V1_PATH, reflection::V1ALPHA_PATH] {
        let services = match ask(&client, &spec, path, reflection::list_services(), timeout).await
        {
            Ok(responses) => responses.into_iter().find_map(|response| match response {
                reflection::Response::Services(services) => Some(services),
                _ => None,
            }),
            Err(error) => {
                last = Some(error);
                continue;
            }
        };

        // `None` means the call succeeded and said nothing useful, which is how a server
        // without this reflection version answers. Try the next one.
        let Some(services) = services else {
            last = Some(EngineError::Other {
                reason: "the server answered reflection but listed no services".to_string(),
            });
            continue;
        };

        // **Reflection lists itself**, and asking for its own schema is noise in a picker
        // nobody wants to call it from.
        let wanted: Vec<String> = services
            .into_iter()
            .filter(|name| !name.starts_with("grpc.reflection."))
            .collect();

        if wanted.is_empty() {
            last = Some(EngineError::Other {
                reason: "the server offers no services beyond reflection itself".to_string(),
            });
            continue;
        }

        let mut files: Vec<Vec<u8>> = Vec::new();
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        // What the server said when it would not describe a symbol. Kept for the one case where
        // it is the whole story: every lookup refused, and nothing came back to load.
        let mut refusals: Vec<String> = Vec::new();

        for service in &wanted {
            let request = reflection::file_containing_symbol(service);
            // **Fall through to the next version, as the listing above does**, rather than `?`.
            // The two calls of one version failing differently is no reason to skip the other.
            let responses = match ask(&client, &spec, path, request, timeout).await {
                Ok(responses) => responses,
                Err(error) => {
                    last = Some(error);
                    continue 'versions;
                }
            };
            for response in responses {
                match response {
                    reflection::Response::Files(returned) => {
                        for file in returned {
                            // Asking by symbol returns the file *and every file it imports*, so
                            // the same descriptor comes back once per service that shares it.
                            if seen.insert(file.clone()) {
                                files.push(file);
                            }
                        }
                    }
                    // **An error is an answer, not a failure.** A server that does not know a
                    // symbol says `NOT_FOUND` inside a healthy call; treating that as the call
                    // failing would report "no such service" as "the server is broken".
                    reflection::Response::Error { code, message } => refusals.push(
                        if message.is_empty() {
                            format!("{service}: {} ({code})", status_name(code))
                        } else {
                            format!("{service}: {message}")
                        },
                    ),
                    _ => {}
                }
            }
        }

        if files.is_empty() {
            let reason = match refusals.first() {
                // The server's own words, which say why — "the server returned nothing" does
                // not, and was all this used to report.
                Some(first) => format!(
                    "the server listed services but would not describe them — {first}"
                ),
                None => "the server listed services but returned no descriptors for them"
                    .to_string(),
            };
            last = Some(EngineError::Other { reason });
            continue;
        }

        return Ok(reflection::descriptor_set(&files));
    }

    Err(last.unwrap_or_else(|| EngineError::Other {
        reason: "the server does not support reflection".to_string(),
    }))
}

/// One complete reflection call: send a single message, half-close, read the answers.
///
/// The body is a plain `Vec<u8>` rather than a channel, which *is* the half-close — a complete
/// body ends the request stream the moment it is sent, so the server is free to answer
/// immediately whether it streams or buffers.
async fn ask(
    client: &reqwest::Client,
    spec: &RequestSpec,
    path: &str,
    message: Vec<u8>,
    timeout: Option<std::time::Duration>,
) -> Result<Vec<reflection::Response>, EngineError> {
    let url = super::build::grpc_url(spec, path)?;

    let request = client
        .post(url)
        .headers(super::build::grpc_headers(spec)?)
        .body(crate::grpc::frame(&message))
        .build()
        .map_err(|error| EngineError::Build {
            reason: error.to_string(),
        })?;

    let response = client
        .execute(request)
        .await
        .map_err(|error| EngineError::from_reqwest_grpc(&error, timeout))?;

    let response: http::Response<reqwest::Body> = response.into();
    let head = response.headers().clone();
    let collected = response
        .into_body()
        .collect()
        .await
        .map_err(|error| EngineError::Other {
            reason: format!("the reflection reply could not be read: {error}"),
        })?;

    let trailers = collected.trailers().cloned().unwrap_or_default();
    let body = collected.to_bytes();

    let (code, _) = verdict(&trailers, &head);

    // `UNIMPLEMENTED` is how a server refuses a reflection version it does not host, and it is
    // the ordinary case rather than a fault — `reflect` tries the other one. Anything else
    // non-zero is a real failure and is named.
    match code {
        Some(0) | None => {}
        Some(12) => {
            return Err(EngineError::Other {
                reason: format!("this server does not implement {path}"),
            });
        }
        Some(code) => {
            return Err(EngineError::Other {
                reason: format!("reflection failed: {} ({code})", status_name(code)),
            });
        }
    }

    Ok(crate::grpc::unframe(&body)
        .into_iter()
        .map(reflection::response)
        .collect())
}
