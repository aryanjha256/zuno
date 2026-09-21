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
mod session;

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

use bytes::Bytes;

use crate::request::{Header, RequestSettings, RequestSpec};
use crate::response::{HttpVersion, ResponseData};

/// What is carrying a session.
///
/// **Reported rather than inferred.** The app could guess from the request's kind today, since
/// only a WebSocket buffer produces a socket — but that guess is wrong the moment anything else
/// opens one, and the whole point of the lifecycle split is that the *response* decides. So the
/// response says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    WebSocket,
    /// A `text/event-stream` response, whatever asked for it — a plain HTTP request or a
    /// GraphQL subscription over graphql-sse.
    EventStream,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Transport::WebSocket => "websocket",
            Transport::EventStream => "sse",
        }
    }
}

/// Which way one frame of a session travelled.
///
/// A transcript is the only response shape where this question exists — an HTTP response has
/// exactly one direction and never needs to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Sent,
    Received,
}

/// One message on an open connection.
///
/// **Deliberately not `ResponseData`.** A frame has no status, no headers and no timing of its
/// own; it is a payload and a moment. Modelling it as a tiny response would mean five fields
/// that are always empty and a viewer that has to know they are meaningless.
///
/// `Text` is a `String` rather than `Bytes`, which is the one place this crate departs from
/// invariant 4 and does so on a guarantee rather than a hope: a WebSocket text frame is
/// *defined* as UTF-8 and tungstenite rejects the connection outright if one is not. Binary
/// frames stay `Bytes`, where the invariant's reasoning still holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    /// One server-sent event.
    ///
    /// **Its own variant rather than `Text`, because the name is how SSE is routed.** A stream
    /// that sends `event: added` and `event: removed` says which is which in a field, not in
    /// the payload, so flattening it to the data would throw away the half you filter on.
    /// Receive-only by nature: nothing composes one.
    Event {
        /// The `event:` field. `None` when the stream did not name one, which the spec calls
        /// `message` — kept absent rather than filled in, so a transcript can show the
        /// difference between a stream that names its events and one that does not.
        name: Option<String>,
        /// The last `id:` the stream sent, which persists until it sends another.
        id: Option<String>,
        data: String,
    },
}

impl Frame {
    /// Bytes on the wire, for a transcript that reports sizes.
    pub fn len(&self) -> usize {
        match self {
            Frame::Text(text) => text.len(),
            Frame::Event { data, .. } => data.len(),
            Frame::Binary(bytes) | Frame::Ping(bytes) | Frame::Pong(bytes) => bytes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What the UI asks of a socket that is already open.
///
/// Its own channel rather than more `Command` variants on the engine's inbox: a frame has to
/// reach *one running task*, and routing it through the engine loop would mean the loop owning
/// a sender per job and answering for the case where the job has already finished. The engine
/// still holds the sender — see `Job` — it just does not interpret what goes down it.
pub(crate) enum Outbound {
    Frame(Frame),
    /// Begin the closing handshake. Not an abort: the peer still gets its Close frame.
    Close,
}

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

    /// The handshake succeeded and the socket is open.
    ///
    /// The session's answer to `Head`, and it carries the same things for the same reason: the
    /// status line and the response headers are worth showing, and this is the moment they are
    /// known. `protocol` is whichever subprotocol the server picked from the offers, which is
    /// the one thing about a socket you cannot see by watching it.
    Opened {
        job: JobId,
        transport: Transport,
        status: u16,
        /// `Switching Protocols`. Carried for the same reason `Head` carries it: the pane
        /// prints the reason phrase beside the code, and deriving it here would be a second
        /// table that can disagree with the server's own words.
        status_text: String,
        headers: Vec<Header>,
        protocol: Option<String>,
        elapsed: Duration,
    },
    /// One message, either way. `at` is measured from the send, so a transcript can show the
    /// gap between frames without every row carrying a wall clock.
    Frame {
        job: JobId,
        at: Duration,
        direction: Direction,
        frame: Frame,
    },
    /// A dropped stream is being picked up again.
    ///
    /// **Only SSE emits this**, because only SSE can resume: the protocol defines `retry:` and
    /// replay from `Last-Event-ID`, so a reconnect loses nothing. A socket has no such
    /// mechanism, and reconnecting one silently would drop every message in the gap while
    /// looking like it had worked.
    Reconnecting {
        job: JobId,
        /// 1 for the first retry. Shown, because a server flapping every three seconds should
        /// look like a problem rather than like a stream that works.
        attempt: u32,
        delay: Duration,
    },
    /// The socket closed. `code` is absent when the peer vanished without a Close frame,
    /// which is a real and common ending rather than an error.
    Closed {
        job: JobId,
        code: Option<u16>,
        reason: String,
    },
}

impl Event {
    pub fn job(&self) -> JobId {
        match self {
            Event::Started { job }
            | Event::Head { job, .. }
            | Event::Progress { job, .. }
            | Event::Done { job, .. }
            | Event::Failed { job, .. }
            | Event::Opened { job, .. }
            | Event::Frame { job, .. }
            | Event::Reconnecting { job, .. }
            | Event::Closed { job, .. } => *job,
        }
    }

    /// True for the last event a job will ever emit.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Event::Done { .. } | Event::Failed { .. } | Event::Closed { .. }
        )
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
    /// Hand something to a job that is still running. Silently dropped if it is not — a frame
    /// typed into a socket that has just closed is a race, not a mistake to report.
    ToJob {
        job: JobId,
        outbound: Outbound,
    },
    /// Throw away every cached client, and with them every cookie jar.
    ClearCookies,
    /// Change where requests are routed. Future clients are built with it.
    SetProxy(ProxyMode),
    /// Change which certificates future clients are built with.
    SetTls(TlsFiles),
}

/// A running job, and the way in if it has one.
struct Job {
    handle: tokio::task::JoinHandle<()>,
    /// Held here rather than in the task so the engine loop can route a frame to it without the
    /// task having to be awake to receive one.
    ///
    /// Present for **every** job, including a plain HTTP request that may never become a
    /// session — because whether it does is the server's decision, made after the job has
    /// already been spawned.
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
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

