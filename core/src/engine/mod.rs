//! The HTTP engine.
//!
//! GPUI's executor is smol-based; reqwest needs a tokio reactor. Rather than bridge
//! them per-future, the engine owns a dedicated tokio runtime on its own thread and
//! talks to the UI over channels (architecture.md §4):
//!
//! ```text
//!   UI thread (GPUI / smol)                Engine thread (tokio)
//!   ───────────────────────                ─────────────────────
//!   engine.send(spec) ──── Command ──────▶  build + execute
//!     → (JobId, Receiver<Event>)             └─ stream body
//!   ◀────────── async-channel ──────────────────┘
//! ```
//!
//! `async-channel` carries events because it is runtime-agnostic: `try_send` is
//! non-blocking and callable from a tokio task, while the receiver is awaited on
//! gpui's executor. Unbounded, so a busy UI can never stall the network.

pub mod build;
pub mod error;
mod probe;
mod run;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use url::Url;

pub use error::EngineError;

use crate::request::{Header, RequestSettings, RequestSpec};
use crate::response::{HttpVersion, ResponseData};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// How requests reach the network.
///
/// **Three states, because `Option<String>` cannot express this one.** `None` would have to
/// mean both "use whatever the environment says" and "use nothing", and those are different
/// requests on the wire — the same overloading `Connection` was split up to avoid.
///
/// The reason this type exists at all is that the middle state was already happening, invisibly:
/// reqwest 0.13 builds every client with `auto_sys_proxy: true`, and hyper-util's matcher reads
/// `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY`. So Zuno has always honoured a system
/// proxy with nothing on screen saying so and no way to override it — which is exactly what
/// `RequestSettings::cookie_store`'s own comment says a behaviour like this must not be.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// Whatever `HTTP_PROXY` and friends say. The default, because it is what Zuno already did:
    /// changing it would silently stop working for anyone behind a corporate proxy today, and
    /// that failure presents as a network problem rather than as a setting.
    #[default]
    System,
    /// No proxy, and the environment ignored.
    ///
    /// **Needs `.no_proxy()` rather than merely omitting one.** Leaving the builder alone keeps
    /// `auto_sys_proxy` on, so "off" would go on quietly using the env var — the confusion
    /// `Engine::clear_cookies` had to exist to prevent, one setting over.
    Off,
    /// An explicit proxy. `ClientBuilder::proxy` clears `auto_sys_proxy` itself, so this needs
    /// no second call.
    Url(String),
}

impl ProxyMode {
    /// Turn typed text into a mode, or `None` when it cannot be one.
    ///
    /// Pure, so the picker can offer the query as a candidate only when it would actually work —
    /// the same shape as the method picker offering an unknown verb as `Method::Other` rather
    /// than letting a send fail later.
    ///
    /// A **bare `host:port` gets an `http://`**, because that is what people type and reqwest
    /// needs a scheme. Only `http` and `https` are accepted: `socks5` is a legal proxy scheme
    /// that reqwest rejects without its `socks` feature, which is not enabled — so offering it
    /// would produce a mode that fails at the next send.
    pub fn from_input(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        let candidate = if text.contains("://") {
            text.to_string()
        } else {
            format!("http://{text}")
        };

        let url = Url::parse(&candidate).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        // A URL with no host is not somewhere a request can be sent.
        url.host_str()?;

        Some(ProxyMode::Url(candidate))
    }

    /// What to show in the status bar and the picker.
    pub fn label(&self) -> &str {
        match self {
            ProxyMode::System => "system",
            ProxyMode::Off => "off",
            ProxyMode::Url(url) => url,
        }
    }
}

/// What the UI learns while a request is in flight.
///
/// Every variant carries its `JobId` so a late event from a cancelled job can be
/// recognised and dropped rather than mistaken for the current one.
#[derive(Debug, Clone)]
pub enum Event {
    Started {
        job: JobId,
    },
    /// Fired at TTFB — before the body has arrived — so the status line and headers
    /// can paint immediately.
    Head {
        job: JobId,
        status: u16,
        status_text: String,
        version: HttpVersion,
        headers: Vec<Header>,
        ttfb: Duration,
    },
    Progress {
        job: JobId,
        received: usize,
        total: Option<usize>,
    },
    /// Boxed because `ResponseData` owns the whole body, and an enum is as large as
    /// its biggest variant — every `Event` would otherwise pay for it.
    Done {
        job: JobId,
        response: Box<ResponseData>,
    },
    Failed {
        job: JobId,
        error: EngineError,
    },
}

