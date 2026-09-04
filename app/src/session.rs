//! Persisting the open buffers across restarts.
//!
//! Window session only — which requests were open, which was in front, and where each one
//! came from. The collections themselves live in `zuno_core::collection` as one file per
//! request; this file deliberately holds only the *ephemeral* half, and that split is the
//! §12 persistence decision.
//!
//! **Why a versioned envelope.** M1 wrote one bare serialized `RequestSpec`, which cannot
//! represent more than one open buffer. A format change is precisely what broke every saved
//! session when `cookie_store` was added (CLAUDE.md, "Lessons"), so the envelope carries a
//! version and `load` migrates every older shape forward instead of discarding it:
//!
//! | On disk | Shipped in | Read as |
//! |---|---|---|
//! | a bare `RequestSpec` | M1 | one scratch tab |
//! | `{version: 1, active, tabs: [RequestSpec]}` | tabs, first slice | tabs with no collection path |
//! | `{version: 2, active, tabs: [{spec, path}]}` | collections | tabs, no environment |
//! | `{version: 3, …, environment}` | environments | current |
//! | `{version: 4, …, collection_panel}` | the collection panel | current |
//!
//! Those migrations are the reason the version exists, and each one is covered by a test —
//! there is no separate migration step to forget to run.
//!
//! The destination is a **global rather than a hardcoded path** so tests can point it at
//! a temp directory. Without that, running the suite would overwrite the developer's own
//! session file — the tests drive `SendRequest`, and a send is a save point.

use std::path::PathBuf;

use gpui::{App, Global, Task};
use serde::{Deserialize, Serialize};
use zuno_core::RequestSpec;

/// Bumped when the on-disk shape changes. A file claiming a *newer* version is refused
/// rather than guessed at — see `parse`.
const CURRENT_VERSION: u32 = 4;

/// One open buffer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub spec: RequestSpec,
    /// The collection file this buffer was opened from or saved to, if any.
    ///
    /// Persisted so that Ctrl+S after a restart overwrites the request's own file instead
    /// of deriving a fresh name and breeding `posts-2.json`, `posts-3.json`… A derived
    /// filename is not an identity, so this is the only thing that ties a buffer to a file.
    pub path: Option<PathBuf>,
}

impl Tab {
    /// A buffer with no collection file behind it yet.
    pub fn scratch(spec: RequestSpec) -> Self {
        Self { spec, path: None }
    }
}

/// Every open buffer, and which one was in front.
///
/// Fields are **required, not `#[serde(default)]`**, and that's load-bearing: a required
/// `version` is what lets `parse` tell an envelope apart from M1's bare `RequestSpec`.
/// Default `tabs` and a legacy file parses as an envelope with zero tabs, silently
/// discarding the user's request instead of migrating it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    version: u32,
    pub active: usize,
    pub tabs: Vec<Tab>,
    /// The selected environment's name, or `None` for no environment.
    ///
    /// Window state rather than collection state: it's "what am I pointed at right now",
    /// and two people sharing a collection through git should not fight over whose turn it
    /// is to be pointed at prod.
    pub environment: Option<String>,
    /// Whether the collection panel is showing.
    ///
    /// Window state for the same reason as `environment`, and persisted rather than
    /// defaulted because a panel you dismissed reappearing on every launch is the kind of
    /// small disobedience that makes an app feel like it isn't listening.
    pub collection_panel: bool,
}

impl Session {
    pub fn new(
        tabs: Vec<Tab>,
        active: usize,
        environment: Option<String>,
        collection_panel: bool,
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            active,
            tabs,
            environment,
            collection_panel,
        }
    }

    /// The shape M1 persisted, expressed in the current format.
    pub fn single(spec: RequestSpec) -> Self {
        Self::new(vec![Tab::scratch(spec)], 0, None, DEFAULT_PANEL)
    }
}

/// Just enough of any envelope to decide how to read the rest of it.
///
/// Dispatching on the declared version beats inferring the shape: the two happen to be
/// distinguishable today, but that's luck, and a wrong guess silently loses a field rather
/// than failing loudly.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

/// `{version: 1, active, tabs: [RequestSpec]}` — tabs before collections existed, so no
/// buffer had a file behind it.
#[derive(Deserialize)]
struct SessionV1 {
    active: usize,
    tabs: Vec<RequestSpec>,
}

