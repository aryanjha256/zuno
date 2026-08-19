//! Persisting the scratch request across restarts.
//!
//! The smallest useful slice of the persistence story (architecture.md §12): one file
//! holding the request you were last working on, so reopening Zuno puts you back where
//! you were instead of at a sample. Collections, environments, and the git-diffable file
//! tree are still M2 — deliberately, since that format choice gets cheaper to make with
//! a working loop in hand.
//!
//! The `Serialize` derives this relies on have been on `RequestSpec` since M1.0, for
//! exactly this moment.
//!
//! The destination is a **global rather than a hardcoded path** so tests can point it at
//! a temp directory. Without that, running the suite would overwrite the developer's own
//! session file — the tests drive `SendRequest`, and a send is a save point.

use std::path::PathBuf;

use gpui::{App, Global};
use zuno_core::RequestSpec;

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
pub fn load(cx: &App) -> Option<RequestSpec> {
    let path = path(cx)?;
    let bytes = std::fs::read(&path).ok()?;

    match serde_json::from_slice::<RequestSpec>(&bytes) {
        Ok(spec) => Some(spec),
        Err(error) => {
            eprintln!(
                "[zuno] ignoring unreadable session at {}: {error}",
                path.display()
            );
            None
        }
    }
}

/// Write the scratch request. Best-effort: a failure is reported but never fatal.
pub fn save(spec: &RequestSpec, cx: &App) {
    let Some(path) = path(cx) else {
        return;
    };

    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("[zuno] could not create {}: {error}", parent.display());
        return;
    }

    match serde_json::to_vec_pretty(spec) {
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

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(serde_json::from_slice::<RequestSpec>(b"not json at all").is_err());
        // A JSON object that isn't a spec must also fail cleanly.
        assert!(serde_json::from_slice::<RequestSpec>(br#"{"unexpected":true}"#).is_err());
    }
}
