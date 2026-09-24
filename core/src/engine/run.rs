//! Executing one request and reporting it as a stream of events.
//!
//! The events matter as much as the result (architecture.md §4): `Head` fires at TTFB
//! so the status line can paint before the last byte lands, and `Progress` keeps a
//! large download from looking like a frozen window. A single `Future<Response>` could
//! only ever render "spinner, then everything".

use std::pin::pin;
use std::time::{Duration, Instant};

use async_channel::Sender;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::Client;

use crate::engine::build;
use crate::engine::error::EngineError;
use crate::engine::probe::Probe;
use crate::engine::{Direction, Event, Frame, JobId};
use crate::request::{Header, RequestSpec};
use crate::response::{HttpVersion, ResponseData, SizeInfo, Timing};

/// How long to wait before picking a dropped stream back up, when the server named no
/// `retry:` of its own. Matches what a browser's `EventSource` uses.
const DEFAULT_RETRY: Duration = Duration::from_secs(3);
/// A ceiling on the backoff, so a server asking for a ten-minute retry cannot park a stream
/// somewhere a person will never see it resume.
const MAX_RETRY: Duration = Duration::from_secs(30);
/// Consecutive failed attempts before giving up. Reset by any connection that delivers an
/// event, so a long-lived stream that blips occasionally never exhausts it.
const MAX_RECONNECTS: u32 = 6;

/// Cap the pre-allocation from a server-declared Content-Length. A hostile or broken
/// `Content-Length: 99999999999` should not decide how much memory we reserve.
const MAX_PREALLOC: usize = 8 * 1024 * 1024;

/// Minimum gap between `Progress` events. The UI cannot paint faster than a frame, so
/// emitting per-chunk would flood the channel with events that get coalesced anyway.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(33);

/// Largest response body Zuno will buffer, and the value `Engine` runs with.
///
/// **A memory limit on the transfer, not on the display.** `body_view::MAX_AUTO_PARSE` is the
/// display one, and it declines to *index* 10MB+ while still showing the bytes. This one is about
/// holding the bytes at all: the stream was collected into an unbounded `Vec<u8>`, so a mistyped
/// URL pointing at a release artifact instead of an API endpoint buffered the whole thing, and
/// `HISTORY_LIMIT` retained up to eleven of them per buffer.
///
/// **Fails rather than truncating**, unlike `MAX_DISPLAY_LINE`, which truncates for display while
/// keeping every byte. A truncated body is not the response: `SaveResponse` would write a corrupt
/// file from it and the JSON viewer would report a parse error at the cut. Being told the transfer
/// was refused is more useful than either.
///
/// 100MB is a policy guess — ten times the parse cap, far past any JSON an API returns on purpose,
/// and far below the point where buffering it hurts. If a legitimate download ever needs more, this
/// belongs in `RequestSettings` rather than as a larger constant.
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Run one request.
///
/// `max_body_bytes` is a parameter rather than a constant read inside, so a test can drive the
/// streaming guard with a small limit instead of pushing 100MB through a socket. `Engine` always
/// passes `MAX_BODY_BYTES`.
pub async fn execute(
    job: JobId,
    client: Client,
    spec: RequestSpec,
    events: Sender<Event>,
    max_body_bytes: usize,
    outbound: tokio::sync::mpsc::UnboundedReceiver<super::Outbound>,
) {
    // Install this job's connection probe for the duration of the task. The client — and with
    // it the resolver and the connector layer — is shared across jobs, so a task-local is what
    // makes a measurement belong to *this* request. See `probe.rs`.
    let probe = Probe::new();
    Probe::scope(
        probe.clone(),
        run(job, client, spec, events, max_body_bytes, probe.clone(), outbound),
    )
    .await
}

