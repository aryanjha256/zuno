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

pub async fn execute(job: JobId, client: Client, spec: RequestSpec, events: Sender<Event>) {
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
                wire: declared_length.unwrap_or(decoded),
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
                // Non-UTF-8 header values are legal on the wire; show them rather
                // than dropping the header.
                .unwrap_or_else(|_| format!("{:?}", value.as_bytes())),
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