/// `{version: 2, active, tabs: [{spec, path}]}` — before environments, so nothing was
/// selected. Identical to v3 apart from the missing field, but spelled out rather than
/// given a serde default: invariant 8 exists because a defaulted field turns "written by an
/// older Zuno" into "written by this one, with everything empty".
#[derive(Deserialize)]
struct SessionV2 {
    active: usize,
    tabs: Vec<Tab>,
}

/// `{version: 3, active, tabs, environment}` — before the collection panel existed, so no
/// window had one to remember. Spelled out rather than given a serde default, per invariant 8:
/// a defaulted field cannot tell "written by an older Zuno" from "written by this one, with the
/// panel hidden", and those two want *opposite* answers — an older file should adopt today's
/// default, a current one should be obeyed.
#[derive(Deserialize)]
struct SessionV3 {
    active: usize,
    tabs: Vec<Tab>,
    environment: Option<String>,
}

/// What a session written before the panel existed adopts, and what a fresh window starts with.
///
/// Visible: the panel is the only thing in Zuno that answers "what have I saved", and a browser
/// nobody discovers is the discoverability failure architecture.md §2 is mostly about.
const DEFAULT_PANEL: bool = true;

/// Where the session lives. `None` disables persistence entirely.
pub struct SessionFile(Option<PathBuf>);

impl Global for SessionFile {}

/// Point persistence at a specific file, or disable it with `None`.
///
/// Two callers: `app_state::resolve` sets it from the active workspace's id, and the test
/// harness sets it to a scratch file — the suite drives `SendRequest`, a send is a save point,
/// and without the override it would overwrite the developer's own session (invariant 6).
pub fn install_at(cx: &mut App, path: Option<PathBuf>) {
    cx.set_global(SessionFile(path));
}

pub(crate) fn path(cx: &App) -> Option<PathBuf> {
    cx.try_global::<SessionFile>()?.0.clone()
}

/// Read the last session, or `None` if there isn't a usable one.
///
/// Every failure — missing, unreadable, malformed, or written by an incompatible version
/// — returns `None`. A corrupt session file must never stop the app from opening; the
/// worst it should cost is starting from the sample request.
pub fn load(cx: &App) -> Option<Session> {
    let path = path(cx)?;
    let bytes = std::fs::read(&path).ok()?;

    match parse(&bytes) {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!(
                "[zuno] ignoring unreadable session at {}: {error}",
                path.display()
            );
            None
        }
    }
}

/// Split out from `load` so the format and migration rules are testable without a
/// `SessionFile` global or a real file on disk.
fn parse(bytes: &[u8]) -> Result<Session, String> {
    let mut session = match serde_json::from_slice::<VersionProbe>(bytes) {
        Ok(probe) => match probe.version {
            4 => serde_json::from_slice::<Session>(bytes).map_err(|error| error.to_string())?,
            3 => {
                let v3 =
                    serde_json::from_slice::<SessionV3>(bytes).map_err(|error| error.to_string())?;
                Session::new(v3.tabs, v3.active, v3.environment, DEFAULT_PANEL)
            }
            2 => {
                let v2 =
                    serde_json::from_slice::<SessionV2>(bytes).map_err(|error| error.to_string())?;
                Session::new(v2.tabs, v2.active, None, DEFAULT_PANEL)
            }
            1 => {
                let v1 =
                    serde_json::from_slice::<SessionV1>(bytes).map_err(|error| error.to_string())?;
                Session::new(
                    v1.tabs.into_iter().map(Tab::scratch).collect(),
                    v1.active,
                    None,
                    DEFAULT_PANEL,
                )
            }
            newer => {
                return Err(format!(
                    "written by a newer Zuno (format v{newer}, this build reads v{CURRENT_VERSION})"
                ));
            }
        },
        // No version at all. Before giving up, try M1's format — one bare spec — and adopt
        // it as a single tab.
        Err(envelope_error) => match serde_json::from_slice::<RequestSpec>(bytes) {
            Ok(spec) => Session::single(spec),
            // Report the envelope's error, not the legacy one: for anything written by
            // this version of Zuno, that's the message describing the real problem.
            Err(_) => return Err(envelope_error.to_string()),
        },
    };

    // No tabs is not a usable session — it would open a window with nothing in it. Treat
    // it as absent so the caller falls back to the sample request.
    if session.tabs.is_empty() {
        return Err("no open requests in it".to_string());
    }

    // A hand-edited or truncated file can point past the end. Clamping here means
    // `views[active_ix]` can never panic; the alternative is every read site guarding.
    if session.active >= session.tabs.len() {
        session.active = 0;
    }

    Ok(session)
}