async fn run(
    job: JobId,
    client: Client,
    spec: RequestSpec,
    events: Sender<Event>,
    max_body_bytes: usize,
    probe: std::sync::Arc<Probe>,
    // Read only if this response turns out to be a stream. A request that stays a request
    // simply drops it, which is why it costs nothing to hand one to every job.
    outbound: tokio::sync::mpsc::UnboundedReceiver<super::Outbound>,
) {
    let started = Instant::now();
    let timeout = spec.settings.timeout;

    // Unbounded channel: `try_send` never blocks, so a slow UI can't stall the
    // network task, and this stays callable from a tokio thread while the receiver
    // lives on gpui's smol executor.
    let emit = |event: Event| {
        let _ = events.try_send(event);
    };

    emit(Event::Started { job });

    let request = match build::build(&client, &spec) {
        Ok(request) => request,
        Err(error) => {
            emit(Event::Failed { job, error });
            return;
        }
    };

    // **The deadline is on the answer, not on the exchange.** `build` no longer sets
    // reqwest's request timeout, which covered the body and so could never be met by a stream.
    // This is the first half of what `settings.timeout` means now — "answer within N" — and
    // `ClientKey::read_timeout` is the second, "do not go silent for N". A response that keeps
    // arriving is allowed to take as long as it takes, which is the only way a subscription
    // can work and is what most clients already mean by a timeout.
    let response = match timeout {
        Some(limit) => match tokio::time::timeout(limit, client.execute(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                emit(Event::Failed {
                    job,
                    error: EngineError::from_reqwest(&error, timeout),
                });
                return;
            }
            // An elapse carries no reqwest error to classify, and none is needed: the answer
            // never came, and `limit` is exactly how long it was given.
            Err(_) => {
                emit(Event::Failed {
                    job,
                    error: EngineError::Timeout { after: limit },
                });
                return;
            }
        },
        None => match client.execute(request).await {
            Ok(response) => response,
            Err(error) => {
                emit(Event::Failed {
                    job,
                    error: EngineError::from_reqwest(&error, timeout),
                });
                return;
            }
        },
    };

    let ttfb = started.elapsed();
    // Read before `bytes_stream` consumes the response. This one header is the whole of SSE
    // detection, and it is why the decision belongs to the *response* rather than the request:
    // an ordinary HTTP request and a GraphQL subscription both become streams here, and
    // neither could have promised it beforehand.
    let event_stream = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    let status = response.status();
    let version = http_version(response.version());
    let headers = collect_headers(response.headers());
    // None once reqwest has transparently decompressed the body.
    let declared_length = response.content_length();

    emit(Event::Head {
        job,
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_string(),
        version,
        headers: headers.clone(),
        ttfb,
    });

    // Refuse before transferring anything, when the server declares more than we will hold.
    // The streaming check below is the real guard — a declared length is a claim, and a chunked
    // response makes none — but this saves pulling down bytes that are going to be rejected.
    // Emitted after `Head` on purpose: the status line and the `Content-Length` header are worth
    // seeing, and they are what make the failure make sense.
    if let Some(declared) = declared_length
        && declared > max_body_bytes as u64
    {
        emit(Event::Failed {
            job,
            error: EngineError::BodyTooLarge {
                limit: max_body_bytes,
                size: declared as usize,
            },
        });
        return;
    }

    if event_stream {
        stream_events(
            job,
            client,
            spec,
            response,
            started,
            status,
            headers,
            max_body_bytes,
            &emit,
            outbound,
        )
        .await;
        return;
    }

    let mut buffer: Vec<u8> = Vec::with_capacity(
        declared_length
            .map(|len| len.min(MAX_PREALLOC as u64) as usize)
            .unwrap_or(16 * 1024),
    );

    let mut stream = pin!(response.bytes_stream());
    let mut last_progress = Instant::now();

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                buffer.extend_from_slice(&chunk);
                // Checked after appending rather than before, so the limit is a ceiling on what we
                // hold and not on what we accept: one chunk of overshoot is bounded and cheap,
                // while a pre-check would need the chunk size to reason about.
                if buffer.len() > max_body_bytes {
                    emit(Event::Failed {
                        job,
                        error: EngineError::BodyTooLarge {
                            limit: max_body_bytes,
                            size: buffer.len(),
                        },
                    });
                    return;
                }
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    last_progress = Instant::now();
                    emit(Event::Progress {
                        job,
                        received: buffer.len(),
                        total: declared_length.map(|len| len as usize),
                    });
                }
            }
            Err(error) => {
                emit(Event::Failed {
                    job,
                    error: EngineError::from_reqwest(&error, timeout),
                });
                return;
            }
        }
    }

    let total = started.elapsed();
    let decoded = buffer.len() as u64;

    emit(Event::Done {
        job,
        response: Box::new(ResponseData {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or_default().to_string(),
            version,
            headers,
            trailers: Vec::new(),
            body: Bytes::from(buffer),
            timing: Timing {
                // Read here rather than at `Head`: a redirect chain opens its sockets across
                // the whole send, so asking at TTFB would report only the first hop's setup
                // and leave the rest misattributed to `Wait`.
                connection: probe.connection(),
                ttfb,
                total,
            },
            size: SizeInfo {
                // Straight through rather than defaulted to `decoded`: a missing declaration is
                // information, and collapsing it made `declared == decoded` indistinguishable
                // from "the server never said".
                declared: declared_length,
                decoded,
            },
        }),
    });
}

