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
use crate::engine::{Event, JobId};
use crate::request::{Header, RequestSpec};
use crate::response::{HttpVersion, ResponseData, SizeInfo, Timing};

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

    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            emit(Event::Failed {
                job,
                error: EngineError::from_reqwest(&error, timeout),
            });
            return;
        }
    };

    let ttfb = started.elapsed();
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
            body: Bytes::from(buffer),
            timing: Timing {
                // reqwest doesn't expose per-stage connection timings; getting DNS,
                // connect, and TLS separately needs a custom hyper connector. The
                // model already types them as Option for exactly this reason.
                dns: None,
                connect: None,
                tls: None,
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

/// Collect response headers into our ordered representation.
///
/// **Known limitation:** `http::HeaderMap` does not preserve wire order across
/// different names — its iteration order is an implementation detail. Duplicates of
/// the *same* name do stay in received order, so a stable sort by name gives
/// deterministic, readable output without scrambling those. True wire order would
/// require a lower-level client than reqwest.
fn collect_headers(headers: &http::HeaderMap) -> Vec<Header> {
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

fn http_version(version: reqwest::Version) -> HttpVersion {
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

        runtime.block_on(execute(JobId(1), Client::new(), spec, sender, LIMIT));

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
