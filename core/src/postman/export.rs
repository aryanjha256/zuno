//! Writing a Postman collection — the other direction from `postman::parse`.
//!
//! **Why this and not OpenAPI.** OpenAPI describes an *API*; a Zuno collection holds *example
//! requests*. Emitting a spec would mean inventing parameter types, body schemas and response
//! definitions that a collection simply does not contain, producing a document that looks
//! authoritative and is guessed. Postman's format holds the same thing Zuno does — concrete
//! requests in folders — so the mapping is real rather than inferred.
//!
//! **It closes an asymmetry that decides adoption.** Zuno reads curl, OpenAPI and Postman, and
//! could write only a single curl command: easy to get in, impossible to get out. ROADMAP
//! records that Postman *import* jumped the queue because migration friction decides who tries
//! the app; the same argument applies to the exit.
//!
//! What cannot survive the trip is **reported, never dropped silently** — the same `skipped`
//! channel the importer uses, for the same reason.

use serde_json::{Map, Value, json};

use crate::collection::Entry;
use crate::request::{Body, MultipartValue, RequestKind, RequestSpec};

/// A collection ready to write, plus everything that did not fit.
pub struct Export {
    pub json: String,
    /// One line per thing Postman has no home for, named rather than counted.
    pub skipped: Vec<String>,
}

/// The schema every Postman importer keys off. `info._postman_id` is deliberately absent: it is
/// optional, Postman generates one on import, and the alternative is a uuid dependency for a
/// field nothing reads.
const SCHEMA: &str = "https://schema.getpostman.com/json/collection/v2.1.0/collection.json";

/// Build a Postman v2.1 collection from everything under one collection root.
///
/// `entries` come from `collection::scan` of the folder being exported, so exporting a subfolder
/// is the same code with a different root — which is what lets any folder, at any depth, be the
/// thing you export.
pub fn to_collection(name: &str, entries: &[Entry]) -> Export {
    let mut skipped = Vec::new();
    let mut root = Folder::default();

    for entry in entries {
        // `relative` is the path under the export root: `billing/invoices.json`. Its directories
        // become nested Postman folders, which is the one structural thing both formats share.
        let mut parts: Vec<&str> = entry.relative.split('/').collect();
        let file = parts.pop().unwrap_or_default();
        let label = file.strip_suffix(".json").unwrap_or(file);

        let mut folder = &mut root;
        for part in parts {
            folder = folder.child(part);
        }
        folder.items.push(item_for(label, &entry.spec, &mut skipped));
    }

    let collection = json!({
        "info": { "name": name, "schema": SCHEMA },
        "item": root.into_items(),
    });

    Export {
        json: serde_json::to_string_pretty(&collection).unwrap_or_default(),
        skipped,
    }
}

/// A directory being assembled into Postman's nested `item` arrays.
///
/// Ordered rather than a map: `scan` returns entries sorted by relative path, and a collection
/// that reorders itself on export would make a diff of two exports unreadable.
#[derive(Default)]
struct Folder {
    names: Vec<String>,
    folders: Vec<Folder>,
    items: Vec<Value>,
}

impl Folder {
    fn child(&mut self, name: &str) -> &mut Folder {
        if let Some(at) = self.names.iter().position(|existing| existing == name) {
            return &mut self.folders[at];
        }
        self.names.push(name.to_string());
        self.folders.push(Folder::default());
        self.folders.last_mut().expect("just pushed")
    }

    fn into_items(self) -> Vec<Value> {
        // Folders first, then requests — the order Postman's own exports use, and the order the
        // collection panel draws.
        let mut out: Vec<Value> = self
            .names
            .into_iter()
            .zip(self.folders)
            .map(|(name, folder)| json!({ "name": name, "item": folder.into_items() }))
            .collect();
        out.extend(self.items);
        out
    }
}

