//! Environments and variable substitution.
//!
//! Two layers, resolved environment-first: a `globals` set that's always active, and one
//! selected environment that overrides it. Request-level variables were considered and left
//! out — they'd add an editable table per request for the layer least likely to be used.
//!
//! **Where they live.** A reserved `environments/` directory inside the collection, so an
//! environment travels with the requests it describes and is reviewable in a pull request —
//! the same argument that made collections one-file-per-request. `collection::scan` skips
//! the directory by name; without that, every environment would be reported as an
//! unparseable request.
//!
//! **Secrets are a file split, not a flag.** `dev.json` is committed; `dev.local.json` sits
//! beside it, is gitignored, and overrides it. That split *is* the marking — anything from
//! the `.local` file is treated as secret and masked on screen, so there's no per-variable
//! flag to set and forget. The collection format exists to be committed, so a design where
//! a token can end up in the committed file is a design that leaks tokens.
//!
//! **Substitution replaces only known names, in a single pass.** Two consequences, both
//! deliberate:
//!
//! - An unknown `{{foo}}` is left *exactly* as written. In a URL or a header that then trips
//!   `EngineError::UnresolvedVariable` at the send boundary, which is the error that already
//!   existed. In a body it simply passes through, because `{{` occurs legitimately in JSON
//!   and other templating languages — `build.rs` deliberately doesn't scan bodies for the
//!   same reason.
//! - No recursion: a value containing `{{other}}` is not expanded again. It keeps
//!   substitution predictable, makes cycles impossible by construction rather than by cycle
//!   detection, and a variable that expands to a variable is a thing nobody has asked for.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use crate::{Body, FormField, MultipartField, MultipartValue, RequestSpec};

/// The reserved directory name inside a collection. Skipped by `collection::scan`.
pub const DIRECTORY: &str = "environments";

/// The always-active layer, by filename.
pub const GLOBALS: &str = "globals";

/// Marks the gitignored half of an environment: `dev.local.json`.
pub const LOCAL_SUFFIX: &str = ".local";

/// What Zuno adds to the collection's `.gitignore`.
const IGNORE_RULE: &str = "*.local.json";

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a valid environment: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// One environment's values, plus which of them came from the gitignored sidecar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environment {
    /// Filename stem: `dev`. Also what the switcher shows.
    pub name: String,
    pub values: BTreeMap<String, String>,
    /// Names defined in the `.local` file. Masked on screen; never written to the
    /// committed file.
    pub secret: BTreeSet<String>,
}

impl Environment {
    /// Whether a value should be hidden on screen.
    pub fn is_secret(&self, name: &str) -> bool {
        self.secret.contains(name)
    }
}

/// Resolves `{{name}}` against one environment plus the globals.
///
/// Holds owned maps rather than borrowing the environments, because the caller assembles
/// this per send and the maps are a handful of short strings.
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    globals: BTreeMap<String, String>,
    active: BTreeMap<String, String>,
    /// Names that came from a gitignored `.local` sidecar, in either layer.
    ///
    /// Carried but never consulted on the send path — a secret is a secret from *files*, not from
    /// the server. It exists for `without_secrets`.
    secret: BTreeSet<String>,
}

impl Resolver {
    pub fn new(globals: Option<&Environment>, active: Option<&Environment>) -> Self {
        let mut secret = BTreeSet::new();
        for env in [globals, active].into_iter().flatten() {
            secret.extend(env.secret.iter().cloned());
        }

        Self {
            globals: globals.map(|env| env.values.clone()).unwrap_or_default(),
            active: active.map(|env| env.values.clone()).unwrap_or_default(),
            secret,
        }
    }