    /// Send one frame down an open session.
    ///
    /// Fire-and-forget on purpose: the frame's fate is reported through the job's own event
    /// stream as either an `Event::Frame` with `Direction::Sent` or an `Event::Failed`, which
    /// is where the caller is already looking. A `Result` here would be a second, earlier
    /// answer to the same question and the two could disagree.
    pub fn send_frame(&self, job: JobId, frame: Frame) {
        let _ = self.commands.send(Command::ToJob {
            job,
            outbound: Outbound::Frame(frame),
        });
    }

    /// Ask a session to close politely, so the peer receives a Close frame.
    ///
    /// **Not `cancel`.** Cancelling aborts the task, which drops the socket where it stands and
    /// looks like a reset from the other end. Both exist because both are wanted: this is the
    /// Disconnect button, `cancel` is what a closing tab does.
    pub fn close(&self, job: JobId) {
        let _ = self.commands.send(Command::ToJob {
            job,
            outbound: Outbound::Close,
        });
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
        let mut jobs: HashMap<JobId, Job> = HashMap::new();
        let mut proxy = ProxyMode::default();
        let mut tls = TlsFiles::default();

        while let Some(command) = commands.recv().await {
            // Opportunistic reaping: without this the map grows for the life of the
            // process.
            jobs.retain(|_, job| !job.handle.is_finished());

            match command {
                Command::Send { job, spec, events } => {
                    // **A session gets an HTTP/1.1-only client**, because there is no 101 in
                    // HTTP/2 — see `ClientKey::http1_only`. Its own cache entry, so ordinary
                    // requests keep negotiating h2 as they always did.
                    //
                    // The timeout needs no such handling *here*: `build` sets no per-request
                    // deadline any more, and `ClientKey::read_timeout` — which does — is already
                    // part of the key below.
                    match clients.get(&spec.settings, &proxy, &tls, spec.kind.is_session()) {
                        Ok(client) => {
                            // **Every job gets a way in, not just the ones that promise a
                            // session.** An HTTP request becomes one the moment the server
                            // answers `text/event-stream`, and it cannot be handed a channel
                            // afterwards — so a job spawned without one had a Disconnect that
                            // routed into nothing: the status bar said "Disconnecting" and the
                            // stream carried on. The receiver is simply never read by a
                            // request that stays a request.
                            let (sender, receiver) = mpsc::unbounded_channel();
                            let handle = if spec.kind.is_session() {
                                tokio::spawn(session::connect(job, client, *spec, events, receiver))
                            } else {
                                tokio::spawn(run::execute(
                                    job,
                                    client,
                                    *spec,
                                    events,
                                    run::MAX_BODY_BYTES,
                                    receiver,
                                ))
                            };
                            jobs.insert(
                                job,
                                Job {
                                    handle,
                                    outbound: Some(sender),
                                },
                            );
                        }
                        Err(error) => {
                            let _ = events.try_send(Event::Failed { job, error });
                        }
                    }
                }
                Command::ToJob { job, outbound } => {
                    if let Some(sender) = jobs.get(&job).and_then(|job| job.outbound.as_ref()) {
                        let _ = sender.send(outbound);
                    }
                }
                Command::Cancel { job } => {
                    if let Some(job) = jobs.remove(&job) {
                        job.handle.abort();
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
    /// **Forces ALPN to offer only `http/1.1`, and a WebSocket does not work without it.**
    ///
    /// There is no 101 in HTTP/2 — the upgrade mechanism was replaced by extended CONNECT —
    /// and `Connection`/`Upgrade` are illegal headers there, so hyper strips them. A `wss://`
    /// URL whose TLS handshake settles on h2 therefore reaches the server as a *plain GET*,
    /// and the server answers 200 or 404 like any other request to that path. Nothing errors;
    /// it simply is not a WebSocket.
    ///
    /// Setting `Request::version` is **not** enough and was the first attempt: that names the
    /// version for a connection already chosen, while ALPN is negotiated by the connector
    /// underneath it. Only the builder decides what gets offered.
    ///
    /// In the key rather than applied per-request, because it is a property of the *connection*
    /// — a pooled h2 connection cannot be talked out of being h2 afterwards.
    http1_only: bool,
    /// How long a response may go **silent** before it is abandoned.
    ///
    /// **This is half of what `settings.timeout` now means**, and the half that made streaming
    /// possible. It used to be `RequestBuilder::timeout`, a deadline on the whole exchange
    /// including the body — which a stream cannot meet by definition, so an event stream
    /// answered by an HTTP request was killed at whatever the setting said. The other half,
    /// "answer within N", is enforced
    /// on the response head in `run::execute`.
    ///
    /// It has to be in the key because `read_timeout` is a `ClientBuilder` setting: two
    /// requests with different timeouts genuinely need different clients now, where before the
    /// cache could pool them. The cost is real and small — most workspaces use one timeout.
    read_timeout: Option<Duration>,
}

impl ClientKey {
    fn new(
        settings: &RequestSettings,
        proxy: &ProxyMode,
        tls: &TlsFiles,
        http1_only: bool,
    ) -> Self {
        Self {
            verify_tls: settings.verify_tls,
            follow_redirects: settings.follow_redirects,
            max_redirects: settings.max_redirects,
            accept_encodings: settings.accept_encodings,
            cookie_store: settings.cookie_store,
            proxy: proxy.clone(),
            tls: tls.clone(),
            http1_only,
            read_timeout: settings.timeout,
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
        http1_only: bool,
    ) -> Result<Client, EngineError> {
        let key = ClientKey::new(settings, proxy, tls, http1_only);

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

    let mut builder = Client::builder()
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

    // ALPN offers only `http/1.1` when set. See `ClientKey::http1_only` — without this a
    // `wss://` handshake silently becomes a plain GET over h2.
    if key.http1_only {
        builder = builder.http1_only();
    }

    // Per read, not per request. See `ClientKey::read_timeout`.
    if let Some(idle) = key.read_timeout {
        builder = builder.read_timeout(idle);
    }

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
    fn a_timeout_is_a_client_property_and_fragments_the_cache() {
        let mut a = RequestSettings::default();
        let mut b = RequestSettings::default();
        a.timeout = Some(Duration::from_secs(1));
        b.timeout = Some(Duration::from_secs(600));

        // **Timeout now *does* fragment the cache**, and this assertion was reversed when it
        // did. It used to be a per-request deadline, which a shared client could ignore; it is
        // now `read_timeout`, a `ClientBuilder` setting, because a deadline on the whole
        // exchange cannot be met by a stream. Two timeouts, two clients, two connection pools —
        // a real cost, paid because SSE does not work otherwise.
        let system = ProxyMode::System;
        assert_ne!(
            ClientKey::new(&a, &system, &TlsFiles::default(), false),
            ClientKey::new(&b, &system, &TlsFiles::default(), false),
            "an idle timeout is a client property, so it has to be part of the key"
        );

        b.timeout = a.timeout;
        b.verify_tls = false;
        assert_ne!(
            ClientKey::new(&a, &system, &TlsFiles::default(), false),
            ClientKey::new(&b, &system, &TlsFiles::default(), false)
        );
    }

    #[test]
    fn changing_the_proxy_misses_the_client_cache() {
        // The reason the mode is in the key at all. Without it, every cached client would keep
        // routing through the old proxy until something remembered to evict them — and nothing
        // on screen would say so.
        let settings = RequestSettings::default();
        assert_ne!(
            ClientKey::new(&settings, &ProxyMode::System, &TlsFiles::default(), false),
            ClientKey::new(&settings, &ProxyMode::Off, &TlsFiles::default(), false)
        );
        assert_ne!(
            ClientKey::new(&settings, &ProxyMode::Off, &TlsFiles::default(), false),
            ClientKey::new(&settings, &ProxyMode::Url("http://p:8080".into()), &TlsFiles::default(), false)
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
            ClientKey::new(&settings, &ProxyMode::System, &none, false),
            ClientKey::new(&settings, &ProxyMode::System, &with_identity, false)
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
            false,
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
            false,
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