fn item_for(label: &str, spec: &RequestSpec, skipped: &mut Vec<String>) -> Value {
    report_unmappable(label, spec, skipped);

    let mut request = Map::new();
    request.insert(
        "method".to_string(),
        // Postman has no notion of a request without a verb; every kind Zuno can export today
        // rides HTTP and has one.
        spec.method().map_or_else(|| "GET".into(), |method| json!(method.as_str())),
    );
    request.insert("header".to_string(), headers_for(spec));
    request.insert("url".to_string(), url_for(spec));

    if let Some(body) = body_for(spec) {
        request.insert("body".to_string(), body);
    }

    json!({ "name": label, "request": Value::Object(request) })
}

/// Everything Postman has no field for.
///
/// **Named per request, not counted.** "3 settings were dropped" makes you go and find which;
/// the importer's `skipped` channel exists for the same reason and reads the same way.
fn report_unmappable(label: &str, spec: &RequestSpec, skipped: &mut Vec<String>) {
    // **Announced, not silently degraded.** Postman keeps sockets in a separate "WebSocket
    // Request" item that the v2.1 collection schema does not describe, so there is nothing
    // honest to write — and what `body_for` produces instead is an ordinary GET to the same
    // URL, which *runs* and does the wrong thing. That is worse than an omission, so it has to
    // be said out loud. This sentence existed as a comment claiming it was handled before it
    // was true, which is the failure this file's own Lessons entry is about.
    if spec.kind.is_session() {
        skipped.push(format!(
            "{label}: exported as a plain GET — Postman's collection format has no WebSocket              request, so the subprotocols and saved messages are not carried. Use a Zuno bundle              to move it between Zunos"
        ));
    }
    if spec.settings != crate::request::RequestSettings::default() {
        skipped.push(format!(
            "{label}: per-request settings (timeout, TLS, redirects, cookies) — Postman has no \
             per-request equivalent"
        ));
    }
    if !spec.captures.is_empty() {
        skipped.push(format!("{label}: {} capture rule(s)", spec.captures.len()));
    }
    if !spec.assertions.is_empty() || spec.expect_status.is_some() {
        skipped.push(format!("{label}: assertions and the expected status"));
    }
}

fn headers_for(spec: &RequestSpec) -> Value {
    Value::Array(
        spec.headers
            .iter()
            .map(|header| {
                json!({
                    "key": header.name,
                    "value": header.value,
                    // Postman keeps a muted row rather than deleting it, exactly as Zuno does —
                    // so the toggle survives the trip in both directions.
                    "disabled": !header.enabled,
                })
            })
            .collect(),
    )
}

/// The URL as both a raw string and a parameter array.
///
/// Both, deliberately: `raw` is what Postman displays and what a reader recognises, and the
/// array is the only place a *disabled* parameter can live. `postman::parse` prefers the array
/// when present, so a Zuno → Postman → Zuno round trip keeps muted rows.
fn url_for(spec: &RequestSpec) -> Value {
    let params: Vec<_> = spec
        .http()
        .map(|http| http.query.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|param| !param.name.trim().is_empty())
        .collect();

    let enabled: Vec<String> = params
        .iter()
        .filter(|param| param.enabled)
        .map(|param| format!("{}={}", param.name.trim(), param.value))
        .collect();

    let mut raw = spec.url.trim().to_string();
    if !enabled.is_empty() {
        raw.push(if raw.contains('?') { '&' } else { '?' });
        raw.push_str(&enabled.join("&"));
    }

    if params.is_empty() {
        return json!({ "raw": raw });
    }

    json!({
        "raw": raw,
        "query": params
            .iter()
            .map(|param| json!({
                "key": param.name.trim(),
                "value": param.value,
                "disabled": !param.enabled,
            }))
            .collect::<Vec<_>>(),
    })
}

