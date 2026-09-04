//! Where collections live on disk.
//!
//! The format itself is `zuno_core::collection` — one request per file, so a collection is
//! a directory you can commit. This module only answers *which* directory, and exists as a
//! global for the same reason `session::SessionFile` does: without an override, the test
//! suite would write into the developer's own collection.
//!
//! Config vs data: the session file is config-ish state Zuno manages for you, so it sits
//! under `XDG_CONFIG_HOME`. Collections are **your** documents — the whole point is that you
//! version and share them — so they follow `XDG_DATA_HOME` instead. A future setting should
//! let this point at a project directory, which is where git-diffable collections earn their
//! keep; that needs a settings panel, so it isn't wired up yet.

use std::path::{Path, PathBuf};

use gpui::{App, Global};

pub struct CollectionRoot(Option<PathBuf>);

impl Global for CollectionRoot {}

pub(crate) fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })?;

    Some(base.join("zuno").join("collections"))
}

/// Point collections at a specific directory, or disable saving with `None`.
///
/// Two callers, and they are the same idea from opposite ends: `app_state::resolve` sets this
/// from the active workspace, and the test harness sets it to a scratch directory (invariant 6).
pub fn install_at(cx: &mut App, path: Option<PathBuf>) {
    cx.set_global(CollectionRoot(path));
}

/// The collection root, or `None` when there is nowhere to save.
pub fn root(cx: &App) -> Option<&Path> {
    cx.try_global::<CollectionRoot>()?.0.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_path_sits_under_the_xdg_data_dir() {
        // Reads the real environment, so assert on shape rather than an exact value.
        if let Some(path) = default_path() {
            assert!(path.ends_with("zuno/collections"), "{path:?}");
            assert!(path.is_absolute(), "{path:?}");
            // Collections are documents, not config — mixing them into the config dir is
            // the mistake this asserts against.
            assert!(!path.to_string_lossy().contains("/.config/"), "{path:?}");
        }
    }
}
