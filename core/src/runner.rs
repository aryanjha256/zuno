//! Running a sequence of requests and checking what comes back.
//!
//! **Entirely in `zuno-core`, and that is the point.** The split exists so the model and engine
//! can be tested without a window and reused by a CLI, and a runner is the canonical case for
//! both: `Engine::send` hands back an `async_channel::Receiver`, which has `recv_blocking`, so
//! this is an ordinary synchronous loop with no async runtime and no GPUI. The app drives it on
//! a background executor; `zuno run ./collection` would drive it on its main thread.
//!
//! **The step list is the only input, and two things produce one.** A folder expands to
//! `collection::scan` sorted by relative path — a smoke test over a feature. A flow file names
//! its steps explicitly and may cross folders, because a collection is organised by *resource*
//! and a workflow runs across those: `Auth/Login`, `Users/Create`, `Users/Delete`. Neither the
//! loop nor the report knows which produced it.
//!
//! **Sequential, always.** A step's captures publish into the environment that the *next* step
//! resolves against, which is what makes a login-then-use flow work at all. Parallelism would
//! not be a speed-up here, it would be a different feature with different semantics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::assertion::{self, Failure};
use crate::engine::{Engine, Event};
use crate::environment::{self, EnvironmentFile};
use crate::json::JsonOutline;
use crate::{EngineError, RequestSpec, Resolver};

/// One request to run, and the name the report calls it by.
pub struct Step {
    /// The collection-relative path, which is what the panel already shows.
    pub label: String,
    /// `None` when a flow names a request that is no longer there.
    ///
    /// A *failed* step rather than a skipped one: a step quietly vanishing from a run is how a
    /// flow reports "3 passed, 0 failed" while checking three things instead of four, which is
    /// the most dangerous shape a green run can have.
    pub spec: Option<RequestSpec>,
}

/// What one step did.
pub struct Outcome {
    pub label: String,
    /// `None` when nothing came back — the request never reached a response.
    pub status: Option<u16>,
    pub duration: Duration,
    pub error: Option<EngineError>,
    /// Why the step could not run at all. Distinct from a request that ran and failed.
    pub unresolved: Option<String>,
    pub failures: Vec<Failure>,
    /// Variables this step published, for the report to say why the next one worked.
    pub captured: Vec<String>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.unresolved.is_none() && self.failures.is_empty()
    }
}

pub struct Report {
    pub outcomes: Vec<Outcome>,
    /// Whether the run was stopped rather than finishing.
    pub cancelled: bool,
}

impl Report {
    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|outcome| outcome.passed()).count()
    }

    pub fn failed(&self) -> usize {
        self.outcomes.len() - self.passed()
    }
}

/// Run every step in order, reporting what each one did.
///
/// **Failures do not stop the run.** A run exists to tell you everything that is wrong in one
/// pass; stopping at the first means running it again to find the second. `cancel` is the only
/// thing that ends it early, and `run_step` polls for it rather than blocking on the event
/// stream, so a request that hangs without ever emitting another event can still be stopped.
pub fn run(
    engine: &Engine,
    steps: Vec<Step>,
    collection_root: &std::path::Path,
    environment: Option<&str>,
    cancel: &Arc<AtomicBool>,
) -> Report {
    run_with_progress(engine, steps, collection_root, environment, cancel, |_| {})
}

/// `run`, reporting each step as it finishes.
///
/// **Separate rather than a callback on `run`**, so the common case and every test stay a plain
/// call. The app needs it because a forty-request run that shows nothing until it ends is
/// indistinguishable from one that has hung — and the honest fix is to say what has landed, not
/// to make the wait prettier.
pub fn run_with_progress(
    engine: &Engine,
    steps: Vec<Step>,
    collection_root: &std::path::Path,
    environment: Option<&str>,
    cancel: &Arc<AtomicBool>,
    mut progress: impl FnMut(&Outcome),
) -> Report {
    let mut outcomes = Vec::with_capacity(steps.len());
    let mut cancelled = false;

    for step in steps {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        // **Rebuilt per step, never hoisted.** The previous step's captures were written to the
        // environment on disk, and this step has to resolve against them — hoisting this out of
        // the loop is the one change that would make a login-then-use flow silently send an
        // empty token.
        let resolver = resolver_for(collection_root, environment);
        let outcome = run_step(engine, step, collection_root, environment, &resolver, cancel);
        progress(&outcome);
        outcomes.push(outcome);
    }

    Report {
        outcomes,
        // The flag, not just whether the loop broke on it: cancelling during the *last* step
        // leaves nothing to break out of, and a report that then claims it finished is the one
        // lie this panel must not tell.
        cancelled: cancelled || cancel.load(Ordering::Relaxed),
    }
}

