//! Application-level state: which workspaces exist, which was last open, and the theme.
//!
//! **The split this file draws.** A *workspace* is a collection directory — your requests, your
//! folders, your `environments/`. Everything in one is yours and committable. What lives here is
//! Zuno's own bookkeeping about them, and it never gets written into your repo.
//!
//! Scoped accordingly: `app.json` under `XDG_CONFIG_HOME` holds the registry, the last workspace
//! and the theme; each workspace's open buffers live in `sessions/<id>.json` beside it. The
//! session was a single file for one hardcoded collection, which was fine while there was only
//! ever one — but every field in it (tabs are paths *into* a collection, `environment` names a
//! file in its `environments/`) is workspace-scoped, so a second workspace would have restored
//! buffers pointing at the first one's files.

use std::path::{Path, PathBuf};

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use crate::theme::Appearance;

/// The id of the workspace that has always existed: the XDG collections directory.
///
/// A literal rather than a generated id so it resolves through exactly the same lookup as any
/// other entry — the alternative is a branch on "is this the default one" at every read.
pub const DEFAULT_ID: &str = "default";

const CURRENT_VERSION: u32 = 1;

/// One registered workspace.
///
/// **No `name` field.** A workspace is named by its directory, so a `mv` cannot leave the name
/// lying — the same reasoning that makes tab labels derive from the URL rather than from a stored
/// `RequestSpec::name`, which a real session file was found doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub id: String,
    pub path: PathBuf,
}

/// `version` is required, for `session::Session`'s reason (invariant 8): a defaulted version
/// cannot tell "written by an older Zuno" from "written by this one, with everything empty".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AppFile {
    version: u32,
    theme: Appearance,
    /// The workspace to open. `None`, or an id no longer in `workspaces`, falls back to the
    /// first entry — a registry that names a workspace it does not hold must still start.
    last: Option<String>,
    workspaces: Vec<WorkspaceEntry>,
}

pub struct AppState {
    /// The config directory, or `None` when persistence is off (the test harness).
    dir: Option<PathBuf>,
    file: AppFile,
}

impl Global for AppState {}

