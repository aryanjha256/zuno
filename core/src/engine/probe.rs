//! Measuring what a request spent before its first byte.
//!
//! reqwest reports TTFB and total by arithmetic on our side and nothing else, so `Timing`'s
//! connection stages were hardcoded `None` from M1.2 until the timeline wanted them. Two hooks
//! in reqwest 0.13 are enough to fill them in, and neither is a custom connector:
//!
//! - **`ClientBuilder::dns_resolver`** takes a `reqwest::dns::Resolve`. Wrapping it times the
//!   lookup.
//! - **`ClientBuilder::connector_layer`** takes a tower `Layer` around the connector service.
//!   One `call` is one connection establishment, so its span is TCP + TLS.
//!
//! **What this cannot do, and why there is no `tls` field any more.** The connector does the
//! TCP connect and the TLS handshake together and exposes no seam between them, so the layer
//! sees one span covering both. Separating them means replacing reqwest's connector with a
//! hand-built one over `hyper-util` plus `rustls` — more work than the whole rest of the
//! feature, to split one bar. `Connection::Opened::connect` is therefore named for what it
//! contains rather than pretending to be just the TCP half.
//!
//! ## Attribution
//!
//! The client is cached per `ClientKey` and shared by every job, so the resolver and the layer
//! are shared too and a measurement has to find its way back to *one* request. A
//! **tokio task-local** does it: `run::execute` installs a fresh `Probe` for the job's task, and
//! both hooks write into whatever probe their task is running under.
//!
//! That works because of where hyper polls the connector. `hyper_util`'s legacy client does
//! `future::select(checkout, connect).await` in `connection_for` — both halves are polled by the
//! **caller's** task, not a spawned one, so the task-local is in scope. Verified by reading
//! `hyper-util-0.1.20/src/client/legacy/client.rs`, not assumed; if a future version spawns the
//! connect instead, `try_with` starts failing and every connection silently reads as `Pooled`,
//! which is what `a_second_request_reuses_its_socket` would catch.
//!
//! ## The two races, both of which resolve the right way
//!
//! **The pool can win.** That same `select` may start connecting and then find an idle
//! connection first, in which case the half-built connection is *spawned* to finish in the
//! background — outside the task-local, so nothing is recorded. That is the correct answer: the
//! request did travel on a pooled socket. It does mean a DNS lookup can be recorded for a
//! connection that was then abandoned, which is why `connection()` keys entirely off whether a
//! socket completed and ignores a stray lookup.
//!
//! **Redirects can open several.** Following a redirect to another host opens another socket, so
//! both counters *sum* rather than keeping the first or the last. Summing is what keeps the
//! phases honest: the elapsed time really did contain all of them, and attributing only the
//! first would quietly move the rest into `Wait`, where it reads as a slow server. `sockets` is
//! carried out so the pane can say a request took more than one, since reqwest surfaces only
//! the final response and the hops are otherwise invisible.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tower_layer::Layer;
use tower_service::Service;

use crate::response::Connection;

tokio::task_local! {
    /// The probe belonging to the job running on this task. Absent outside `Probe::scope`,
    /// which is why every write goes through `try_with` and does nothing rather than panicking.
    static PROBE: Arc<Probe>;
}

/// Counters for one request's connection setup.
///
/// Atomics rather than a mutex because the writers are two independent futures and the values
/// are two integers — and `Ordering::Relaxed` is enough since nothing here orders against
/// anything else: the reader runs after both writers have finished, on the same task.
#[derive(Debug, Default)]
pub(crate) struct Probe {
    dns_nanos: AtomicU64,
    lookups: AtomicU32,
    connect_nanos: AtomicU64,
    sockets: AtomicU32,
}

impl Probe {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Run `future` with `probe` installed as the task's probe.
    pub(crate) fn scope<F: Future>(
        probe: Arc<Self>,
        future: F,
    ) -> impl Future<Output = F::Output> {
        PROBE.scope(probe, future)
    }

    /// What the counters add up to.
    ///
    /// **`sockets` is the only thing that decides between `Opened` and `Pooled`**, deliberately.
    /// A lookup with no completed socket is the abandoned-connection race above, and reporting
    /// `Opened { dns: Some(..), connect: 0 }` for it would draw a handshake that never happened
    /// onto a request that reused its connection.
    pub(crate) fn connection(&self) -> Connection {
        let sockets = self.sockets.load(Ordering::Relaxed);
        if sockets == 0 {
            return Connection::Pooled;
        }

        Connection::Opened {
            // Zero lookups means there was no name to resolve — an IP-literal URL. That is a
            // different fact from "resolved instantly", so it is `None` and not `Some(0)`.
            dns: (self.lookups.load(Ordering::Relaxed) > 0)
                .then(|| Duration::from_nanos(self.dns_nanos.load(Ordering::Relaxed))),
            // The layer's span *contains* the lookup, since the connector calls the resolver
            // itself. Subtracting here rather than in the pane keeps the phases summing to the
            // total in one place; `saturating_sub` because the two spans are taken by different
            // futures and a nanosecond of overlap must not wrap a `Duration`.
            connect: Duration::from_nanos(
                self.connect_nanos
                    .load(Ordering::Relaxed)
                    .saturating_sub(self.dns_nanos.load(Ordering::Relaxed)),
            ),
            sockets,
        }
    }
}

