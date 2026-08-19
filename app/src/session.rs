//! Persisting the open buffers across restarts.
//!
//! Window session only — which requests were open and which was in front. Collections,
//! environments, and the git-diffable file tree are still M2 (architecture.md §12); this
//! file deliberately holds *ephemeral* state, and that split is the half of §12 being
//! committed to here.
//!
//! **Why a versioned envelope rather than a bare `RequestSpec`.** M1 wrote one serialized
//! spec, which cannot represent more than one open buffer. Tabs need a list plus an active
//! index, so the format has to change — and a format change is precisely what broke every
//! saved session when `cookie_store` was added (CLAUDE.md, "Lessons"). So the envelope
//! carries a version from the start, and `load` falls back to reading a bare spec and
//! adopting it as a single tab. That fallback *is* the migration: existing
//! `~/.config/zuno/session.json` files keep working, and there's no separate migration
//! step to forget to run.
//!
//! The destination is a **global rather than a hardcoded path** so tests can point it at
//! a temp directory. Without that, running the suite would overwrite the developer's own
//! session file — the tests drive `SendRequest`, and a send is a save point.

use std::path::PathBuf;

use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use zuno_core::RequestSpec;

/// Bumped when the on-disk shape changes incompatibly. A file claiming a *newer* version
/// is refused rather than guessed at — see `load`.
const CURRENT_VERSION: u32 = 1;

/// Every open buffer, and which one was in front.
///
/// Fields are **required, not `#[serde(default)]`**, and that's load-bearing: it's what
/// lets `load` tell an envelope apart from M1's bare `RequestSpec`. Give `tabs` a default
/// and a legacy file would parse as an envelope with zero tabs, silently discarding the
/// user's request instead of migrating it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    version: u32,
    pub active: usize,
    pub tabs: Vec<RequestSpec>,
}

impl Session {
    pub fn new(tabs: Vec<RequestSpec>, active: usize) -> Self {
        Self {
            version: CURRENT_VERSION,
            active,
            tabs,
        }
    }

    /// The shape M1 persisted, expressed in the new format.
    pub fn single(spec: RequestSpec) -> Self {
        Self::new(vec![spec], 0)
    }
}

/// Where the scratch request lives. `None` disables persistence entirely.
pub struct SessionFile(Option<PathBuf>);

impl Global for SessionFile {}

/// Follows the XDG basedir spec directly rather than taking a `dirs`-style dependency —
/// it's a few lines, and Zuno is Linux-first for now. A macOS or Windows build will want
/// the platform-conventional location instead.
fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    Some(base.join("zuno").join("session.json"))
}

pub fn install(cx: &mut App) {
    cx.set_global(SessionFile(default_path()));
}

/// Point persistence at a specific file, or disable it with `None`.
///
/// Test-only for now. It exists because the suite drives `SendRequest`, a send is a save
/// point, and without an override the tests would overwrite the developer's own session.
#[cfg(test)]
pub fn install_at(cx: &mut App, path: Option<PathBuf>) {
    cx.set_global(SessionFile(path));
}

fn path(cx: &App) -> Option<PathBuf> {
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

/// Split out from `load` so the format rules are testable without a `SessionFile` global
/// or a real file on disk.
fn parse(bytes: &[u8]) -> Result<Session, String> {
    let mut session = match serde_json::from_slice::<Session>(bytes) {
        Ok(session) => session,
        // Not an envelope. Before giving up, try M1's format — one bare spec — and adopt
        // it as a single tab. Discrimination is unambiguous in both directions because
        // each type has required fields the other lacks.
        Err(envelope_error) => match serde_json::from_slice::<RequestSpec>(bytes) {
            Ok(spec) => return Ok(Session::single(spec)),
            // Report the envelope's error, not the legacy one: for anything written by
            // this version of Zuno, that's the message that describes the real problem.
            Err(_) => return Err(envelope_error.to_string()),
        },
    };

    // Checked *after* parsing rather than by probing the version first, because a future
    // format is overwhelmingly likely to fail the parse anyway — and when it doesn't, this
    // catches it. Either way nothing gets misread as v1.
    if session.version > CURRENT_VERSION {
        return Err(format!(
            "written by a newer Zuno (format v{}, this build reads v{CURRENT_VERSION})",
            session.version
        ));
    }

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

/// Write the open buffers. Best-effort: a failure is reported but never fatal.
pub fn save(session: &Session, cx: &App) {
    let Some(path) = path(cx) else {
        return;
    };

    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("[zuno] could not create {}: {error}", parent.display());
        return;
    }

    match serde_json::to_vec_pretty(session) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(&path, bytes) {
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
    fn the_default_path_sits_under_the_xdg_config_dir() {
        // Reads the real environment, so assert on shape rather than an exact value.
        if let Some(path) = default_path() {
            assert!(path.ends_with("zuno/session.json"), "{path:?}");
            assert!(path.is_absolute(), "{path:?}");
        }
    }

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
        let session = Session::new(vec![named("first"), named("second"), named("third")], 2);
        let json = serde_json::to_vec_pretty(&session).expect("serialize");

        let back = parse(&json).expect("parse");
        assert_eq!(back, session);
        assert_eq!(back.active, 2);
        let names: Vec<&str> = back.tabs.iter().map(|tab| tab.name.as_str()).collect();
        assert_eq!(names, ["first", "second", "third"]);
    }

    #[test]
    fn a_session_written_by_m1_opens_as_a_single_tab() {
        // The exact bytes M1 wrote: one bare spec, no envelope around it. This is the
        // migration path, and it's the test that would have caught the `cookie_store`
        // breakage described in CLAUDE.md.
        let spec = named("saved by m1");
        let legacy = serde_json::to_vec_pretty(&spec).expect("serialize");

        let session = parse(&legacy).expect("a bare spec must still load");
        assert_eq!(session.tabs, vec![spec]);
        assert_eq!(session.active, 0);
    }

    #[test]
    fn an_active_index_past_the_end_is_clamped_rather_than_panicking() {
        // A truncated or hand-edited file. Left alone, this indexes out of bounds at the
        // first render.
        let session = Session::new(vec![named("only")], 7);
        let json = serde_json::to_vec_pretty(&session).expect("serialize");

        let back = parse(&json).expect("parse");
        assert_eq!(back.active, 0);
        assert!(back.tabs.get(back.active).is_some());
    }

    #[test]
    fn a_newer_format_is_refused_rather_than_misread() {
        // Better to reopen at the sample than to silently drop fields a future version
        // added.
        let json = br#"{"version":99,"active":0,"tabs":[]}"#;
        assert!(parse(json).is_err());
    }

    #[test]
    fn an_envelope_with_no_tabs_is_not_a_usable_session() {
        let json = br#"{"version":1,"active":0,"tabs":[]}"#;
        assert!(parse(json).is_err());
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(parse(b"not json at all").is_err());
        // A JSON object that is neither an envelope nor a spec must also fail cleanly.
        assert!(parse(br#"{"unexpected":true}"#).is_err());
    }
}
