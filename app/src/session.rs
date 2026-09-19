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
//! | `{version: 4, …, collection_panel}` | the collection panel | tabs, with a fixed-width panel |
//! | `{version: 5, …, panel_width}` | a resizable panel | current |
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

/// What a session written before the panel could be resized adopts, and what a fresh window
/// starts with. Taken from `collection_panel` rather than restated, so the default and the
/// double-click reset target cannot drift into two different numbers.
use crate::collection_panel::DEFAULT_WIDTH;

/// Bumped when the on-disk shape changes. A file claiming a *newer* version is refused
/// rather than guessed at — see `parse`.
const CURRENT_VERSION: u32 = 5;

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
/// A tab this build cannot read, kept verbatim so that opening a workspace in an older Zuno
/// does not delete it.
///
/// **Carrying, not skipping, and the difference is the whole point.** A session is one file
/// holding every open buffer, and serde fails an entire `Vec` on one bad element — so a tab of
/// a kind this build predates would otherwise take every other tab in the window with it. That
/// is what GraphQL does to 0.2.9 today.
///
/// Skipping it is only half a fix: the next save would write the survivors, and the skipped tab
/// would be gone for good — the same silent data loss, one release later. Keeping the raw bytes
/// and splicing them back at the same index means a round trip through an older build costs
/// nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct CarriedTab {
    /// Position in the file's `tabs` array, so it goes back where it came from.
    pub at: usize,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    version: u32,
    /// Index into `tabs` — *this build's* tabs, with carried ones excluded. Converted to and
    /// from the file's own indexing at the edges, so nothing else has to think about it.
    pub active: usize,
    pub tabs: Vec<Tab>,
    /// Tabs written by a newer Zuno, passed through untouched. Never empty only in the
    /// forward-compatibility case; a file this build fully understands carries none.
    pub carried: Vec<CarriedTab>,
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
    /// How wide the collection panel is, in pixels.
    ///
    /// Stored unclamped, and read back through `collection_panel::clamp_width`: the ceiling
    /// depends on the window, so a width that was legal on a wide monitor has to be reined in
    /// when the same session opens on a laptop rather than rejected at load.
    pub panel_width: f32,
}

/// The on-disk shape of a v5 session.
///
/// `tabs` is a `Vec<Value>` rather than a `Vec<Tab>` so that each one can be parsed on its own
/// and a failure confined to that entry — see `CarriedTab`.
#[derive(Serialize, Deserialize)]
struct StoredSession {
    version: u32,
    active: usize,
    tabs: Vec<serde_json::Value>,
    environment: Option<String>,
    collection_panel: bool,
    panel_width: f32,
}

impl Serialize for Session {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Carried tabs go back where they came from, so a file round-tripped through a build
        // that could not read one of its tabs comes out with that tab still in it.
        //
        // **`at` is a hint, not a gate.** It is the position in the file this session was
        // *loaded* from, and the window has moved on since: closing a readable tab leaves the
        // recorded index pointing past the end of what is being written. An earlier version
        // walked `0..total` and stopped when it ran out of readable tabs, which silently
        // dropped any carried tab whose index it never reached — losing exactly the data this
        // whole mechanism exists to keep. Anything unplaced is appended instead.
        let mut carried: Vec<&CarriedTab> = self.carried.iter().collect();
        carried.sort_by_key(|tab| tab.at);
        let mut carried = carried.into_iter().peekable();

        let mut tabs = Vec::with_capacity(self.tabs.len() + self.carried.len());
        // Where each of *this build's* tabs landed, so `active` can be mapped without
        // arithmetic that has to stay in step with the loop.
        let mut placed = Vec::with_capacity(self.tabs.len());

        for tab in &self.tabs {
            while carried.peek().is_some_and(|next| next.at <= tabs.len()) {
                tabs.push(carried.next().expect("peeked").raw.clone());
            }
            placed.push(tabs.len());
            tabs.push(serde_json::to_value(tab).map_err(serde::ser::Error::custom)?);
        }
        // Whatever is left over — including indices past the end — rather than dropped.
        for tab in carried {
            tabs.push(tab.raw.clone());
        }

