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
use std::path::PathBuf;
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

/// Certificate files handed to every client.
///
/// **Paths, not bytes**, and app-level rather than per request for the reason `ProxyMode` is:
/// these name files on this machine, so a `RequestSettings` field would write a path into every
/// committed collection file and break on anyone else's clone.
///
/// A single PEM carrying both certificate and key, because that is the only shape our TLS
/// backend accepts: `Identity::from_pem` is the sole constructor under `rustls`, while
/// PKCS#12 and separate cert/key files are `native-tls` only. That constraint is convenient —
/// PKCS#12 would mean storing its password.
///
/// **Known limitation:** the files are read when a client is built and the paths are what key
/// the cache, so editing a certificate in place without changing its path keeps the old one
/// until the setting is re-applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TlsFiles {
    /// The identity being presented, if any. **One**, and that is TLS deciding rather than a
    /// simplification: a handshake presents a single certificate, and reqwest takes one per
    /// client. Choosing between several needs a rule for which to use per host, which is a
    /// different feature.
    #[serde(default)]
    pub identity: Option<PathBuf>,
    /// Identities the user has chosen before, so switching does not mean browsing again. Holds
    /// `identity` too — the same shape the saved proxy list has.
    #[serde(default)]
    pub identities: Vec<PathBuf>,
    /// Extra trusted issuers, **all active at once**.
    ///
    /// A set rather than one, because `add_root_certificate` is callable repeatedly and trust is
    /// additive: a corporate CA *and* a staging CA is an ordinary thing to need. That asymmetry
    /// with `identity` is why the panel draws two differently-shaped sections.
    ///
    /// Added to the defaults rather than replacing them, and the scalpel where
    /// `verify_tls: false` is the sledgehammer: trusting one issuer is not trusting anything.
    #[serde(default)]
    pub root_cas: Vec<PathBuf>,
}

impl TlsFiles {
    pub fn is_empty(&self) -> bool {
        self.identity.is_none() && self.root_cas.is_empty()
    }
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
    /// Change which certificates future clients are built with.
    SetTls(TlsFiles),
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