    /// A resolver that treats every secret as undefined.
    ///
    /// For curl export: a copied command should be runnable against your dev box without carrying
    /// a live credential into whatever you paste it into. `dev.json` values are substituted;
    /// `dev.local.json` values come out as `{{token}}` for the recipient to fill in.
    ///
    /// **Deliberately implemented by *removing* the values rather than by adding a redaction pass.**
    /// `resolve` already leaves an unknown placeholder verbatim — that rule exists so the send
    /// boundary can name it and so a JSON body's own braces survive — and "withheld" wants exactly
    /// that behaviour. One substitution path, no second set of rules to keep in step, and no way for
    /// a redacting mode to forget a field the way `apply` once forgot form bodies.
    ///
    /// Note this makes the result *un-sendable* when a secret appears in the URL or a header:
    /// `build.rs` rejects unresolved variables there. That's correct — this output is for a
    /// clipboard, never for the wire.
    pub fn without_secrets(&self) -> Self {
        let strip = |map: &BTreeMap<String, String>| -> BTreeMap<String, String> {
            map.iter()
                .filter(|(name, _)| !self.secret.contains(*name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        };

        Self {
            globals: strip(&self.globals),
            active: strip(&self.active),
            secret: self.secret.clone(),
        }
    }

    /// Whether `name` came from a gitignored sidecar.
    pub fn is_secret(&self, name: &str) -> bool {
        self.secret.contains(name)
    }

    /// The selected environment wins over globals — that's the whole point of selecting one.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.active
            .get(name)
            .or_else(|| self.globals.get(name))
            .map(String::as_str)
    }

    /// Substitute every `{{known}}` in `text`, leaving unknown placeholders untouched.
    ///
    /// Returns `Cow::Borrowed` when there is nothing to do, which is the common case for
    /// header names and most bodies.
    pub fn resolve<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if !text.contains("{{") {
            return std::borrow::Cow::Borrowed(text);
        }

        let mut out = String::with_capacity(text.len());
        let mut rest = text;

        while let Some(start) = rest.find("{{") {
            // The `}}` must be searched from after the `{{`, or `{{}}` would find its own
            // opener's tail.
            let Some(end) = rest[start + 2..].find("}}") else {
                // No closing braces at all: the remainder is literal.
                break;
            };
            let name = &rest[start + 2..start + 2 + end];
            let after = start + 2 + end + 2;

            out.push_str(&rest[..start]);
            match self.get(name.trim()) {
                Some(value) => out.push_str(value),
                // Unknown: emit the placeholder verbatim so the send-boundary check can
                // name it, and so a JSON body's own `{{...}}` survives untouched.
                None => out.push_str(&rest[start..after]),
            }
            rest = &rest[after..];
        }

        out.push_str(rest);
        std::borrow::Cow::Owned(out)
    }

    /// Which secret names this spec actually refers to, in first-seen order.
    ///
    /// For the curl export's status line: "copied, and `{{token}}` is left for you to fill in" is
    /// the difference between a command that looks carefully redacted and one that looks broken.
    ///
    /// Exhaustive over `Body` with no catch-all, the same rule `apply` follows — a new variant must
    /// fail the build until someone decides whether a variable can appear in it. A catch-all here
    /// would under-report rather than crash, which is the quieter and worse failure.
    pub fn withheld_in(&self, spec: &RequestSpec) -> Vec<String> {
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();

        let mut scan = |text: &str| {
            for name in placeholders(text) {
                if self.secret.contains(&name) && seen.insert(name.clone()) {
                    found.push(name);
                }
            }
        };

        // **Enabled rows only, and that is the whole correctness of this.** A first version scanned
        // every row, and the sample request ships a *disabled*
        // `Authorization: Bearer {{token}}` — so a fresh buffer announced that a secret had been
        // withheld from a command which never referenced one. This has to describe what was
        // exported, not what is merely typed on screen.
        scan(&spec.url);
        for param in spec.enabled_query() {
            scan(&param.name);
            scan(&param.value);
        }
        for header in spec.enabled_headers() {
            scan(&header.name);
            scan(&header.value);
        }
        match &spec.body {
            Body::Empty => {}
            Body::Raw { text, .. } => scan(text),
            Body::Form(fields) => {
                for field in fields.iter().filter(|field| field.enabled) {
                    scan(&field.name);
                    scan(&field.value);
                }
            }
            Body::Multipart(fields) => {
                for field in fields.iter().filter(|field| field.enabled) {
                    scan(&field.name);
                    if let MultipartValue::Text(text) = &field.value {
                        scan(text);
                    }
                }
            }
            // A path, never substituted — see `apply`.
            Body::Binary(_) => {}
        }

        found
    }