fn run_step(
    engine: &Engine,
    step: Step,
    collection_root: &std::path::Path,
    environment: Option<&str>,
    resolver: &Resolver,
    cancel: &Arc<AtomicBool>,
) -> Outcome {
    let Step { label, spec } = step;
    let Some(spec) = spec else {
        return Outcome {
            label,
            status: None,
            duration: Duration::ZERO,
            error: None,
            unresolved: Some("this request is no longer in the collection".to_string()),
            failures: Vec::new(),
            captured: Vec::new(),
        };
    };
    let expect_status = spec.expect_status;
    let assertions = spec.assertions.clone();
    let captures = spec.captures.clone();

    let (job, events) = engine.send(resolver.apply(&spec));

    let mut response = None;
    let mut error = None;
    let mut cancelling = false;

    // **Polled rather than `recv_blocking`, and that is the whole point.** A blocking receive
    // only returns when an *event* arrives, so the cancel flag is only ever read between events
    // — and a request that has sent `Started` and then hangs produces none at all. The stop
    // button would do nothing on exactly the request you most want to stop. 10ms of granularity
    // is nothing beside HTTP latency, and this is a background thread.
    loop {
        if !cancelling && cancel.load(Ordering::Relaxed) {
            engine.cancel(job);
            cancelling = true;
        }

        match events.try_recv() {
            Ok(Event::Done { response: got, .. }) => {
                response = Some(got);
                break;
            }
            Ok(Event::Failed { error: got, .. }) => {
                error = Some(got);
                break;
            }
            Ok(_) => {}
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            // The engine dropped the sender without a terminal event, which means it is gone.
            Err(async_channel::TryRecvError::Closed) => break,
        }
    }

    let Some(response) = response else {
        return Outcome {
            label,
            status: None,
            duration: Duration::ZERO,
            error,
            unresolved: None,
            failures: Vec::new(),
            captured: Vec::new(),
        };
    };

    // **Parsed rather than sniffed.** A `content-type` of `text/plain` on a JSON body is common
    // enough that asserting on it should still work, and a body that is not JSON fails at its
    // first byte — so trying costs nothing and refusing on a header costs a real case.
    let outline = JsonOutline::parse(response.body.clone()).ok();
    let failures = assertion::check_all(
        outline.as_ref(),
        expect_status,
        response.status,
        &assertions,
    );

    let captured = match (&outline, environment) {
        (Some(outline), Some(name)) if !captures.is_empty() && failures.is_empty() => {
            publish_captures(collection_root, name, outline, &captures)
        }
        // Captures do not run when the step failed, for the reason they do not run on a failed
        // send in the app: an error body has fields too, and publishing one leaves the *next*
        // step failing for a reason nothing points at.
        _ => Vec::new(),
    };

    Outcome {
        label,
        status: Some(response.status),
        duration: response.timing.total,
        error: None,
        unresolved: None,
        failures,
        captured,
    }
}

fn publish_captures(
    collection_root: &std::path::Path,
    environment: &str,
    outline: &JsonOutline,
    captures: &[crate::capture::Capture],
) -> Vec<String> {
    let mut file = environment::read(collection_root, environment)
        .unwrap_or_else(|_| EnvironmentFile {
            name: environment.to_string(),
            ..Default::default()
        });

    let published = crate::capture::publish(&mut file, outline, captures);
    if environment::save(collection_root, &file).is_err() {
        return Vec::new();
    }
    published.written
}

fn resolver_for(collection_root: &std::path::Path, environment: Option<&str>) -> Resolver {
    let globals = environment::load(collection_root, environment::GLOBALS).ok();
    let active = environment.and_then(|name| environment::load(collection_root, name).ok());
    Resolver::new(globals.as_ref(), active.as_ref())
}

/// Every request under `root`, in the order `scan` already sorts them.
pub fn steps_in_folder(collection_root: &std::path::Path, folder: &std::path::Path) -> Vec<Step> {
    crate::collection::scan(folder)
        .into_iter()
        .map(|entry| Step {
            // Relative to the *collection*, not to the folder, so a report row names the same
            // path the panel does.
            label: entry
                .path
                .strip_prefix(collection_root)
                .map(|path| path.display().to_string())
                .unwrap_or(entry.relative),
            spec: Some(entry.spec),
        })
        .collect()
}

/// A flow's steps, resolved against the collection.
///
/// A step naming a file that is gone becomes a `Step` with no spec, which the run reports as a
/// failure — the flow is stale and you have to be told, not quietly given a shorter run.
pub fn steps_for_flow(collection_root: &std::path::Path, flow: &crate::flow::Flow) -> Vec<Step> {
    flow.steps
        .iter()
        .map(|relative| Step {
            label: relative.clone(),
            spec: crate::collection::read(&collection_root.join(relative)).ok(),
        })
        .collect()
}