/// Write the open buffers, blocking until it lands. Best-effort: a failure is reported but
/// never fatal.
///
/// Use this only where the write genuinely has to finish before the next thing happens — the
/// quit hook, where the process is about to go away, and explicit user actions small enough that
/// a person wants to know they landed. A *send* is neither; see `save_in_background`.
pub fn save(session: &Session, cx: &App) {
    let Some(path) = path(cx) else {
        return;
    };
    write_to(&path, session);
}

/// Write the open buffers on a background thread, returning the task.
///
/// **The caller must hold the task**: dropping it cancels the write.
///
/// Serializing every open buffer is real work — `Session` carries a full `RequestSpec` per tab,
/// bodies included, so fifty tabs is megabytes through `to_vec_pretty` — and then the write
/// blocks. A send is the wrong moment for it: architecture.md §8 budgets 5ms from the Send
/// keypress to bytes on the wire, and this used to sit inside that budget along with a
/// `create_dir_all`.
///
/// Assembling the `Session` still has to happen on the UI thread, because only it can read the
/// buffers — but that part is a clone, not a format, which is why this takes an owned `Session`
/// rather than a `&`.
pub fn save_in_background(session: Session, cx: &App) -> Task<()> {
    let Some(path) = path(cx) else {
        return Task::ready(());
    };
    cx.background_executor()
        .spawn(async move { write_to(&path, &session) })
}