/// The body, or `None` when there is nothing to send.
///
/// Exhaustive on the kind and on `Body` with no catch-all, for `Resolver::apply`'s reason: a new
/// kind or variant must fail the build until someone decides how Postman expresses it — silently
/// exporting nothing is how a collection arrives on the other side with its bodies missing.
fn body_for(spec: &RequestSpec) -> Option<Value> {
    let http = match &spec.kind {
        RequestKind::Http(http) => http,
        // Postman keeps sockets in a separate "WebSocket Request" item that the v2.1
        // collection schema does not describe — there is no body to write here, and inventing
        // an HTTP GET in its place would export something that runs and does the wrong thing.
        // `report_unmappable` is where this gets announced.
        RequestKind::WebSocket(_) => return None,
        // Postman models GraphQL as a body *mode* on an HTTP request, which is exactly what the
        // importer reads back into a `GraphQl` kind. `variables` is a string of JSON on both
        // sides, so it travels verbatim.
        RequestKind::GraphQl(graphql) => {
            if graphql.query.trim().is_empty() {
                return None;
            }
            return Some(json!({
                "mode": "graphql",
                "graphql": {
                    "query": graphql.query,
                    "variables": graphql.variables,
                },
            }));
        }
    };

    match &http.body {
        Body::Empty => None,
        Body::Raw { text, kind } => Some(json!({
            "mode": "raw",
            "raw": text,
            "options": { "raw": { "language": language_for(*kind) } },
        })),
        Body::Form(fields) => Some(json!({
            "mode": "urlencoded",
            "urlencoded": fields
                .iter()
                .map(|field| json!({
                    "key": field.name,
                    "value": field.value,
                    "disabled": !field.enabled,
                }))
                .collect::<Vec<_>>(),
        })),
        Body::Multipart(fields) => Some(json!({
            "mode": "formdata",
            "formdata": fields
                .iter()
                .map(|field| match &field.value {
                    // A file part carries a path, not contents — on both sides. It only resolves
                    // on a machine that has the file, which is Postman's behaviour too.
                    MultipartValue::File(path) => json!({
                        "key": field.name,
                        "type": "file",
                        "src": path.display().to_string(),
                        "disabled": !field.enabled,
                    }),
                    MultipartValue::Text(text) => json!({
                        "key": field.name,
                        "type": "text",
                        "value": text,
                        "disabled": !field.enabled,
                    }),
                })
                .collect::<Vec<_>>(),
        })),
        Body::Binary(path) => Some(json!({
            "mode": "file",
            "file": { "src": path.display().to_string() },
        })),
    }
}

fn language_for(kind: crate::request::RawKind) -> &'static str {
    use crate::request::RawKind;
    match kind {
        RawKind::Json => "json",
        RawKind::Xml => "xml",
        RawKind::Html => "html",
        RawKind::Text => "text",
    }
}

#[cfg(test)]
mod websocket_tests {
    use super::*;
    use crate::request::{RequestKind, RequestSpec, WebSocketRequest};

