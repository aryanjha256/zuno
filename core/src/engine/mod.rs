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
mod run;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use reqwest::Client;
use tokio::sync::mpsc;

pub use error::EngineError;

use crate::request::{Header, RequestSettings, RequestSpec};
use crate::response::{HttpVersion, ResponseData};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

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

        while let Some(command) = commands.recv().await {
            // Opportunistic reaping: without this the map grows for the life of the
            // process.
            jobs.retain(|_, handle| !handle.is_finished());

            match command {
                Command::Send { job, spec, events } => match clients.get(&spec.settings) {
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
            }
        }
    });
}

/// The settings that reqwest can only configure per *client*, not per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ClientKey {
    verify_tls: bool,
    follow_redirects: bool,
    max_redirects: u8,
    accept_encodings: bool,
    cookie_store: bool,
}

impl From<&RequestSettings> for ClientKey {
    fn from(settings: &RequestSettings) -> Self {
        Self {
            verify_tls: settings.verify_tls,
            follow_redirects: settings.follow_redirects,
            max_redirects: settings.max_redirects,
            accept_encodings: settings.accept_encodings,
            cookie_store: settings.cookie_store,
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

    fn get(&mut self, settings: &RequestSettings) -> Result<Client, EngineError> {
        let key = ClientKey::from(settings);

        if let Some(client) = self.clients.get(&key) {
            return Ok(client.clone());
        }

        let client = build_client(&key)?;
        self.clients.insert(key, client.clone());
        Ok(client)
    }
}

fn build_client(key: &ClientKey) -> Result<Client, EngineError> {
    let redirect = if key.follow_redirects {
        reqwest::redirect::Policy::limited(key.max_redirects as usize)
    } else {
        reqwest::redirect::Policy::none()
    };

    Client::builder()
        .user_agent(concat!("zuno/", env!("CARGO_PKG_VERSION")))
        .danger_accept_invalid_certs(!key.verify_tls)
        .redirect(redirect)
        .gzip(key.accept_encodings)
        .brotli(key.accept_encodings)
        .deflate(key.accept_encodings)
        .zstd(key.accept_encodings)
        .cookie_store(key.cookie_store)
        .build()
        .map_err(|error| EngineError::Build {
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
        assert_eq!(ClientKey::from(&a), ClientKey::from(&b));

        b.verify_tls = false;
        assert_ne!(ClientKey::from(&a), ClientKey::from(&b));
    }
}