/// Serialize and write, reporting failures without propagating them.
///
/// In place rather than write-to-temp-and-rename, unlike `collection::write`, and the difference
/// is deliberate: a truncated session costs you the tab layout, while a truncated collection file
/// costs a request you may have intended to keep.
fn write_to(path: &std::path::Path, session: &Session) {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("[zuno] could not create {}: {error}", parent.display());
        return;
    }

    match serde_json::to_vec_pretty(session) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(path, bytes) {
                eprintln!("[zuno] could not save session: {error}");
            }
        }
        Err(error) => eprintln!("[zuno] could not serialize session: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_survives_a_round_trip_through_json() {
        let spec = RequestSpec::sample();
        let json = serde_json::to_vec_pretty(&spec).expect("serialize");
        let back: RequestSpec = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(spec, back);
    }

    /// Distinguishable from `sample()` by name, so tab order can be asserted.
    fn named(name: &str) -> RequestSpec {
        RequestSpec {
            name: name.to_string(),
            ..RequestSpec::sample()
        }
    }

    #[test]
    fn many_tabs_and_the_active_index_survive_a_round_trip() {
        let session = Session::new(
            vec![
                Tab::scratch(named("first")),
                Tab {
                    spec: named("second"),
                    path: Some(PathBuf::from("/collections/second.json")),
                },
                Tab::scratch(named("third")),
            ],
            2,
            Some("dev".to_string()),
            false,
        );
        let json = serde_json::to_vec_pretty(&session).expect("serialize");

        let back = parse(&json).expect("parse");
        assert_eq!(back, session);
        assert_eq!(back.active, 2);
        let names: Vec<&str> = back.tabs.iter().map(|tab| tab.spec.name.as_str()).collect();
        assert_eq!(names, ["first", "second", "third"]);
        assert_eq!(
            back.tabs[1].path.as_deref(),
            Some(std::path::Path::new("/collections/second.json")),
            "a buffer's collection file must survive a restart"
        );
        assert_eq!(
            back.environment.as_deref(),
            Some("dev"),
            "the selected environment must survive a restart"
        );
    }

    #[test]
    fn a_session_written_by_m1_opens_as_a_single_tab() {
        // The exact bytes M1 wrote: one bare spec, no envelope around it. This is the
        // oldest migration path, and the test that would have caught the `cookie_store`
        // breakage described in CLAUDE.md.
        let spec = named("saved by m1");
        let legacy = serde_json::to_vec_pretty(&spec).expect("serialize");

        let session = parse(&legacy).expect("a bare spec must still load");
        assert_eq!(session.tabs, vec![Tab::scratch(spec)]);
        assert_eq!(session.active, 0);
    }

    #[test]
    fn a_v1_envelope_migrates_to_tabs_without_paths() {
        // Written by the first tabs slice, before collections existed: `tabs` is a list of
        // bare specs rather than of `{spec, path}`.
        let json = format!(
            r#"{{"version":1,"active":1,"tabs":[{},{}]}}"#,
            serde_json::to_string(&named("one")).expect("serialize"),
            serde_json::to_string(&named("two")).expect("serialize"),
        );

        let session = parse(json.as_bytes()).expect("a v1 envelope must still load");
        assert_eq!(session.tabs.len(), 2, "both buffers must survive");
        assert_eq!(session.active, 1, "and which one was in front");
        assert_eq!(session.tabs[0].spec.name, "one");
        assert!(
            session.tabs.iter().all(|tab| tab.path.is_none()),
            "no buffer had a collection file to remember"
        );
    }

    #[test]
    fn a_v3_envelope_migrates_and_adopts_the_default_panel() {
        // Written before the collection panel existed, so the file has no opinion about it and
        // must adopt today's default rather than a bare `false` — which is what a
        // `#[serde(default)]` would have produced, and is invariant 8's whole argument.
        let json = format!(
            r#"{{"version":3,"active":1,"tabs":[{{"spec":{},"path":null}},{{"spec":{},"path":"/c/two.json"}}],"environment":"dev"}}"#,
            serde_json::to_string(&named("one")).expect("serialize"),
            serde_json::to_string(&named("two")).expect("serialize"),
        );

        let session = parse(json.as_bytes()).expect("a v3 envelope must still load");
        assert_eq!(session.tabs.len(), 2, "both buffers must survive");
        assert_eq!(session.active, 1);
        assert_eq!(session.environment.as_deref(), Some("dev"));
        assert_eq!(
            session.tabs[1].path,
            Some(PathBuf::from("/c/two.json")),
            "the collection file each buffer came from must survive"
        );
        assert_eq!(
            session.collection_panel, DEFAULT_PANEL,
            "an older file has no stored preference and must take the default"
        );
    }

    #[test]
    fn a_hidden_panel_stays_hidden_across_a_restart() {
        // The half a defaulted field could not express: v4 says `false` because the reader
        // dismissed it, and that has to be obeyed rather than overwritten by the default the
        // migration above applies.
        let session = Session::new(vec![Tab::scratch(named("only"))], 0, None, false);
        let json = serde_json::to_vec_pretty(&session).expect("serialize");

        let parsed = parse(&json).expect("a v4 envelope must load");
        assert!(
            !parsed.collection_panel,
            "a dismissed panel must not reappear on the next launch"
        );
    }

    #[test]
    fn an_active_index_past_the_end_is_clamped_rather_than_panicking() {
        // A truncated or hand-edited file. Left alone, this indexes out of bounds at the
        // first render.
        let session = Session::new(vec![Tab::scratch(named("only"))], 7, None, DEFAULT_PANEL);
        let json = serde_json::to_vec_pretty(&session).expect("serialize");

        let back = parse(&json).expect("parse");
        assert_eq!(back.active, 0);
        assert!(back.tabs.get(back.active).is_some());
    }

    #[test]
    fn a_newer_format_is_refused_rather_than_misread() {
        // Better to reopen at the sample than to silently drop fields a future version
        // added. The message has to name the versions, since this is the one failure a
        // person can act on — by upgrading.
        let error = parse(br#"{"version":99,"active":0,"tabs":[]}"#).expect_err("must refuse");
        assert!(error.contains("99"), "{error}");
        assert!(error.contains("newer Zuno"), "{error}");
    }

    #[test]
    fn an_envelope_with_no_tabs_is_not_a_usable_session() {
        assert!(parse(br#"{"version":2,"active":0,"tabs":[]}"#).is_err());
        assert!(parse(br#"{"version":1,"active":0,"tabs":[]}"#).is_err());
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(parse(b"not json at all").is_err());
        // A JSON object that is neither an envelope nor a spec must also fail cleanly.
        assert!(parse(br#"{"unexpected":true}"#).is_err());
        // A well-formed envelope whose tabs are nonsense must fail rather than half-load.
        assert!(parse(br#"{"version":2,"active":0,"tabs":[{"nope":1}]}"#).is_err());
    }
}