    /// Build future clients with `files`. Same ordering guarantee as `set_proxy`.
    pub fn set_tls(&self, files: TlsFiles) {
        let _ = self.commands.send(Command::SetTls(files));
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
        let mut tls = TlsFiles::default();

        while let Some(command) = commands.recv().await {
            // Opportunistic reaping: without this the map grows for the life of the
            // process.
            jobs.retain(|_, handle| !handle.is_finished());

            match command {
                Command::Send { job, spec, events } => match clients.get(&spec.settings, &proxy, &tls) {
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
                Command::SetTls(files) => tls = files,
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
    /// In the key for the same reason the proxy is: a changed certificate must miss the cache
    /// rather than leave every pooled client presenting the old one.
    tls: TlsFiles,
}

impl ClientKey {
    fn new(settings: &RequestSettings, proxy: &ProxyMode, tls: &TlsFiles) -> Self {
        Self {
            verify_tls: settings.verify_tls,
            follow_redirects: settings.follow_redirects,
            max_redirects: settings.max_redirects,
            accept_encodings: settings.accept_encodings,
            cookie_store: settings.cookie_store,
            proxy: proxy.clone(),
            tls: tls.clone(),
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
        tls: &TlsFiles,
    ) -> Result<Client, EngineError> {
        let key = ClientKey::new(settings, proxy, tls);

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

    // Read here rather than at send: a client is built once per distinct settings and reused.
    // **Reported, not swallowed** — a certificate silently ignored means a request that fails
    // its handshake for a reason nothing on screen explains.
    let builder = match &key.tls.identity {
        None => builder,
        Some(path) => builder.identity(read_identity(path)?),
    };
    let mut builder = builder;
    for path in &key.tls.root_cas {
        builder = builder.add_root_certificate(read_root_ca(path)?);
    }

    builder.build().map_err(|error| EngineError::Build {
        reason: error.to_string(),
    })
}

fn read_identity(path: &std::path::Path) -> Result<reqwest::Identity, EngineError> {
    let pem = std::fs::read(path).map_err(|error| EngineError::Build {
        reason: format!("client certificate {}: {error}", path.display()),
    })?;
    reqwest::Identity::from_pem(&pem).map_err(|error| EngineError::Build {
        reason: format!("client certificate {}: {error}", path.display()),
    })
}

fn read_root_ca(path: &std::path::Path) -> Result<reqwest::Certificate, EngineError> {
    let pem = std::fs::read(path).map_err(|error| EngineError::Build {
        reason: format!("root certificate {}: {error}", path.display()),
    })?;
    reqwest::Certificate::from_pem(&pem).map_err(|error| EngineError::Build {
        reason: format!("root certificate {}: {error}", path.display()),
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
            ClientKey::new(&a, &system, &TlsFiles::default()),
            ClientKey::new(&b, &system, &TlsFiles::default())
        );

        b.verify_tls = false;
        assert_ne!(
            ClientKey::new(&a, &system, &TlsFiles::default()),
            ClientKey::new(&b, &system, &TlsFiles::default())
        );
    }

    #[test]
    fn changing_the_proxy_misses_the_client_cache() {
        // The reason the mode is in the key at all. Without it, every cached client would keep
        // routing through the old proxy until something remembered to evict them — and nothing
        // on screen would say so.
        let settings = RequestSettings::default();
        assert_ne!(
            ClientKey::new(&settings, &ProxyMode::System, &TlsFiles::default()),
            ClientKey::new(&settings, &ProxyMode::Off, &TlsFiles::default())
        );
        assert_ne!(
            ClientKey::new(&settings, &ProxyMode::Off, &TlsFiles::default()),
            ClientKey::new(&settings, &ProxyMode::Url("http://p:8080".into()), &TlsFiles::default())
        );
    }

    #[test]
    fn tls_files_report_whether_anything_is_configured() {
        // What the titlebar icon reads to decide whether it is muted or lit. Either half alone
        // counts: a root CA with no identity is still a certificate in force.
        assert!(TlsFiles::default().is_empty());
        assert!(
            !TlsFiles {
                identity: Some(PathBuf::from("/k/a.pem")),
                ..TlsFiles::default()
            }
            .is_empty()
        );
        assert!(
            !TlsFiles {
                root_cas: vec![PathBuf::from("/k/corp.pem")],
                ..TlsFiles::default()
            }
            .is_empty()
        );
        // A remembered identity that is not the active one is not "in force".
        assert!(
            TlsFiles {
                identities: vec![PathBuf::from("/k/a.pem")],
                ..TlsFiles::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn changing_a_certificate_misses_the_client_cache() {
        let settings = RequestSettings::default();
        let none = TlsFiles::default();
        let with_identity = TlsFiles {
            identity: Some(PathBuf::from("/tmp/a.pem")),
            ..TlsFiles::default()
        };
        assert_ne!(
            ClientKey::new(&settings, &ProxyMode::System, &none),
            ClientKey::new(&settings, &ProxyMode::System, &with_identity)
        );
    }

    #[test]
    fn a_certificate_that_cannot_be_read_is_reported_rather_than_ignored() {
        // The failure that matters. A cert silently dropped means a handshake that fails for a
        // reason nothing on screen explains — so both a missing file and a malformed one have
        // to name the path.
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/identity.pem");

        let missing = ClientKey::new(
            &RequestSettings::default(),
            &ProxyMode::System,
            &TlsFiles {
                identity: Some(PathBuf::from("/definitely/not/here.pem")),
                ..TlsFiles::default()
            },
        );
        let error = build_client(&missing).expect_err("a missing certificate must fail");
        assert!(
            matches!(&error, EngineError::Build { reason } if reason.contains("not/here.pem")),
            "{error:?}"
        );

        // And a real one builds, so the failure above is about the file and not about the path
        // ever being honoured at all.
        let ok = ClientKey::new(
            &RequestSettings::default(),
            &ProxyMode::System,
            &TlsFiles {
                identity: Some(PathBuf::from(fixture)),
                ..TlsFiles::default()
            },
        );
        assert!(build_client(&ok).is_ok(), "a valid PEM identity should build");
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