fn record_dns(elapsed: Duration) {
    let _ = PROBE.try_with(|probe| {
        probe
            .dns_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
        probe.lookups.fetch_add(1, Ordering::Relaxed);
    });
}

fn record_socket(elapsed: Duration) {
    let _ = PROBE.try_with(|probe| {
        probe
            .connect_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
        probe.sockets.fetch_add(1, Ordering::Relaxed);
    });
}

/// A resolver that times what it looks up.
///
/// **This replaces reqwest's `GaiResolver` rather than wrapping it**, because that type sits in
/// a `pub(crate)` module and `Name`'s inner `hyper_util` name is `pub(super)` — so there is
/// nothing to delegate to from outside the crate. `tokio::net::lookup_host` reaches the same
/// `getaddrinfo` through the same blocking pool, so resolution behaviour is unchanged; the port
/// is `0` because reqwest replaces it with the scheme's own afterwards (`dns::Resolve`'s
/// contract).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TimedResolver;

impl Resolve for TimedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let started = Instant::now();
            // `(String, u16)` rather than a borrowed `&str`: the future outlives the local.
            let resolved = tokio::net::lookup_host((host, 0)).await;
            // Recorded even on failure: a lookup that took 5 seconds to fail is exactly the
            // thing someone opens this pane to find out.
            record_dns(started.elapsed());

            match resolved {
                Ok(addrs) => Ok(Box::new(addrs) as Addrs),
                Err(error) => Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
            }
        })
    }
}

/// The layer half: times each connection the connector establishes.
///
/// Generic over the inner service rather than naming reqwest's `BoxedConnectorService`, which
/// is `pub(crate)` — a blanket impl satisfies the bound without being able to name the type.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TimedConnect;

impl<S> Layer<S> for TimedConnect {
    type Service = TimedConnector<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TimedConnector { inner }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TimedConnector<S> {
    inner: S,
}

impl<S, Request> Service<Request> for TimedConnector<S>
where
    S: Service<Request>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let started = Instant::now();
        let inner = self.inner.call(request);

        Box::pin(async move {
            let connected = inner.await;
            // **Only a connection that succeeded counts as a socket.** A failed attempt has no
            // response to describe, and counting it would report `sockets: 2` for a request that
            // opened one after a refused first address.
            if connected.is_ok() {
                record_socket(started.elapsed());
            }
            connected
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_recorded_reads_as_a_pooled_socket() {
        // The success path only asks a probe for its answer after headers have arrived, so
        // "no socket was opened" can only mean the pool served this request.
        assert_eq!(Probe::default().connection(), Connection::Pooled);
    }

    #[test]
    fn an_ip_literal_records_a_socket_with_no_lookup() {
        let probe = Probe::default();
        probe.connect_nanos.store(3_000_000, Ordering::Relaxed);
        probe.sockets.store(1, Ordering::Relaxed);

        assert_eq!(
            probe.connection(),
            Connection::Opened {
                dns: None,
                connect: Duration::from_millis(3),
                sockets: 1,
            }
        );
    }

    #[test]
    fn the_lookup_is_subtracted_out_of_the_connect_span() {
        // The connector calls the resolver, so its span contains the lookup. Reporting the
        // layer's raw number would double-count DNS and push the phases past the total.
        let probe = Probe::default();
        probe.dns_nanos.store(2_000_000, Ordering::Relaxed);
        probe.lookups.store(1, Ordering::Relaxed);
        probe.connect_nanos.store(9_000_000, Ordering::Relaxed);
        probe.sockets.store(1, Ordering::Relaxed);

        assert_eq!(
            probe.connection(),
            Connection::Opened {
                dns: Some(Duration::from_millis(2)),
                connect: Duration::from_millis(7),
                sockets: 1,
            }
        );
    }

    #[test]
    fn a_lookup_with_no_socket_is_still_a_pooled_connection() {
        // The abandoned-connection race: the pool won, but the connect future had already
        // resolved a name before it was dropped. Reporting `Opened` here would draw a
        // handshake onto a request that reused its socket.
        let probe = Probe::default();
        probe.dns_nanos.store(1_000_000, Ordering::Relaxed);
        probe.lookups.store(1, Ordering::Relaxed);

        assert_eq!(probe.connection(), Connection::Pooled);
    }

    #[test]
    fn redirects_across_hosts_sum_their_sockets() {
        let probe = Probe::default();
        probe.dns_nanos.store(1_000_000, Ordering::Relaxed);
        probe.lookups.store(2, Ordering::Relaxed);
        probe.connect_nanos.store(11_000_000, Ordering::Relaxed);
        probe.sockets.store(2, Ordering::Relaxed);

        assert_eq!(
            probe.connection(),
            Connection::Opened {
                dns: Some(Duration::from_millis(1)),
                connect: Duration::from_millis(10),
                sockets: 2,
            }
        );
    }
}
