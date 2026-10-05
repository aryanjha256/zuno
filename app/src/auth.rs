//! The resolver the screen shows values with — the derived `Authorization` header, and every
//! `{{variable}}`'s colour and hover in the inputs and editors.
//!
//! **Cached, and refreshed at the points it can change**, for `Workspace::globals_active`'s
//! reason: resolving means opening and parsing the environment files, which invariant 3 forbids
//! on the UI thread, and the headers tab asks every frame. The points are a workspace loading or
//! switching, the environment changing or being edited, an import writing one, and a capture
//! writing into one. A refresh that lands after a newer one started is dropped, so two in flight
//! cannot leave the older answer on screen.

use std::path::{Path, PathBuf};

use gpui::{App, Global};
use zuno_core::{Resolver, environment};

/// Which auth a request uses — the choice on the Auth tab, apart from the fields it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthKind {
    #[default]
    None,
    Basic,
    Bearer,
}

impl AuthKind {
    pub const ALL: [AuthKind; 3] = [AuthKind::None, AuthKind::Basic, AuthKind::Bearer];

    pub fn label(self) -> &'static str {
        match self {
            AuthKind::None => "No auth",
            AuthKind::Basic => "Basic",
            AuthKind::Bearer => "Bearer",
        }
    }

    pub fn of(auth: &zuno_core::Auth) -> Self {
        match auth {
            zuno_core::Auth::None => AuthKind::None,
            zuno_core::Auth::Basic { .. } => AuthKind::Basic,
            zuno_core::Auth::Bearer { .. } => AuthKind::Bearer,
        }
    }
}

/// Whether a secret field holds a literal rather than a `{{placeholder}}` — the case that puts
/// a password into a committed request file, and so the case the Auth tab warns about.
pub fn is_literal_secret(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && !(text.starts_with("{{") && text.ends_with("}}"))
}

#[derive(Default)]
pub struct ShownResolver {
    pub resolver: Resolver,
    /// What the last refresh read, so a capture — which knows neither — can ask for the same.
    root: Option<PathBuf>,
    environment: Option<String>,
    generation: u64,
}

impl Global for ShownResolver {}

/// Globals underneath, the named environment on top — what a send resolves with.
pub fn load(root: &Path, environment: Option<&str>) -> Resolver {
    let globals = environment::load(root, environment::GLOBALS).ok();
    let active = environment.and_then(|name| match environment::load(root, name) {
        Ok(env) => Some(env),
        Err(error) => {
            eprintln!("[zuno] {error}");
            None
        }
    });
    Resolver::new(globals.as_ref(), active.as_ref())
}

/// Re-read the environment `environment` names, off the UI thread.
pub fn refresh(environment: Option<String>, cx: &mut App) {
    let root = crate::collections::root(cx).map(Path::to_path_buf);
    let shown = cx.default_global::<ShownResolver>();
    shown.generation += 1;
    shown.root = root.clone();
    shown.environment = environment.clone();
    let generation = shown.generation;

    let Some(root) = root else {
        cx.global_mut::<ShownResolver>().resolver = Resolver::default();
        return;
    };
    let work = cx
        .background_executor()
        .spawn(async move { load(&root, environment.as_deref()) });
    cx.spawn(async move |cx| {
        let resolver = work.await;
        let _ = cx.update(|cx| {
            let shown = cx.global_mut::<ShownResolver>();
            if shown.generation == generation {
                shown.resolver = resolver;
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

/// Install a resolver directly — a test has no environment files to load one from.
#[cfg(test)]
pub fn set_for_test(resolver: Resolver, cx: &mut App) {
    let shown = cx.default_global::<ShownResolver>();
    shown.generation += 1;
    shown.resolver = resolver;
}

/// Re-read whatever the last refresh read — for a capture, which changes an environment's
/// values without knowing which one is selected.
pub fn reload(cx: &mut App) {
    let Some(shown) = cx.try_global::<ShownResolver>() else {
        return;
    };
    let environment = shown.environment.clone();
    refresh(environment, cx);
}

/// What a `{{name}}` resolves to on screen, and where the value comes from.
pub struct VariableInfo {
    pub value: Option<String>,
    /// `dev`, `dev.local`, `globals` — or, when undefined, where it was looked for.
    pub origin: String,
}

/// Whether `name` resolves right now. `None` before any resolver has loaded — a test harness, or
/// the first frame — so nothing is painted as broken that has not been checked.
pub fn is_defined(name: &str, cx: &App) -> Option<bool> {
    cx.try_global::<ShownResolver>()
        .map(|shown| shown.resolver.get(name).is_some())
}

pub fn describe(name: &str, cx: &App) -> VariableInfo {
    let Some(shown) = cx.try_global::<ShownResolver>() else {
        return VariableInfo {
            value: None,
            origin: "no environment loaded".to_string(),
        };
    };
    let layer = if shown.resolver.from_globals(name) {
        "globals".to_string()
    } else {
        shown.environment.clone().unwrap_or_else(|| "globals".to_string())
    };
    match shown.resolver.get(name) {
        Some(value) => VariableInfo {
            value: Some(value.to_string()),
            origin: if shown.resolver.is_secret(name) {
                format!("{layer}.local")
            } else {
                layer
            },
        },
        None => VariableInfo {
            value: None,
            origin: match &shown.environment {
                Some(env) => format!("not defined in {env} or globals"),
                None => "not defined — no environment selected".to_string(),
            },
        },
    }
}

/// The header `auth` produces, resolved the way a send resolves it.
pub fn shown_header(spec_auth: &zuno_core::Auth, cx: &App) -> Option<String> {
    match cx.try_global::<ShownResolver>() {
        Some(shown) => shown.resolver.resolve_auth(spec_auth).header_value(),
        None => spec_auth.header_value(),
    }
}