    /// A copy of `spec` with variables substituted, ready to send.
    ///
    /// The stored request keeps its `{{placeholders}}` — that's the point of having them —
    /// so this is deliberately a new value rather than an in-place edit.
    ///
    /// Covers the URL, query names and values, header names and values, and every body a
    /// variable can appear in.
    ///
    /// Query rows matter here: `build.rs` validates the URL and headers but *not* query
    /// rows, so before this a `{{var}}` in a query parameter reached the wire literally.
    /// Form and multipart fields had the same hole for the same reason, and it was worse:
    /// `build.rs` deliberately never scans bodies for `{{…}}` (`{{` is legal in JSON), so
    /// there was no error either — an unresolved `client_secret={{secret}}` was simply sent.
    pub fn apply(&self, spec: &RequestSpec) -> RequestSpec {
        let mut resolved = spec.clone();
        resolved.url = self.resolve(&spec.url).into_owned();

        for param in &mut resolved.query {
            let name = self.resolve(&param.name).into_owned();
            let value = self.resolve(&param.value).into_owned();
            param.name = name;
            param.value = value;
        }
        for header in &mut resolved.headers {
            let name = self.resolve(&header.name).into_owned();
            let value = self.resolve(&header.value).into_owned();
            header.name = name;
            header.value = value;
        }

        // Exhaustive with no catch-all, for the reason `RequestView::load` is: a new `Body`
        // variant must fail the build until someone decides whether a variable belongs in it.
        // A catch-all here is what left form and multipart silently unsubstituted once they
        // became authorable.
        resolved.body = match &spec.body {
            Body::Empty => Body::Empty,
            Body::Raw { text, kind } => Body::Raw {
                text: self.resolve(text).into_owned(),
                kind: *kind,
            },
            Body::Form(fields) => Body::Form(
                fields
                    .iter()
                    .map(|field| FormField {
                        enabled: field.enabled,
                        name: self.resolve(&field.name).into_owned(),
                        value: self.resolve(&field.value).into_owned(),
                    })
                    .collect(),
            ),
            Body::Multipart(fields) => Body::Multipart(
                fields
                    .iter()
                    .map(|field| MultipartField {
                        enabled: field.enabled,
                        name: self.resolve(&field.name).into_owned(),
                        value: match &field.value {
                            MultipartValue::Text(text) => {
                                MultipartValue::Text(self.resolve(text).into_owned())
                            }
                            // Left alone for the same reason as `Binary` below.
                            MultipartValue::File(path) => MultipartValue::File(path.clone()),
                        },
                    })
                    .collect(),
            ),
            // A path, not text: substituting into it would let a variable choose which file
            // gets uploaded.
            Body::Binary(path) => Body::Binary(path.clone()),
        };

        resolved
    }
}

/// Every `{{name}}` in `text`, trimmed, in order of appearance.
///
/// Shares `resolve`'s scanning rules on purpose — notably that `}}` is searched from *after* the
/// `{{`, so `{{}}` cannot find its own opener's tail — because a name this reports and `resolve`
/// misses (or vice versa) is a placeholder the UI describes wrongly.
fn placeholders(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else {
            break;
        };
        names.push(rest[start + 2..start + 2 + end].trim().to_string());
        rest = &rest[start + 2 + end + 2..];
    }

    names
}

/// The environments directory inside a collection.
pub fn directory(collection_root: &Path) -> PathBuf {
    collection_root.join(DIRECTORY)
}

/// Every environment in the collection, sorted by name, globals excluded.
///
/// Like `collection::scan`, a single unreadable file is skipped rather than failing the
/// whole listing — a half-edited environment must not hide the others.
pub fn scan(collection_root: &Path) -> Vec<Environment> {
    let dir = directory(collection_root);
    let Ok(listing) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut names: Vec<String> = listing
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let stem = name.strip_suffix(".json")?;
            // A `.local` file is not an environment of its own — it's the sidecar of one,
            // and listing it would offer "dev.local" as something you could select.
            if stem.ends_with(LOCAL_SUFFIX) || stem == GLOBALS {
                return None;
            }
            Some(stem.to_string())
        })
        .collect();
    names.sort();

    names
        .into_iter()
        .filter_map(|name| match load(collection_root, &name) {
            Ok(env) => Some(env),
            Err(error) => {
                eprintln!("[zuno] skipping environment: {error}");
                None
            }
        })
        .collect()
}

/// Read one environment, merging its `.local` sidecar over the committed file.
pub fn load(collection_root: &Path, name: &str) -> Result<Environment, EnvironmentError> {
    let dir = directory(collection_root);
    let mut values = read_map(&dir.join(format!("{name}.json")))?;

    let local = read_map(&dir.join(format!("{name}{LOCAL_SUFFIX}.json")))?;
    let secret = local.keys().cloned().collect();
    // The sidecar overrides, so a placeholder can sit in the committed file and the real
    // value can live only locally.
    values.extend(local);

    Ok(Environment {
        name: name.to_string(),
        values,
        secret,
    })
}