impl Event {
    pub fn job(&self) -> JobId {
        match self {
            Event::Started { job }
            | Event::Head { job, .. }
            | Event::Progress { job, .. }
            | Event::Done { job, .. }
            | Event::Failed { job, .. } => *job,
        }
    }

    /// True for the last event a job will ever emit.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Event::Done { .. } | Event::Failed { .. })
    }
}

enum Command {
    Send {
        job: JobId,
        spec: Box<RequestSpec>,
        events: Sender<Event>,
    },
    Cancel {
        job: JobId,
    },
    /// Throw away every cached client, and with them every cookie jar.
    ClearCookies,
    /// Change where requests are routed. Future clients are built with it.
    SetProxy(ProxyMode),
}

pub struct Engine {
    commands: mpsc::UnboundedSender<Command>,
    next_job: AtomicU64,
}

impl Engine {
    /// Spawn the engine thread and its runtime.
    pub fn new() -> std::io::Result<Self> {
        let (commands, receiver) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name("zuno-http".to_string())
            .spawn(move || drive(receiver))?;

        Ok(Self {
            commands,
            next_job: AtomicU64::new(1),
        })
    }

    /// Submit a request. Returns immediately with the job's id and its event stream.
    ///
    /// The receiver closes once the job emits a terminal event, so the consumer's loop
    /// ends naturally.
    pub fn send(&self, spec: RequestSpec) -> (JobId, Receiver<Event>) {
        let job = JobId(self.next_job.fetch_add(1, Ordering::Relaxed));
        let (events, receiver) = async_channel::unbounded();

        // A closed command channel means the engine thread is gone; report it through
        // the same path as any other failure rather than panicking.
        if self
            .commands
            .send(Command::Send {
                job,
                spec: Box::new(spec),
                events: events.clone(),
            })
            .is_err()
        {
            let _ = events.try_send(Event::Failed {
                job,
                error: EngineError::Other {
                    reason: "the HTTP engine is not running".to_string(),
                },
            });
        }

        (job, receiver)
    }

    /// Abort an in-flight job.
    ///
    /// Dropping the UI-side task stops *consuming* events; this is what stops the
    /// socket. Both halves are needed — see `RequestView::cancel`.
    pub fn cancel(&self, job: JobId) {
        let _ = self.commands.send(Command::Cancel { job });
    }

    /// Forget every stored cookie.
    ///
    /// **Why this has to exist for the cookie toggle to make sense.** `cookie_store` is
    /// part of `ClientKey`, so turning it off doesn't empty a jar — it routes the request
    /// through a *different* cached client. Turning it back on returns you to the original
    /// client with every previous cookie intact, so without this you could switch cookies
    /// off, back on, and still be silently logged in. A toggle alone would create the
    /// confusion it was added to remove.
    ///
    /// Implemented by dropping the cached clients rather than reaching into a jar: reqwest
    /// owns the store behind `cookie_store(true)` and exposes no way to clear it, and the
    /// next request rebuilds a client with an empty one. The cost is the connection pool,
    /// which is why this is an explicit action and not something a toggle does implicitly.
    pub fn clear_cookies(&self) {
        let _ = self.commands.send(Command::ClearCookies);
    }

    /// Route future requests through `mode`.
    ///
    /// Sent as a command rather than held behind a lock, so it lands on the engine thread in
    /// order with the sends around it. An **in-flight job keeps the client it already has**,
    /// which is `clear_cookies`' rule and right for the same reason: changing a setting should
    /// not sabotage a response you are waiting on.
    pub fn set_proxy(&self, mode: ProxyMode) {
        let _ = self.commands.send(Command::SetProxy(mode));
    }
}