        StoredSession {
            version: self.version,
            active: placed.get(self.active).copied().unwrap_or(0),
            tabs,
            environment: self.environment.clone(),
            collection_panel: self.collection_panel,
            panel_width: self.panel_width,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Session {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = StoredSession::deserialize(deserializer)?;

        let mut tabs = Vec::new();
        let mut carried = Vec::new();
        let mut active = stored.active;

        for (at, raw) in stored.tabs.into_iter().enumerate() {
            match serde_json::from_value::<Tab>(raw.clone()) {
                Ok(tab) => tabs.push(tab),
                Err(_) => {
                    // A tab of a kind this build predates. Kept, not dropped.
                    carried.push(CarriedTab { at, raw });
                    if at < active {
                        active -= 1;
                    } else if at == active {
                        // The buffer that was in front is one this build cannot show; the
                        // nearest readable one is the honest fallback.
                        active = active.saturating_sub(1);
                    }
                }
            }
        }

        Ok(Session {
            version: stored.version,
            active,
            tabs,
            carried,
            environment: stored.environment,
            collection_panel: stored.collection_panel,
            panel_width: stored.panel_width,
        })
    }
}

impl Session {
    pub fn new(
        tabs: Vec<Tab>,
        active: usize,
        environment: Option<String>,
        collection_panel: bool,
        panel_width: f32,
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            active,
            tabs,
            carried: Vec::new(),
            environment,
            collection_panel,
            panel_width,
        }
    }

    /// The shape M1 persisted, expressed in the current format.
    pub fn single(spec: RequestSpec) -> Self {
        Self::new(vec![Tab::scratch(spec)], 0, None, DEFAULT_PANEL, DEFAULT_WIDTH)
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

/// `{version: 4, active, tabs, environment, collection_panel}` — the panel before it could be
/// resized, so no window had a width to remember. Spelled out rather than defaulted for the
/// same reason as every arm above it.
#[derive(Deserialize)]
struct SessionV4 {
    active: usize,
    tabs: Vec<Tab>,
    environment: Option<String>,
    collection_panel: bool,
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
            // **Moved aside, not left in place, and this is the half that matters.**
            // Returning `None` starts the window from the sample request — and the quit hook
            // then saves *that* over the file, so an unreadable session is silently replaced
            // by a one-tab default and whatever was open is gone. That is not hypothetical:
            // a session written by a newer build did exactly this to a workspace's open tabs,
            // and the read failure was only the visible half of it.
            //
            // Renaming rather than copying, so there is exactly one copy and no doubt about
            // which file is live. A failure to rename is reported and otherwise ignored: the
            // app still has to open, which is what the doc comment above promises.
            let salvage = path.with_extension("json.bak");
            let saved = std::fs::rename(&path, &salvage).is_ok();

            eprintln!(
                "[zuno] ignoring unreadable session at {}: {error}",
                path.display()
            );
            if saved {
                eprintln!("[zuno] the previous session was kept at {}", salvage.display());
            }
            None
        }
    }
}