/// How a connection's pump ended.
///
/// **Finished and Dropped are not the same thing**, and collapsing them was a bug: SSE has no
/// in-band "that is all", so every close looked like a drop and a completed subscription
/// restarted. A server that stops writing and closes has said everything it intends to.
enum Ended {
    /// The server closed the stream cleanly.
    Finished,
    /// The connection failed mid-stream — a read error, or the idle timeout. This is the case a
    /// resume exists for.
    Dropped,
    /// The person pressed Disconnect.
    Asked,
    /// One event grew past the body limit. Not resumable — a replay from the same id sends the
    /// same oversized event again — so this ends the stream rather than dropping it.
    TooLarge(usize),
}

/// Read a `text/event-stream` as a session instead of a body, reconnecting when it drops.
///
/// **The same events a WebSocket emits**, so the whole transcript — the row list, the frame
/// detail with its JSON viewer, copy, find — works with nothing added for SSE. That reuse is
/// the payoff for keeping lifecycle off the kind: a socket promises a session up front, an HTTP
/// response turns into one, and past this point the app cannot tell them apart.
///
/// `Opened` after `Head` is deliberate redundancy. `Head` has already painted the status line
/// and the headers; `Opened` is what tells the app a transcript exists, and sending both means
/// no arm anywhere has to special-case which kind it came from.
///
/// **Reconnection lives here and only here.** SSE is the one transport that can resume: the
/// protocol defines `retry:` and replay from `Last-Event-ID`, so picking a dropped stream back
/// up loses nothing. It is also why the loop keeps one parser across connections — the id it
/// resumes from is stream state, while a half-read line belongs to the connection that died.
#[allow(clippy::too_many_arguments)]
async fn stream_events(
    job: JobId,
    client: reqwest::Client,
    spec: RequestSpec,
    mut response: reqwest::Response,
    started: Instant,
    status: reqwest::StatusCode,
    headers: Vec<Header>,
    max_body_bytes: usize,
    emit: &impl Fn(Event),
    mut outbound: tokio::sync::mpsc::UnboundedReceiver<super::Outbound>,
) {
    emit(Event::Opened {
        job,
        transport: crate::engine::Transport::EventStream,
        // Always known here: a stream is recognised from the response head, so the head has
        // already arrived by the time this is emitted.
        status: Some((
            status.as_u16(),
            status.canonical_reason().unwrap_or_default().to_string(),
        )),
        headers,
        // An SSE stream negotiates nothing — the field exists for a WebSocket's subprotocol.
        protocol: None,
        elapsed: started.elapsed(),
    });

    // **The one guard the streaming path does not inherit.** `max_body_bytes` above watches a
    // buffer that a stream never fills, because a stream is not held — so the same number has to
    // bound what a single event may accumulate instead, or SSE is the one transport in the app
    // with no ceiling at all.
    let mut parser = crate::sse::Parser::with_limit(max_body_bytes);
    let mut attempt = 0u32;

    loop {
        let (ended, delivered) =
            pump(job, response, &mut parser, started, emit, &mut outbound).await;

        match ended {
            Ended::Asked => {
                emit(Event::Closed { job, code: None, reason: String::new() });
                return;
            }
            // **A clean close ends the stream.** It used to reconnect, which meant a graphql-sse
            // subscription — deliver the events, close — restarted every single time. `finish`
            // belongs here and nowhere else: a server that closed without the final blank line
            // has still said something, while before a *retry* the same call would emit an event
            // the replay is about to send again.
            Ended::Finished => {
                if let Some(event) = parser.finish() {
                    emit(frame_for(job, started, event));
                }
                emit(Event::Closed { job, code: None, reason: String::new() });
                return;
            }
            // Reported as a failure and not retried: the same `Last-Event-ID` would ask for the
            // same oversized event back.
            Ended::TooLarge(size) => {
                emit(Event::Failed {
                    job,
                    error: EngineError::BodyTooLarge { limit: max_body_bytes, size },
                });
                return;
            }
            // A read error or the idle timeout. Not reported as a failure — a red pane over
            // something the resume is about to fix — and the attempt cap decides when it is
            // really over.
            Ended::Dropped => {}
        }

        // **Only a connection that delivered something counts as good.** Resetting on connect
        // alone would let a server that accepts and immediately hangs up be retried forever,
        // which is exactly the flapping this cap exists to surface.
        if delivered {
            attempt = 0;
        }

        // Keep trying until one connection sticks, or the cap runs out. A refused reconnect is
        // not the end — the server may be restarting, which is the ordinary reason the stream
        // dropped in the first place — so failures are attempts too rather than a return.
        response = loop {
            attempt += 1;
            if attempt > MAX_RECONNECTS {
                emit(Event::Closed {
                    job,
                    code: None,
                    reason: format!("gave up after {MAX_RECONNECTS} attempts"),
                });
                return;
            }

            // The server's own `retry:` if it sent one, backed off by the attempt so a dead
            // endpoint is not hammered, and capped so a huge `retry:` cannot park the stream
            // somewhere nobody will see it resume.
            let base = parser
                .retry
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_RETRY);
            let delay = (base * attempt).min(MAX_RETRY);

            emit(Event::Reconnecting { job, attempt, delay });

            // Disconnect has to win the wait, or pressing it during a backoff does nothing for
            // however long the delay happens to be.
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = outbound.recv() => {
                    emit(Event::Closed { job, code: None, reason: String::new() });
                    return;
                }
            }

            parser.reset_between_connections();

            match reconnect(&client, &spec, parser.last_id()).await {
                Retry::Stream(response) => break response,
                // **The same ending as `Ended::Finished`, so it has to carry the same empty
                // reason.** `session_line` reads a `None` code *with* a reason as a failure and
                // paints it red — that is where `Event::Failed` puts its error — so wording this
                // one made a server politely saying "do not come back" render as `failed — the
                // server ended the stream`. A `204` is the most orderly end SSE has.
                Retry::Finished => {
                    emit(Event::Closed { job, code: None, reason: String::new() });
                    return;
                }
                Retry::Failed => {}
            }
        };
    }
}