impl AppState {
    fn fresh(default_workspace: Option<PathBuf>) -> AppFile {
        AppFile {
            version: CURRENT_VERSION,
            theme: Appearance::Dark,
            last: Some(DEFAULT_ID.to_string()),
            workspaces: default_workspace
                .map(|path| {
                    vec![WorkspaceEntry {
                        id: DEFAULT_ID.to_string(),
                        path,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    /// The workspace to open: `last` when it still exists, else the first entry.
    pub fn active(&self) -> Option<&WorkspaceEntry> {
        self.file
            .last
            .as_deref()
            .and_then(|id| self.file.workspaces.iter().find(|entry| entry.id == id))
            .or_else(|| self.file.workspaces.first())
    }
}

/// Where a new workspace lands unless the creator says otherwise — the IDE bargain: a default
/// location so naming one is enough, and an override for the case that matters, which is a
/// workspace inside the repo it belongs to.
pub fn default_new_location() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })?;

    Some(base.join("zuno").join("workspaces"))
}

/// A workspace's display name: its directory's own name.
///
/// Derived rather than stored, so a `mv` cannot leave a stale name behind — the same reason tab
/// labels come from the URL instead of `RequestSpec::name`.
pub fn label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Every registered workspace, in registration order.
pub fn workspaces(cx: &App) -> Vec<WorkspaceEntry> {
    cx.try_global::<AppState>()
        .map(|state| state.file.workspaces.clone())
        .unwrap_or_default()
}

pub fn active_id(cx: &App) -> Option<String> {
    cx.try_global::<AppState>()?
        .active()
        .map(|entry| entry.id.clone())
}

/// An id for a workspace at `path`, unique within the registry.
///
/// Derived from the directory name rather than random, so `sessions/payments.json` says which
/// workspace it belongs to — the same argument the collection format makes for deriving request
/// filenames instead of using an id. `default` is reserved, and a collision takes a suffix the
/// way `collection::allocate` does.
fn allocate_id(existing: &[WorkspaceEntry], path: &Path) -> String {
    let stem = path
        .file_name()
        // Lowercased on top of `slug`, which preserves case: an id *is* a filename, so
        // `Payments-API` and `payments-api` would be two registry entries fighting over one
        // session file on a case-insensitive filesystem. Ids are never shown — the label is the
        // directory's own name — so nothing is lost by flattening them.
        .map(|name| zuno_core::collection::slug(&name.to_string_lossy()).to_lowercase())
        .filter(|slug| !slug.is_empty() && slug != DEFAULT_ID)
        .unwrap_or_else(|| "workspace".to_string());

    let taken = |candidate: &str| existing.iter().any(|entry| entry.id == candidate);
    if !taken(&stem) {
        return stem;
    }
    (2..).map(|n| format!("{stem}-{n}")).find(|c| !taken(c)).expect("an id is always free")
}

/// Register `path`, returning its id. An already-registered path is returned as-is rather than
/// duplicated — two entries for one directory would mean two sessions fighting over it.
pub fn add_workspace(cx: &mut App, path: PathBuf) -> Option<String> {
    if cx.try_global::<AppState>().is_none() {
        return None;
    }
    let state = cx.global_mut::<AppState>();

    if let Some(entry) = state.file.workspaces.iter().find(|entry| entry.path == path) {
        return Some(entry.id.clone());
    }

    let id = allocate_id(&state.file.workspaces, &path);
    state.file.workspaces.push(WorkspaceEntry { id: id.clone(), path });
    save(cx);
    Some(id)
}

/// Drop a workspace from the registry and delete its session.
///
/// **Never touches the directory.** The registry is Zuno's bookkeeping and the folder is your
/// work — the same line `collection::remove` draws by refusing a directory outright. Refuses the
/// last entry, since a registry with nothing in it leaves the window with no collection at all.
pub fn forget_workspace(cx: &mut App, id: &str) -> bool {
    if cx.try_global::<AppState>().is_none() {
        return false;
    }
    let state = cx.global_mut::<AppState>();
    if state.file.workspaces.len() < 2 || !state.file.workspaces.iter().any(|e| e.id == id) {
        return false;
    }

    state.file.workspaces.retain(|entry| entry.id != id);
    if state.file.last.as_deref() == Some(id) {
        state.file.last = state.file.workspaces.first().map(|entry| entry.id.clone());
    }

    // Re-adding the same path later is a fresh start, so the old session would never be read
    // again — leaving it behind accumulates files nothing can reach.
    if let Some(dir) = state.dir.clone() {
        let _ = std::fs::remove_file(session_path(&dir, id));
    }

    save(cx);
    resolve(cx);
    true
}

/// Make `id` the active workspace, re-resolving the collection root and session file.
pub fn set_active(cx: &mut App, id: &str) -> bool {
    if cx.try_global::<AppState>().is_none() {
        return false;
    }
    let state = cx.global_mut::<AppState>();
    if !state.file.workspaces.iter().any(|entry| entry.id == id) {
        return false;
    }
    if state.file.last.as_deref() == Some(id) {
        return false;
    }

    state.file.last = Some(id.to_string());
    save(cx);
    resolve(cx);
    true
}

fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    Some(base.join("zuno"))
}

/// `sessions/<id>.json`, derived from the id rather than recorded in the registry — so an entry
/// can never point at a session belonging to another workspace.
pub fn session_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("sessions").join(format!("{id}.json"))
}

fn read(dir: &Path) -> Option<AppFile> {
    let bytes = std::fs::read(dir.join("app.json")).ok()?;
    match serde_json::from_slice::<AppFile>(&bytes) {
        Ok(file) if file.version <= CURRENT_VERSION => Some(file),
        Ok(file) => {
            eprintln!(
                "[zuno] ignoring app.json written by a newer Zuno (format v{}, this build reads \
                 v{CURRENT_VERSION})",
                file.version
            );
            None
        }
        Err(error) => {
            eprintln!("[zuno] ignoring unreadable app.json: {error}");
            None
        }
    }
}

/// Move the pre-registry session into place, once.
///
/// The old build kept one `session.json` for the one collection it could ever have. Copied
/// rather than moved: leaving the original means a downgrade still finds its session, and the
/// cost is one small stale file. Only ever runs when the new path is absent, so it cannot
/// overwrite a session this build wrote.
fn adopt_legacy_session(dir: &Path) {
    let legacy = dir.join("session.json");
    let target = session_path(dir, DEFAULT_ID);
    if target.exists() || !legacy.exists() {
        return;
    }

    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::copy(&legacy, &target) {
        eprintln!("[zuno] could not adopt the existing session: {error}");
    }
}

/// Create the default workspace's directory as it is registered.
///
/// Nothing else did: `collection::allocate` makes the root on the first save, so on a fresh
/// install the folder does not exist yet — and the switcher, which marks any entry whose path is
/// not a directory, called the *default* workspace "missing" on a brand-new machine.
///
/// Only on synthesis, and only for the default entry. Recreating a workspace someone deliberately
/// deleted would be worse than reporting it gone, which is the state the marker is actually for.
fn ensure_default_workspace(file: &AppFile) {
    let Some(entry) = file.workspaces.iter().find(|entry| entry.id == DEFAULT_ID) else {
        return;
    };
    if entry.path.exists() {
        return;
    }
    if let Err(error) = std::fs::create_dir_all(&entry.path) {
        eprintln!("[zuno] could not create {}: {error}", entry.path.display());
    }
}

pub fn install(cx: &mut App) {
    let dir = config_dir();
    let file = match dir.as_deref() {
        Some(dir) => {
            adopt_legacy_session(dir);
            read(dir)
        }
        None => None,
    };

    let file = file.unwrap_or_else(|| {
        let fresh = AppState::fresh(crate::collections::default_path());
        ensure_default_workspace(&fresh);
        fresh
    });

    cx.set_global(AppState { dir, file });
    resolve(cx);
}

/// Point the app state at a specific directory, or disable persistence with `None`.
///
/// Test-only. Invariant 6: the suite drives sends and saves, and without this it would write
/// into the developer's own config.
#[cfg(test)]
pub fn install_at(cx: &mut App, dir: Option<PathBuf>, workspaces: Vec<WorkspaceEntry>) {
    let file = AppFile {
        version: CURRENT_VERSION,
        theme: Appearance::Dark,
        last: workspaces.first().map(|entry| entry.id.clone()),
        workspaces,
    };
    cx.set_global(AppState { dir, file });
    resolve(cx);
}

/// Install the active workspace's collection root and session file.
///
/// The registry is the source of truth; these two globals are the *resolved answer*, which is
/// what keeps the test harness's `install_at` seams working unchanged — and what makes switching
/// a matter of re-resolving rather than threading a root through every call site.
fn resolve(cx: &mut App) {
    let state = cx.global::<AppState>();
    let root = state.active().map(|entry| entry.path.clone());
    let session = state
        .dir
        .as_deref()
        .zip(state.active())
        .map(|(dir, entry)| session_path(dir, &entry.id));

    crate::collections::install_at(cx, root);
    crate::session::install_at(cx, session);
}

pub fn theme(cx: &App) -> Appearance {
    cx.try_global::<AppState>()
        .map(|state| state.file.theme)
        .unwrap_or(Appearance::Dark)
}

/// Remember the theme across restarts. It was hardcoded to `Dark` at every startup, so
/// `Ctrl+Shift+T` never survived one.
pub fn set_theme(cx: &mut App, theme: Appearance) {
    // No `try_global_mut` in 0.2.2 — check for presence, then take the mutable borrow.
    if cx.try_global::<AppState>().is_none() {
        return;
    }
    let state = cx.global_mut::<AppState>();
    if state.file.theme == theme {
        return;
    }
    state.file.theme = theme;
    save(cx);
}

pub fn save(cx: &App) {
    let Some(state) = cx.try_global::<AppState>() else {
        return;
    };
    let Some(dir) = state.dir.as_deref() else {
        return;
    };

    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("[zuno] could not create {}: {error}", dir.display());
        return;
    }
    match serde_json::to_vec_pretty(&state.file) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            if let Err(error) = std::fs::write(dir.join("app.json"), bytes) {
                eprintln!("[zuno] could not write app.json: {error}");
            }
        }
        Err(error) => eprintln!("[zuno] could not serialize app.json: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zuno-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn the_config_dir_sits_under_xdg_config() {
        // Reads the real environment, so assert on shape rather than an exact value.
        if let Some(dir) = config_dir() {
            assert!(dir.ends_with("zuno"), "{dir:?}");
            assert!(dir.is_absolute(), "{dir:?}");
        }
    }

    #[test]
    fn a_session_path_is_derived_from_the_id() {
        let dir = PathBuf::from("/cfg/zuno");
        assert_eq!(
            session_path(&dir, DEFAULT_ID),
            PathBuf::from("/cfg/zuno/sessions/default.json")
        );
        assert_eq!(
            session_path(&dir, "a1b2"),
            PathBuf::from("/cfg/zuno/sessions/a1b2.json")
        );
    }

    #[test]
    fn a_last_that_names_a_missing_workspace_falls_back_to_the_first() {
        // A startup path: `last` can name a workspace that has since been forgotten, and Zuno
        // still has to open. Returning `None` there would leave the window with no collection
        // at all rather than with the one remaining workspace.
        let state = AppState {
            dir: None,
            file: AppFile {
                version: CURRENT_VERSION,
                theme: Appearance::Dark,
                last: Some("gone".into()),
                workspaces: vec![WorkspaceEntry {
                    id: DEFAULT_ID.into(),
                    path: "/w/default".into(),
                }],
            },
        };
        assert_eq!(state.active().map(|e| e.id.as_str()), Some(DEFAULT_ID));
    }

    #[test]
    fn an_empty_registry_has_no_active_workspace() {
        let state = AppState {
            dir: None,
            file: AppFile {
                version: CURRENT_VERSION,
                theme: Appearance::Dark,
                last: None,
                workspaces: Vec::new(),
            },
        };
        assert!(state.active().is_none());
    }

    #[test]
    fn the_file_round_trips_through_json() {
        // Same guard `session.rs` keeps: a field that stops surviving serialization is a silent
        // reset of someone's registry.
        let file = AppFile {
            version: CURRENT_VERSION,
            theme: Appearance::Light,
            last: Some(DEFAULT_ID.into()),
            workspaces: vec![WorkspaceEntry {
                id: DEFAULT_ID.into(),
                path: "/w/default".into(),
            }],
        };
        let json = serde_json::to_vec_pretty(&file).expect("serialize");
        let back: AppFile = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(file, back);
    }

    #[test]
    fn ids_derive_from_the_directory_and_never_collide() {
        let mut existing: Vec<WorkspaceEntry> = Vec::new();
        let id = allocate_id(&existing, Path::new("/code/Payments API"));
        assert_eq!(id, "payments-api", "the id should read like the folder");

        existing.push(WorkspaceEntry { id: id.clone(), path: "/code/Payments API".into() });
        let second = allocate_id(&existing, Path::new("/elsewhere/payments-api"));
        assert_eq!(second, "payments-api-2", "a taken id takes a suffix, like `allocate`");

        // `default` is the built-in workspace's id; a folder called "default" must not claim it
        // and silently take over the original's session file.
        let reserved = allocate_id(&existing, Path::new("/code/default"));
        assert_ne!(reserved, DEFAULT_ID);
    }

    #[test]
    fn the_default_workspace_is_created_when_it_is_registered() {
        // On a fresh install nothing had made the collection directory — the first `Ctrl+S`
        // did — so the switcher marked the built-in workspace "missing" on a new machine.
        let dir = scratch("ensure-default");
        let default = dir.join("collections");
        let other = dir.join("gone");

        let file = AppFile {
            version: CURRENT_VERSION,
            theme: Appearance::Dark,
            last: Some(DEFAULT_ID.into()),
            workspaces: vec![
                WorkspaceEntry { id: DEFAULT_ID.into(), path: default.clone() },
                WorkspaceEntry { id: "other".into(), path: other.clone() },
            ],
        };
        ensure_default_workspace(&file);

        assert!(default.is_dir(), "the default workspace's directory must exist");
        assert!(
            !other.exists(),
            "only the default is created — remaking a workspace someone deleted would be worse \
             than reporting it gone"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_legacy_session_is_adopted_once_and_never_overwrites() {
        let dir = scratch("adopt");
        std::fs::write(dir.join("session.json"), b"legacy").expect("write");

        adopt_legacy_session(&dir);
        assert_eq!(
            std::fs::read(session_path(&dir, DEFAULT_ID)).expect("adopted"),
            b"legacy",
            "the pre-registry session must become the default workspace's"
        );
        assert!(
            dir.join("session.json").exists(),
            "copied, not moved — a downgrade still needs to find its session"
        );

        // Second run, with the new file already written by this build. Overwriting here would
        // silently replace the real session with a stale one on every launch.
        std::fs::write(session_path(&dir, DEFAULT_ID), b"current").expect("write");
        adopt_legacy_session(&dir);
        assert_eq!(
            std::fs::read(session_path(&dir, DEFAULT_ID)).expect("kept"),
            b"current"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