/// A missing file is an empty map, not an error: an environment may have only a committed
/// half, only a local half, or neither yet.
fn read_map(path: &Path) -> Result<BTreeMap<String, String>, EnvironmentError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => {
            return Err(EnvironmentError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    serde_json::from_slice(&bytes).map_err(|source| EnvironmentError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Make sure the collection's `.gitignore` excludes `*.local.json`.
///
/// Zuno writing into a file it doesn't own is a real intrusion, so it is narrow and
/// idempotent: one line, appended only if no existing line already says it, and the file is
/// created only when the collection root exists. The alternative — documenting the
/// convention and hoping — leaks tokens by default, which is worse than the intrusion.
///
/// Returns whether it changed anything, so the caller can say so rather than acting
/// silently on someone's repository.
pub fn ensure_gitignored(collection_root: &Path) -> Result<bool, EnvironmentError> {
    let path = collection_root.join(".gitignore");

    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(EnvironmentError::Read {
                path,
                source,
            });
        }
    };

    if existing.lines().any(|line| line.trim() == IGNORE_RULE) {
        return Ok(false);
    }

    let mut updated = existing;
    // Don't glue the rule onto an unterminated final line.
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("# Zuno: environment secrets stay out of version control.\n");
    updated.push_str(IGNORE_RULE);
    updated.push('\n');

    std::fs::write(&path, updated).map_err(|source| EnvironmentError::Write {
        path,
        source,
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Header, QueryParam, RawKind};

    fn env(name: &str, pairs: &[(&str, &str)]) -> Environment {
        Environment {
            name: name.to_string(),
            values: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            secret: BTreeSet::new(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zuno-env-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(directory(&dir)).expect("scratch");
        dir
    }

    fn write_env(root: &Path, file: &str, json: &str) {
        std::fs::write(directory(root).join(file), json).expect("write");
    }

    #[test]
    fn the_selected_environment_overrides_globals() {
        let globals = env("globals", &[("baseUrl", "https://prod.test"), ("v", "v1")]);
        let dev = env("dev", &[("baseUrl", "http://localhost:3000")]);
        let resolver = Resolver::new(Some(&globals), Some(&dev));

        assert_eq!(resolver.get("baseUrl"), Some("http://localhost:3000"));
        // Falls through to globals for anything the environment doesn't define.
        assert_eq!(resolver.get("v"), Some("v1"));
        assert_eq!(resolver.get("nope"), None);
    }

    #[test]
    fn substitution_replaces_known_names() {
        let resolver = Resolver::new(None, Some(&env("dev", &[("host", "example.test")])));
        assert_eq!(resolver.resolve("https://{{host}}/users"), "https://example.test/users");
        // Repeated, and adjacent.
        assert_eq!(resolver.resolve("{{host}}{{host}}"), "example.testexample.test");
        // Whitespace inside the braces is tolerated, since people type it.
        assert_eq!(resolver.resolve("{{ host }}"), "example.test");
    }

    #[test]
    fn an_unknown_placeholder_is_left_exactly_as_written() {
        // Load-bearing: it's what lets `EngineError::UnresolvedVariable` name the variable at
        // the send boundary, and what lets a JSON body keep its own `{{...}}`.
        let resolver = Resolver::new(None, Some(&env("dev", &[("known", "x")])));
        assert_eq!(resolver.resolve("{{unknown}}"), "{{unknown}}");
        assert_eq!(resolver.resolve("{{known}}/{{unknown}}"), "x/{{unknown}}");
    }

    #[test]
    fn substitution_does_not_recurse() {
        // A value containing a placeholder is not expanded again — which is also what makes
        // a cycle impossible rather than something to detect.
        let resolver = Resolver::new(None, Some(&env("dev", &[("a", "{{b}}"), ("b", "boom")])));
        assert_eq!(resolver.resolve("{{a}}"), "{{b}}");
    }

    #[test]
    fn malformed_placeholders_do_not_panic_or_eat_text() {
        let resolver = Resolver::new(None, Some(&env("dev", &[("a", "1")])));
        // Unclosed: the rest is literal.
        assert_eq!(resolver.resolve("{{a"), "{{a");
        assert_eq!(resolver.resolve("x {{a"), "x {{a");
        // Empty name is not a variable.
        assert_eq!(resolver.resolve("{{}}"), "{{}}");
        // Braces that aren't placeholders at all — the JSON case.
        assert_eq!(resolver.resolve("}}{{"), "}}{{");
        assert_eq!(resolver.resolve(r#"{"a": 1}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn nothing_to_substitute_borrows_rather_than_allocating() {
        let resolver = Resolver::new(None, None);
        assert!(matches!(
            resolver.resolve("no placeholders here"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn apply_covers_url_query_headers_and_raw_body() {
        let resolver = Resolver::new(
            None,
            Some(&env("dev", &[("host", "api.test"), ("tok", "abc"), ("q", "widgets")])),
        );

        let spec = RequestSpec {
            url: "https://{{host}}/v1".to_string(),
            // The gap this closes: `build.rs` validates the URL and headers but not query
            // rows, so a `{{var}}` here used to reach the wire literally.
            query: vec![QueryParam::new("search", "{{q}}")],
            headers: vec![Header::new("Authorization", "Bearer {{tok}}")],
            body: Body::Raw {
                text: r#"{"host":"{{host}}","keep":"{{unknown}}"}"#.to_string(),
                kind: RawKind::Json,
            },
            ..RequestSpec::default()
        };

        let sent = resolver.apply(&spec);
        assert_eq!(sent.url, "https://api.test/v1");
        assert_eq!(sent.query[0].value, "widgets");
        assert_eq!(sent.headers[0].value, "Bearer abc");
        assert_eq!(
            sent.body.as_text(),
            Some(r#"{"host":"api.test","keep":"{{unknown}}"}"#),
            "known names substituted, unknown ones left for the body to keep"
        );

        // The stored request must be untouched, or saving would bake the environment in.
        assert_eq!(spec.url, "https://{{host}}/v1");
    }

    #[test]
    fn apply_substitutes_form_field_values() {
        // The motivating case, and the one that was silently broken: a client-credentials
        // token request is a *form* body, so the secret from `dev.local.json` was sent as the
        // literal string `{{secret}}`. `build.rs` never scans bodies for `{{…}}`, so there was
        // no error either — just a request that failed at the server for no visible reason.
        let resolver = Resolver::new(
            None,
            Some(&env("dev", &[("id", "zuno-cli"), ("secret", "s3cret")])),
        );

        let spec = RequestSpec {
            body: Body::Form(vec![
                FormField {
                    enabled: true,
                    name: "grant_type".into(),
                    value: "client_credentials".into(),
                },
                FormField {
                    enabled: true,
                    name: "client_id".into(),
                    value: "{{id}}".into(),
                },
                FormField {
                    enabled: true,
                    name: "client_secret".into(),
                    value: "{{secret}}".into(),
                },
            ]),
            ..RequestSpec::default()
        };

        let Body::Form(fields) = resolver.apply(&spec).body else {
            panic!("expected a form body");
        };
        assert_eq!(fields[1].value, "zuno-cli");
        assert_eq!(fields[2].value, "s3cret");
        assert_eq!(fields[0].value, "client_credentials", "untouched values stay put");

        // The stored request keeps its placeholders, or saving would bake the secret into a
        // committed collection file — the exact leak the `.local` split exists to prevent.
        let Body::Form(stored) = &spec.body else { unreachable!() };
        assert_eq!(stored[2].value, "{{secret}}");
    }

    #[test]
    fn apply_substitutes_a_form_field_name() {
        // Names as well as values, matching how query and header rows are handled.
        let resolver = Resolver::new(None, Some(&env("dev", &[("key", "api_key")])));
        let spec = RequestSpec {
            body: Body::Form(vec![FormField {
                enabled: true,
                name: "{{key}}".into(),
                value: "x".into(),
            }]),
            ..RequestSpec::default()
        };

        let Body::Form(fields) = resolver.apply(&spec).body else {
            panic!("expected a form body");
        };
        assert_eq!(fields[0].name, "api_key");
    }

    #[test]
    fn apply_substitutes_multipart_text_parts_but_never_file_paths() {
        let resolver = Resolver::new(
            None,
            Some(&env("dev", &[("caption", "hello"), ("p", "/etc/passwd")])),
        );

        let spec = RequestSpec {
            body: Body::Multipart(vec![
                MultipartField {
                    enabled: true,
                    name: "caption".into(),
                    value: MultipartValue::Text("{{caption}}".into()),
                },
                MultipartField {
                    enabled: true,
                    name: "avatar".into(),
                    value: MultipartValue::File(PathBuf::from("{{p}}")),
                },
            ]),
            ..RequestSpec::default()
        };

        let Body::Multipart(fields) = resolver.apply(&spec).body else {
            panic!("expected a multipart body");
        };
        assert_eq!(fields[0].value, MultipartValue::Text("hello".to_string()));
        assert_eq!(
            fields[1].value,
            MultipartValue::File(PathBuf::from("{{p}}")),
            "a variable choosing which file gets uploaded is not a feature"
        );
    }

    #[test]
    fn a_binary_body_path_is_never_substituted() {
        // A variable choosing which file gets uploaded is not a feature.
        let resolver = Resolver::new(None, Some(&env("dev", &[("p", "/etc/passwd")])));
        let spec = RequestSpec {
            body: Body::Binary(PathBuf::from("{{p}}")),
            ..RequestSpec::default()
        };
        assert_eq!(resolver.apply(&spec).body, Body::Binary(PathBuf::from("{{p}}")));
    }

    #[test]
    fn a_local_sidecar_overrides_and_marks_secrets() {
        let root = scratch("secrets");
        write_env(&root, "dev.json", r#"{"baseUrl":"http://localhost","token":"replace-me"}"#);
        write_env(&root, "dev.local.json", r#"{"token":"real-secret"}"#);

        let dev = load(&root, "dev").expect("load");
        assert_eq!(dev.values.get("token").map(String::as_str), Some("real-secret"));
        assert_eq!(dev.values.get("baseUrl").map(String::as_str), Some("http://localhost"));
        assert!(dev.is_secret("token"), "sidecar values are secret");
        assert!(!dev.is_secret("baseUrl"), "committed values are not");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_environment_may_have_only_a_local_half() {
        let root = scratch("local-only");
        write_env(&root, "dev.local.json", r#"{"token":"x"}"#);

        let dev = load(&root, "dev").expect("load");
        assert_eq!(dev.values.len(), 1);
        assert!(dev.is_secret("token"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scanning_lists_environments_but_not_sidecars_or_globals() {
        let root = scratch("scan");
        write_env(&root, "dev.json", "{}");
        write_env(&root, "prod.json", "{}");
        write_env(&root, "dev.local.json", "{}");
        write_env(&root, "globals.json", "{}");
        write_env(&root, "notes.md", "");

        let names: Vec<String> = scan(&root).into_iter().map(|env| env.name).collect();
        assert_eq!(names, ["dev", "prod"], "sidecars and globals are not selectable");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn one_broken_environment_does_not_hide_the_others() {
        let root = scratch("broken");
        write_env(&root, "good.json", r#"{"a":"1"}"#);
        write_env(&root, "bad.json", "{ not json");

        let names: Vec<String> = scan(&root).into_iter().map(|env| env.name).collect();
        assert_eq!(names, ["good"]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_gitignore_rule_is_added_once_and_never_duplicated() {
        let root = scratch("gitignore");

        assert!(ensure_gitignored(&root).expect("write"), "should have written");
        let first = std::fs::read_to_string(root.join(".gitignore")).expect("read");
        assert!(first.contains(IGNORE_RULE));

        // Idempotent: running again must report no change and leave the file alone.
        assert!(!ensure_gitignored(&root).expect("write"), "should be a no-op");
        assert_eq!(std::fs::read_to_string(root.join(".gitignore")).expect("read"), first);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_existing_gitignore_is_appended_to_not_replaced() {
        let root = scratch("gitignore-existing");
        // No trailing newline, which is the case that would otherwise glue the rule onto
        // someone else's last line.
        std::fs::write(root.join(".gitignore"), "target/\n*.log").expect("write");

        assert!(ensure_gitignored(&root).expect("write"));
        let text = std::fs::read_to_string(root.join(".gitignore")).expect("read");
        assert!(text.contains("target/"), "existing rules must survive: {text:?}");
        assert!(text.contains("*.log"), "{text:?}");
        assert!(text.lines().any(|line| line == "*.log"), "not glued: {text:?}");
        assert!(text.contains(IGNORE_RULE), "{text:?}");

        std::fs::remove_dir_all(&root).ok();
    }
}