/// Read one connection until it ends. Reports how, and whether anything arrived.
async fn pump(
    job: JobId,
    response: reqwest::Response,
    parser: &mut crate::sse::Parser,
    started: Instant,
    emit: &impl Fn(Event),
    outbound: &mut tokio::sync::mpsc::UnboundedReceiver<super::Outbound>,
) -> (Ended, bool) {
    let mut stream = pin!(response.bytes_stream());
    let mut delivered = false;

    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => match chunk {
                Some(chunk) => chunk,
                None => return (Ended::Finished, delivered),
            },
            // **Disconnect.** SSE has no closing handshake — the connection is simply dropped —
            // so this returns rather than negotiating, and says so through the same `Closed`
            // event a socket ends with. Dropping `stream` here is what actually stops the
            // transfer; without a path to this point the request carried on while the UI said
            // it was disconnecting.
            _ = outbound.recv() => return (Ended::Asked, delivered),
        };

        match chunk {
            Ok(chunk) => {
                for event in parser.push(&chunk) {
                    delivered = true;
                    emit(frame_for(job, started, event));
                }
                // Checked every push: the parser reports an overflow through this rather than
                // through its return, which is the events a chunk completed.
                if let Some(size) = parser.too_large() {
                    return (Ended::TooLarge(size), delivered);
                }
            }
            // A read that failed or an idle timeout: the connection broke rather than the
            // stream ending, which is the only case worth resuming.
            Err(_) => return (Ended::Dropped, delivered),
        }
    }
}

/// What a reconnect attempt produced.
enum Retry {
    Stream(reqwest::Response),
    /// The server said not to come back — a `204`, which is how SSE says "this is over".
    Finished,
    Failed,
}

/// Re-issue the request, asking the server to replay from where the stream stopped.
async fn reconnect(
    client: &reqwest::Client,
    spec: &RequestSpec,
    last_id: Option<&str>,
) -> Retry {
    let Ok(mut request) = build::build(client, spec) else {
        return Retry::Failed;
    };

    // **The whole reason a reconnect is lossless.** A server that honours it replays everything
    // after this id, so the gap closes; one that ignores it simply starts fresh, which is no
    // worse than not asking.
    if let Some(id) = last_id
        && let Ok(value) = http::HeaderValue::from_str(id)
    {
        request.headers_mut().insert("last-event-id", value);
    }

    let Ok(response) = client.execute(request).await else {
        return Retry::Failed;
    };

    // **`204 No Content` is the spec's way of ending a stream for good**, and an SSE client that
    // ignores it reconnects forever against a server politely saying stop. It is the only
    // in-band "do not come back" the protocol has, because a plain close is indistinguishable
    // from a dropped connection — which is exactly why the reconnect exists.
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Retry::Finished;
    }
    if response.status().is_client_error() || response.status().is_server_error() {
        return Retry::Failed;
    }

    Retry::Stream(response)
}