    /// **A socket exports as a plain GET, and the report has to say so.**
    ///
    /// Postman's v2.1 collection schema has no WebSocket item, so `body_for` produces nothing —
    /// which leaves an ordinary GET to the same URL. That *runs*, and does the wrong thing,
    /// which is worse than an omission. Silence here was shipped once behind a comment claiming
    /// `report_unmappable` handled it.
    #[test]
    fn a_socket_is_reported_rather_than_quietly_degraded() {
        let mut spec = RequestSpec::default();
        spec.name = "prices".to_string();
        spec.url = "wss://api.test/ws".to_string();
        spec.kind = RequestKind::WebSocket(WebSocketRequest {
            subprotocols: vec!["graphql-transport-ws".to_string()],
            messages: Vec::new(),
        });

        let mut skipped = Vec::new();
        report_unmappable("prices", &spec, &mut skipped);

        assert!(
            skipped.iter().any(|line| line.contains("WebSocket")),
            "the export has to name what it could not carry, got {skipped:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::{scan, write};
    use crate::request::{FormField, GraphQlRequest, Header, Method, QueryParam};

    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zuno-export-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        root
    }

    /// **Export then re-import, and nothing the two formats share may change.**
    ///
    /// We own both directions, so this is the test that catches drift the way `curl.rs`'s
    /// round trip does: a field written here that `parse` does not read, or vice versa, shows
    /// up as a difference rather than as a silent loss on someone else's machine.
    #[test]
    fn a_collection_survives_export_and_reimport() {
        let root = root();

        let mut http = RequestSpec::default();
        http.url = "https://api.test/users".into();
        http.headers = vec![
            Header::new("Accept", "application/json"),
            Header::disabled("X-Debug", "1"),
        ];
        let inner = http.http_mut().expect("HTTP");
        inner.method = Method::Post;
        inner.query = vec![
            QueryParam::new("limit", "10"),
            QueryParam { enabled: false, name: "page".into(), value: "2".into() },
        ];
        inner.body = Body::Form(vec![FormField {
            enabled: true,
            name: "grant_type".into(),
            value: "client_credentials".into(),
        }]);
        std::fs::create_dir_all(root.join("billing")).expect("subdir");
        write(&root.join("billing/create-user.json"), &http).expect("write");

        let mut gql = RequestSpec::default();
        gql.url = "https://api.test/graphql".into();
        gql.kind = RequestKind::GraphQl(GraphQlRequest {
            method: Method::Post,
            query: "query Me { me { id } }".into(),
            variables: r#"{"n": 1}"#.into(),
            operation: None,
        });
        write(&root.join("me.json"), &gql).expect("write");

        let entries = scan(&root);
        let export = to_collection("my-api", &entries);

        let value: serde_json::Value =
            serde_json::from_str(&export.json).expect("the export must be valid JSON");
        let back = crate::postman::parse(&value).expect("re-import");

        // The folder survived as a folder.
        let created = back
            .requests
            .iter()
            .find(|r| r.spec.url.contains("/users"))
            .expect("the HTTP request");
        assert_eq!(created.folders, vec!["billing".to_string()]);

        let http_back = created.spec.http().expect("HTTP");
        assert_eq!(http_back.method, Method::Post);
        assert_eq!(
            http_back.body,
            Body::Form(vec![FormField {
                enabled: true,
                name: "grant_type".into(),
                value: "client_credentials".into(),
            }])
        );
        // A muted row is a muted row on both sides — the toggle is half of how people debug.
        assert!(created.spec.headers.iter().any(|h| h.name == "X-Debug" && !h.enabled));
        assert!(http_back.query.iter().any(|p| p.name == "page" && !p.enabled));

        // GraphQL is a body *mode* in Postman and a *kind* in Zuno; it has to come back a kind.
        let me = back
            .requests
            .iter()
            .find(|r| r.spec.url.contains("/graphql"))
            .expect("the GraphQL request");
        let graphql = me.spec.graphql().expect("must re-import as a GraphQL request");
        assert_eq!(graphql.query, "query Me { me { id } }");
        assert_eq!(graphql.variables, r#"{"n": 1}"#);

        std::fs::remove_dir_all(&root).ok();
    }

    /// What Postman has no home for is named, not dropped in silence.
    #[test]
    fn what_cannot_be_exported_is_reported() {
        let root = root();

        let mut spec = RequestSpec::default();
        spec.url = "https://api.test/x".into();
        spec.settings.verify_tls = false;
        spec.expect_status = Some(200);
        spec.captures = vec![crate::capture::Capture {
            path: "$.token".into(),
            name: "token".into(),
            ..Default::default()
        }];
        write(&root.join("x.json"), &spec).expect("write");

        let entries = scan(&root);
        let export = to_collection("x", &entries);

        let report = export.skipped.join("\n");
        assert!(report.contains("settings"), "settings must be reported: {report}");
        assert!(report.contains("capture"), "captures must be reported: {report}");
        assert!(report.contains("assertions"), "assertions must be reported: {report}");

        std::fs::remove_dir_all(&root).ok();
    }
}