/// The engine thread's main loop.
fn drive(mut commands: mpsc::UnboundedReceiver<Command>) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("zuno-http-worker")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            // Nothing can be sent without a runtime. Report and let each `send` fail
            // through the closed-channel path above.
            eprintln!("[zuno] could not start the HTTP runtime: {error}");
            return;
        }
    };

    runtime.block_on(async move {
        let mut clients = ClientCache::default();
        let mut jobs: HashMap<JobId, tokio::task::JoinHandle<()>> = HashMap::new();
        let mut proxy = ProxyMode::default();

        while let Some(command) = commands.recv().await {
            // Opportunistic reaping: without this the map grows for the life of the
            // process.
            jobs.retain(|_, handle| !handle.is_finished());

            match command {
                Command::Send { job, spec, events } => match clients.get(&spec.settings, &proxy) {
                    Ok(client) => {
                        jobs.insert(
                            job,
                            tokio::spawn(run::execute(
                                job,
                                client,
                                *spec,
                                events,
                                run::MAX_BODY_BYTES,
                            )),
                        );
                    }
                    Err(error) => {
                        let _ = events.try_send(Event::Failed { job, error });
                    }
                },
                Command::Cancel { job } => {
                    if let Some(handle) = jobs.remove(&job) {
                        handle.abort();
                    }
                }
                // In-flight jobs hold their own `Client` clone, so they finish against the
                // old jar. Only later requests see the fresh one — which is the behaviour
                // you want: clearing cookies shouldn't sabotage a response you're waiting
                // on.
                Command::ClearCookies => clients.clear(),
                // No `clear()` needed: the mode is part of `ClientKey`, so a changed proxy
                // simply misses the cache rather than relying on anyone remembering to evict.
                Command::SetProxy(mode) => proxy = mode,
            }
        }
    });
}

/// The settings that reqwest can only configure per *client*, not per request.
///
/// **`Clone` rather than `Copy`**, since the proxy carries a URL. Worth the loss: with the mode
/// in the key, changing the proxy misses the cache by construction, where a key without it would
/// leave every cached client quietly routing through the old one until somebody remembered to
/// evict — the class of hazard this codebase keeps turning into funnels.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    verify_tls: bool,
    follow_redirects: bool,
    max_redirects: u8,
    accept_encodings: bool,
    cookie_store: bool,
    /// Not a `RequestSettings` field: a proxy is a property of the machine and its network, not
    /// of a request. Keeping it out of `RequestSpec` is also what keeps a URL carrying
    /// `user:pass@` from being serialized into a committed collection file (invariant 10).
    proxy: ProxyMode,
}

impl ClientKey {
    fn new(settings: &RequestSettings, proxy: &ProxyMode) -> Self {
        Self {
            verify_tls: settings.verify_tls,
            follow_redirects: settings.follow_redirects,
            max_redirects: settings.max_redirects,
            accept_encodings: settings.accept_encodings,
            cookie_store: settings.cookie_store,
            proxy: proxy.clone(),
        }
    }
}

/// One client per distinct set of client-level settings.
///
/// Building a client per request would be simpler but would throw away connection
/// pooling — and pooling is exactly what makes hitting Send twice in a row feel
/// instant, which is the whole point of the milestone.
#[derive(Default)]
struct ClientCache {
    clients: HashMap<ClientKey, Client>,
}

impl ClientCache {
    /// Drop every client, so the next request builds a fresh one with an empty jar.
    fn clear(&mut self) {
        self.clients.clear();
    }

    fn get(
        &mut self,
        settings: &RequestSettings,
        proxy: &ProxyMode,
    ) -> Result<Client, EngineError> {
        let key = ClientKey::new(settings, proxy);

        if let Some(client) = self.clients.get(&key) {
            return Ok(client.clone());
        }

        let client = build_client(&key)?;
        self.clients.insert(key.clone(), client.clone());
        Ok(client)
    }
}