fn frame_for(job: JobId, started: Instant, event: crate::sse::Event) -> Event {
    Event::Frame {
        job,
        at: started.elapsed(),
        direction: Direction::Received,
        frame: Frame::Event {
            name: event.name,
            id: event.id,
            data: event.data,
        },
    }
}

/// Collect response headers into our ordered representation.
///
/// **Known limitation:** `http::HeaderMap` does not preserve wire order across
/// different names — its iteration order is an implementation detail. Duplicates of
/// the *same* name do stay in received order, so a stable sort by name gives
/// deterministic, readable output without scrambling those. True wire order would
/// require a lower-level client than reqwest.
pub(crate) fn collect_headers(headers: &http::HeaderMap) -> Vec<Header> {
    let mut collected: Vec<Header> = headers
        .iter()
        .map(|(name, value)| Header {
            enabled: true,
            name: name.as_str().to_string(),
            value: value
                .to_str()
                .map(str::to_string)
                // Non-UTF-8 header values are legal on the wire, so show them rather than dropping
                // the header — but *lossily decoded*, not debug-printed. The common real case is a
                // latin-1 filename in `Content-Disposition`, where `caf\u{fffd}.txt` is readable
                // and `[99, 97, 102, 233, 46, 116, 120, 116]` is not.
                .unwrap_or_else(|_| String::from_utf8_lossy(value.as_bytes()).into_owned()),
        })
        .collect();

    collected.sort_by(|a, b| a.name.cmp(&b.name));
    collected
}

pub(crate) fn http_version(version: reqwest::Version) -> HttpVersion {
    match version {
        reqwest::Version::HTTP_09 => HttpVersion::Http09,
        reqwest::Version::HTTP_10 => HttpVersion::Http10,
        reqwest::Version::HTTP_11 => HttpVersion::Http11,
        reqwest::Version::HTTP_2 => HttpVersion::Http2,
        reqwest::Version::HTTP_3 => HttpVersion::Http3,
        _ => HttpVersion::Http11,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve a response with **no `Content-Length`** — framed by closing the connection — carrying
    /// roughly `bytes` bytes of body.
    ///
    /// The missing length is what makes this exercise the *streaming* guard: with a declared length
    /// the pre-check would fire first and the loop would never run.
    fn serve_unbounded_body(bytes: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 1024];
                let _ = stream.read(&mut discard);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: application/octet-stream\r\n\
                      Connection: close\r\n\
                      \r\n",
                );
                let chunk = vec![b'x'; 8 * 1024];
                let mut sent = 0;
                while sent < bytes {
                    // The client hangs up the moment it gives up, so a write error here is the
                    // expected end of this thread rather than a problem.
                    if stream.write_all(&chunk).is_err() {
                        break;
                    }
                    sent += chunk.len();
                }
                let _ = stream.flush();
            }
        });

        format!("http://{addr}")
    }

    #[test]
    fn a_streamed_body_past_the_limit_fails_instead_of_buffering_without_bound() {
        // Driven with a 64KB limit rather than the real 100MB, which is the whole reason
        // `max_body_bytes` is a parameter: the guard is what needs testing, not the policy number.
        const LIMIT: usize = 64 * 1024;

        let base = serve_unbounded_body(LIMIT * 4);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let (sender, events) = async_channel::unbounded();
        let spec = RequestSpec {
            url: format!("{base}/big"),
            ..RequestSpec::default()
        };

        let (_close, closes) = tokio::sync::mpsc::unbounded_channel();
        runtime.block_on(execute(JobId(1), Client::new(), spec, sender, LIMIT, closes));

        let collected: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        let failure = collected
            .iter()
            .find_map(|event| match event {
                Event::Failed { error, .. } => Some(error),
                _ => None,
            })
            .expect("a Failed event");

        assert!(
            matches!(
                failure,
                EngineError::BodyTooLarge { limit, size } if *limit == LIMIT && *size > LIMIT
            ),
            "{failure:?}"
        );
        // The overshoot is bounded by one chunk, not by however much the server had left to send.
        // Without the guard this collects the lot, which is the failure being prevented.
        assert!(
            matches!(failure, EngineError::BodyTooLarge { size, .. } if *size < LIMIT * 4),
            "should have stopped near the limit, not read the whole body: {failure:?}"
        );
        assert!(
            !collected.iter().any(|e| matches!(e, Event::Done { .. })),
            "a refused body must not also report success"
        );
    }
}