/// Split out from `load` so the format and migration rules are testable without a
/// `SessionFile` global or a real file on disk.
fn parse(bytes: &[u8]) -> Result<Session, String> {
    let mut session = match serde_json::from_slice::<VersionProbe>(bytes) {
        Ok(probe) => match probe.version {
            5 => serde_json::from_slice::<Session>(bytes).map_err(|error| error.to_string())?,
            4 => {
                let v4 =
                    serde_json::from_slice::<SessionV4>(bytes).map_err(|error| error.to_string())?;
                Session::new(
                    v4.tabs,
                    v4.active,
                    v4.environment,
                    v4.collection_panel,
                    DEFAULT_WIDTH,
                )
            }
            3 => {
                let v3 =
                    serde_json::from_slice::<SessionV3>(bytes).map_err(|error| error.to_string())?;
                Session::new(v3.tabs, v3.active, v3.environment, DEFAULT_PANEL, DEFAULT_WIDTH)
            }
            2 => {
                let v2 =
                    serde_json::from_slice::<SessionV2>(bytes).map_err(|error| error.to_string())?;
                Session::new(v2.tabs, v2.active, None, DEFAULT_PANEL, DEFAULT_WIDTH)
            }
            1 => {
                let v1 =
                    serde_json::from_slice::<SessionV1>(bytes).map_err(|error| error.to_string())?;
                Session::new(
                    v1.tabs.into_iter().map(Tab::scratch).collect(),
                    v1.active,
                    None,
                    DEFAULT_PANEL,
                    DEFAULT_WIDTH,
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
            // Not the default: a round trip that writes 232 and reads 232 would hold with the
            // field dropped from the struct entirely.
            340.0,
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

    /// **The upgrade case the version number cannot see.** A session written by 0.2.9 is
    /// already `version: 5`, so `parse` takes the current arm and hands the bytes straight to
    /// serde — but the *specs* inside it are in the pre-`RequestKind` shape. Nothing about the
    /// envelope says so, which is exactly why this needs its own test rather than relying on
    /// the version dispatch: the only thing standing between a returning user and an empty
    /// window is `RequestSpec`'s own compatibility shim.
    ///
    /// The spec bytes below are the shape 0.2.9 wrote, not a re-serialization of the current
    /// struct, so this fails if that shim is removed.
    /// **A tab this build cannot read must not take the rest of the window with it.**
    ///
    /// This is what GraphQL does to 0.2.9 today: one tab it cannot parse fails the whole `Vec`,
    /// so every *readable* tab in that session is discarded too. The fix cannot reach 0.2.9,
    /// but it means the next kind — gRPC, MQTT — cannot do the same to this build.
    #[test]
    fn a_tab_written_by_a_newer_zuno_does_not_discard_the_readable_ones() {
        let known = serde_json::to_string(&named("keep-me")).expect("serialize");
        // A kind this build has never heard of.
        let json = format!(
            r#"{{"version":5,"active":2,"tabs":[
                {{"spec":{known},"path":null}},
                {{"spec":{{"id":0,"name":"grpc","url":"https://a.test","headers":[],
                   "settings":{{"timeout":{{"secs":30,"nanos":0}},"follow_redirects":true,
                   "max_redirects":10,"verify_tls":true,"accept_encodings":true,
                   "cookie_store":true}},
                   "kind":{{"Grpc":{{"service":"S","method":"M"}}}},
                   "captures":[],"expect_status":null,"assertions":[]}},"path":null}},
                {{"spec":{known},"path":"/c/two.json"}}
            ],"environment":null,"collection_panel":true,"panel_width":240.0}}"#
        );

        let session = parse(json.as_bytes()).expect("the readable tabs must still load");
        assert_eq!(session.tabs.len(), 2, "both readable tabs must survive");
        assert_eq!(session.carried.len(), 1, "the unreadable one must be carried, not dropped");
        assert_eq!(session.carried[0].at, 1, "and remember where it sat");
        // `active` was 2 in file terms; with one tab ahead of it unreadable, it is 1 here.
        assert_eq!(session.active, 1);
    }

    /// **And carrying it is only half the fix — it has to be written back.**
    ///
    /// Skipping an unreadable tab on load and saving the survivors would delete it for good,
    /// which is the same silent loss one release later. Asserted on the bytes: the unknown kind
    /// must still be in the file, at the same index, after a round trip.
    #[test]
    fn a_carried_tab_survives_being_written_back_out() {
        let known = serde_json::to_string(&named("keep-me")).expect("serialize");
        let json = format!(
            r#"{{"version":5,"active":0,"tabs":[
                {{"spec":{known},"path":null}},
                {{"spec":{{"id":0,"name":"grpc","url":"https://a.test","headers":[],
                   "settings":{{"timeout":{{"secs":30,"nanos":0}},"follow_redirects":true,
                   "max_redirects":10,"verify_tls":true,"accept_encodings":true,
                   "cookie_store":true}},
                   "kind":{{"Grpc":{{"service":"S","method":"M"}}}},
                   "captures":[],"expect_status":null,"assertions":[]}},"path":null}}
            ],"environment":null,"collection_panel":true,"panel_width":240.0}}"#
        );

        let session = parse(json.as_bytes()).expect("parse");
        let written = serde_json::to_value(&session).expect("serialize");
        let tabs = written["tabs"].as_array().expect("tabs");

        assert_eq!(tabs.len(), 2, "the carried tab must be written back");
        assert_eq!(
            tabs[1]["spec"]["kind"]["Grpc"]["service"], "S",
            "verbatim, and at its original index: {written}"
        );

        // And it still parses on the way back in, so repeated open/quit cycles are stable.
        let again = parse(serde_json::to_vec(&session).unwrap().as_slice()).expect("reparse");
        assert_eq!(again.tabs.len(), 1);
        assert_eq!(again.carried.len(), 1);
    }

    /// **A carried tab survives the reader closing the tabs around it.**
    ///
    /// `CarriedTab::at` is a position in the file this session was *loaded* from, and closing a
    /// readable tab leaves it pointing past the end of what gets written. The first version of
    /// the splice walked `0..total` and stopped when it ran out of readable tabs, so a carried
    /// tab it never reached was silently dropped — the exact loss the mechanism exists to
    /// prevent. Both existing tests used a file whose counts happened to line up and saw none
    /// of it.
    #[test]
    fn a_carried_tab_is_kept_even_when_its_index_is_past_the_end() {
        let raw = serde_json::json!({"spec": {"kind": "future"}, "path": null});

        // Every readable tab has since been closed; `at` still says 1.
        let emptied = Session {
            version: CURRENT_VERSION,
            active: 0,
            tabs: vec![],
            carried: vec![CarriedTab { at: 1, raw: raw.clone() }],
            environment: None,
            collection_panel: true,
            panel_width: 240.0,
        };
        let out = serde_json::to_value(&emptied).expect("serialize");
        assert_eq!(
            out["tabs"].as_array().map(Vec::len),
            Some(1),
            "a carried tab must not be dropped because its index outran the file: {out}"
        );

        // And one that still fits keeps its place, with `active` pointing at the right buffer.
        let mixed = Session {
            version: CURRENT_VERSION,
            active: 1,
            tabs: vec![Tab::scratch(named("one")), Tab::scratch(named("two"))],
            carried: vec![CarriedTab { at: 0, raw }],
            environment: None,
            collection_panel: true,
            panel_width: 240.0,
        };
        let out = serde_json::to_value(&mixed).expect("serialize");
        assert_eq!(out["tabs"].as_array().map(Vec::len), Some(3));
        assert_eq!(out["tabs"][0]["spec"]["kind"], "future", "carried tab keeps index 0");
        assert_eq!(
            out["active"], 2,
            "active must follow the buffer it named, not its old index: {out}"
        );
    }

    /// **An unreadable session is moved aside rather than left to be overwritten.**
    ///
    /// The failure this guards is not the read — it is what happens *next*: `load` returns
    /// `None`, the window opens on the sample request, and the quit hook saves that over the
    /// file. The user's open tabs are then gone with nothing to recover from. Asserted at the
    /// consequence — the original bytes still exist somewhere — rather than by checking the
    /// log line, which would pass against a version that printed and then let the file be
    /// clobbered.
    #[gpui::test]
    fn an_unreadable_session_is_kept_rather_than_silently_replaced(cx: &mut gpui::TestAppContext) {
        let dir = std::env::temp_dir().join(format!("zuno-salvage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("workspace.json");

        // A shape no build can read — stands in for "written by a newer Zuno".
        let unreadable = br#"{"version":5,"active":0,"tabs":[{"spec":{"nope":true},"path":null}]}"#;
        std::fs::write(&path, unreadable).expect("write");

        let loaded = cx.update(|cx| {
            install_at(cx, Some(path.clone()));
            load(cx)
        });

        assert!(loaded.is_none(), "an unreadable session must not open");
        assert!(
            !path.exists(),
            "the unreadable file must be moved, so the quit hook cannot overwrite it in place"
        );

        let salvage = path.with_extension("json.bak");
        assert_eq!(
            std::fs::read(&salvage).expect("the salvaged file must exist"),
            unreadable,
            "the original bytes must survive byte-for-byte, or there is nothing to recover"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_v5_session_holding_pre_kind_specs_still_restores_its_buffers() {
        let legacy_spec = r#"{
            "id": 0,
            "name": "Untitled",
            "method": "Delete",
            "url": "https://a.test/one",
            "query": [],
            "headers": [],
            "body": "Empty",
            "settings": {
                "timeout": {"secs": 30, "nanos": 0},
                "follow_redirects": true,
                "max_redirects": 10,
                "verify_tls": true,
                "accept_encodings": true,
                "cookie_store": true
            },
            "captures": [],
            "expect_status": null,
            "assertions": []
        }"#;

        let json = format!(
            r#"{{"version":5,"active":0,"tabs":[{{"spec":{legacy_spec},"path":"/c/one.json"}}],"environment":"dev","collection_panel":true,"panel_width":240.0}}"#
        );

        let session = parse(json.as_bytes()).expect("a 0.2.9 session must still load");
        assert_eq!(session.tabs.len(), 1, "the buffer must survive the upgrade");
        assert_eq!(session.tabs[0].spec.url, "https://a.test/one");
        assert_eq!(
            session.tabs[0].spec.method(),
            Some(&zuno_core::Method::Delete),
            "the method has to come through the kind split, not be defaulted to GET"
        );
        assert_eq!(session.tabs[0].path, Some(PathBuf::from("/c/one.json")));
    }

    #[test]
    fn a_v4_envelope_migrates_and_adopts_the_default_panel_width() {
        // Written before the panel could be resized. Every other field is already in its
        // current shape, so this arm exists purely to supply the width — which is exactly the
        // case a `#[serde(default)]` would have handled invisibly, and invariant 8 forbids for
        // the reason the assertion below spells out: a defaulted `0.0` is indistinguishable
        // from a window that genuinely had no panel, and would collapse it.
        let json = format!(
            r#"{{"version":4,"active":1,"tabs":[{{"spec":{},"path":null}},{{"spec":{},"path":"/c/two.json"}}],"environment":"dev","collection_panel":false}}"#,
            serde_json::to_string(&named("one")).expect("serialize"),
            serde_json::to_string(&named("two")).expect("serialize"),
        );

        let session = parse(json.as_bytes()).expect("a v4 envelope must still load");
        assert_eq!(session.tabs.len(), 2, "both buffers must survive");
        assert_eq!(session.active, 1);
        assert_eq!(session.environment.as_deref(), Some("dev"));
        assert_eq!(
            session.tabs[1].path,
            Some(PathBuf::from("/c/two.json")),
            "the collection file each buffer came from must survive"
        );
        assert!(
            !session.collection_panel,
            "a v4 file *does* have an opinion about the panel and it must be obeyed"
        );
        assert_eq!(
            session.panel_width, DEFAULT_WIDTH,
            "but no opinion about its width, so it takes today's default"
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
        let session = Session::new(vec![Tab::scratch(named("only"))], 0, None, false, DEFAULT_WIDTH);
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
        let session = Session::new(vec![Tab::scratch(named("only"))], 7, None, DEFAULT_PANEL, DEFAULT_WIDTH);
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