fn build_client(key: &ClientKey) -> Result<Client, EngineError> {
    let redirect = if key.follow_redirects {
        reqwest::redirect::Policy::limited(key.max_redirects as usize)
    } else {
        reqwest::redirect::Policy::none()
    };

    let builder = Client::builder()
        .user_agent(concat!("zuno/", env!("CARGO_PKG_VERSION")))
        .danger_accept_invalid_certs(!key.verify_tls)
        .redirect(redirect)
        // The two hooks that fill in `Timing`'s connection stages. They are installed on
        // *every* client rather than behind a setting, because they cost one `Instant::now`
        // per connection and the alternative is a timeline that is blank until someone finds
        // a toggle. See `probe.rs` for why a shared client can still attribute a measurement
        // to one job.
        .dns_resolver(probe::TimedResolver)
        .connector_layer(probe::TimedConnect)
        .gzip(key.accept_encodings)
        .brotli(key.accept_encodings)
        .deflate(key.accept_encodings)
        .zstd(key.accept_encodings)
        .cookie_store(key.cookie_store);

    let builder = match &key.proxy {
        // Nothing to do: reqwest's default already reads the environment.
        ProxyMode::System => builder,
        ProxyMode::Off => builder.no_proxy(),
        ProxyMode::Url(url) => builder.proxy(
            // **Reported, not swallowed.** `from_input` is what stops an unusable URL being
            // chosen through the picker, but `app.json` can be hand-edited — and a proxy that
            // is silently ignored sends the request straight to the network under a status bar
            // claiming otherwise, which is the one outcome worse than refusing.
            reqwest::Proxy::all(url.as_str()).map_err(|error| EngineError::Build {
                reason: format!("proxy {url}: {error}"),
            })?,
        ),
    };

    builder.build().map_err(|error| EngineError::Build {
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_report_their_job_and_terminality() {
        let job = JobId(7);
        assert_eq!(Event::Started { job }.job(), job);
        assert!(!Event::Started { job }.is_terminal());
        assert!(
            Event::Failed {
                job,
                error: EngineError::EmptyUrl
            }
            .is_terminal()
        );
    }

    #[test]
    fn client_keys_ignore_settings_that_are_per_request() {
        let mut a = RequestSettings::default();
        let mut b = RequestSettings::default();
        a.timeout = Some(Duration::from_secs(1));
        b.timeout = Some(Duration::from_secs(600));

        // Timeout is applied per request, so it must not fragment the client cache
        // (and with it, the connection pool).
        let system = ProxyMode::System;
        assert_eq!(
            ClientKey::new(&a, &system),
            ClientKey::new(&b, &system)
        );

        b.verify_tls = false;
        assert_ne!(
            ClientKey::new(&a, &system),
            ClientKey::new(&b, &system)
        );
    }

    #[test]
    fn changing_the_proxy_misses_the_client_cache() {
        // The reason the mode is in the key at all. Without it, every cached client would keep
        // routing through the old proxy until something remembered to evict them — and nothing
        // on screen would say so.
        let settings = RequestSettings::default();
        assert_ne!(
            ClientKey::new(&settings, &ProxyMode::System),
            ClientKey::new(&settings, &ProxyMode::Off)
        );
        assert_ne!(
            ClientKey::new(&settings, &ProxyMode::Off),
            ClientKey::new(&settings, &ProxyMode::Url("http://p:8080".into()))
        );
    }

    #[test]
    fn typed_text_becomes_a_proxy_only_when_it_could_work() {
        // Drives the picker's fallback row: offering a candidate that fails at the next send
        // is the dead-control shape, one layer down.
        assert_eq!(
            ProxyMode::from_input("http://127.0.0.1:8080"),
            Some(ProxyMode::Url("http://127.0.0.1:8080".into()))
        );
        // A bare host:port is what people type, and reqwest needs a scheme.
        assert_eq!(
            ProxyMode::from_input("localhost:3128"),
            Some(ProxyMode::Url("http://localhost:3128".into()))
        );
        assert_eq!(
            ProxyMode::from_input("  https://proxy.corp  "),
            Some(ProxyMode::Url("https://proxy.corp".into())),
            "trimmed, since a pasted value carries whitespace"
        );

        assert_eq!(ProxyMode::from_input(""), None);
        assert_eq!(ProxyMode::from_input("   "), None);
        // Legal as a proxy scheme, and rejected by reqwest without its `socks` feature — so
        // offering it would produce a mode that cannot send.
        assert_eq!(ProxyMode::from_input("socks5://127.0.0.1:1080"), None);
        assert_eq!(ProxyMode::from_input("http://"), None, "no host");
    }
}
