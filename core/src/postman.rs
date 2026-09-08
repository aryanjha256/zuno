//! Turning a Postman collection export into requests.
//!
//! **A hand-written walk over `serde_json::Value`, for `openapi.rs`'s reason.** The v2 schema is
//! published and there are crates that model it, but the parts this reads — the item tree, a
//! request's method, URL, headers, body and auth — are the stable half, and everything genuinely
//! hard here (`raw` versus the split URL fields, auth stored as arrays of key/value rows,
//! scripts) is awkward in a way a typed model does not help with.
//!
//! **Permissive by construction.** An item Zuno cannot make sense of is named in
//! `Import::skipped` and never fatal. A collection is a document written for another tool;
//! refusing ninety requests because one uses a feature we don't read would break the feature
//! exactly where it is most useful.
//!
//! **Postman's variable syntax is already Zuno's.** `{{baseUrl}}` needs no rewriting anywhere in
//! this file — in a URL, a header, a body or an auth token — which is the single largest reason
//! this import is faithful rather than approximate.
//!
//! Deliberately absent: `event` scripts, which are JavaScript and are named in `skipped` rather
//! than dropped silently; collection-level `protocolProfileBehavior`, which is per-request
//! settings Zuno models differently; and request descriptions, for which `RequestSpec` has no
//! field — counted and reported once rather than per request.

use serde_json::{Map, Value};

use crate::import::{Import, Imported, Variable};
use crate::{
    Body, FormField, Header, Method, MultipartField, MultipartValue, QueryParam, RawKind,
    RequestId, RequestSpec,
};

/// How deep imported folders may nest.
///
/// One less than `collection::MAX_DEPTH`, because everything an import writes lands inside a
/// folder named for the collection and that spends the first level. Anything deeper is
/// *flattened* into the deepest folder that fits rather than written where `scan` would never
/// look: a request on disk that the tree cannot show is the worst outcome available.
const MAX_FOLDER_DEPTH: usize = crate::collection::MAX_DEPTH - 1;

#[derive(Debug, thiserror::Error)]
pub enum PostmanError {
    #[error("not a Postman collection — no \"item\" array")]
    NotPostman,
    #[error("Postman schema {0} is not supported — Zuno reads v2.0 and v2.1")]
    UnsupportedSchema(String),
    #[error("the collection has no requests in it")]
    NoItems,
}

/// Read a collection.
pub fn parse(root: &Value) -> Result<Import, PostmanError> {
    let items = root
        .get("item")
        .and_then(Value::as_array)
        .ok_or(PostmanError::NotPostman)?;

    let info = root.get("info");

    // `info.schema` is a URL:
    // `https://schema.getpostman.com/json/collection/v2.1.0/collection.json`. 2.0 and 2.1 read
    // identically for everything below — 2.1 added `protocolProfileBehavior` and structured
    // descriptions, neither of which this reads — so one check covers both, the same call
    // `openapi.rs` makes about 3.0 versus 3.1. An export with no `schema` at all is accepted:
    // several tools emit one, and the shape is what identifies it.
    if let Some(schema) = info.and_then(|info| info.get("schema")).and_then(Value::as_str)
        && !schema.contains("/v2.")
    {
        return Err(PostmanError::UnsupportedSchema(schema.to_string()));
    }

    let title = info
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut import = Import {
        title,
        ..Import::default()
    };
    let mut descriptions = 0usize;

    // The collection's own auth is the root of the inheritance chain, and its own scripts run
    // around every request in it.
    note_scripts(root, "the collection", &mut import.skipped);
    walk(items, &[], root.get("auth"), &mut import, &mut descriptions);

    if import.requests.is_empty() {
        return Err(PostmanError::NoItems);
    }

    for row in array(root.get("variable")) {
        let Some(name) = row.get("key").and_then(Value::as_str) else {
            continue;
        };
        // A disabled variable is not in effect in Postman, and an environment has no row to be
        // switched off, so importing it would turn something dormant back on.
        if name.is_empty() || flag(row, "disabled") {
            continue;
        }
        import.variables.push(Variable {
            name: name.to_string(),
            value: row.get("value").map(scalar).unwrap_or_default(),
            secret: row.get("type").and_then(Value::as_str) == Some("secret"),
        });
    }

    if descriptions > 0 {
        import.skipped.push(format!(
            "{descriptions} descriptions — Zuno has nowhere to keep them"
        ));
    }

    Ok(import)
}

/// Walk the item tree, carrying the folders above and the auth in force.
fn walk(
    items: &[Value],
    folders: &[String],
    auth: Option<&Value>,
    import: &mut Import,
    descriptions: &mut usize,
) {
    for item in items {
        let Some(item) = item.as_object() else {
            import.skipped.push("an item that is not an object".to_string());
            continue;
        };

        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if item.contains_key("description") {
            *descriptions += 1;
        }
        let auth = inherited(item, auth);

        // A folder is an item with children; a request is an item with a `request`. Both keys
        // present is not a shape Postman writes, and children are the more informative half.
        if let Some(children) = item.get("item").and_then(Value::as_array) {
            note_scripts(&Value::Object(item.clone()), &name, &mut import.skipped);

            let mut nested = folders.to_vec();
            if nested.len() >= MAX_FOLDER_DEPTH {
                import.skipped.push(format!(
                    "{name}: nested deeper than {MAX_FOLDER_DEPTH} folders, flattened into {}",
                    nested.join("/")
                ));
            } else if !name.is_empty() {
                nested.push(name);
            }
            walk(children, &nested, auth, import, descriptions);
            continue;
        }

        let Some(request) = item.get("request") else {
            import
                .skipped
                .push(format!("{name}: neither a folder nor a request"));
            continue;
        };

        note_scripts(&Value::Object(item.clone()), &name, &mut import.skipped);
        match request_from(&name, request, auth, &mut import.skipped) {
            Some(spec) => import.requests.push(Imported {
                folders: folders.to_vec(),
                spec,
            }),
            None => import
                .skipped
                .push(format!("{name}: its request is not readable")),
        }
    }
}

/// The auth in force inside `item`, given what was in force outside it.
///
/// `{"type":"inherit"}` is Postman spelling out the default, and has to keep the parent's rather
/// than being read as an auth type of its own.
fn inherited<'a>(item: &'a Map<String, Value>, parent: Option<&'a Value>) -> Option<&'a Value> {
    match item.get("auth") {
        Some(auth) if auth.get("type").and_then(Value::as_str) == Some("inherit") => parent,
        Some(auth) => Some(auth),
        None => parent,
    }
}

fn request_from(
    name: &str,
    request: &Value,
    auth: Option<&Value>,
    skipped: &mut Vec<String>,
) -> Option<RequestSpec> {
    // Collection files always store 0; a live handle is assigned when a buffer opens.
    let mut spec = RequestSpec {
        id: RequestId(0),
        name: name.to_string(),
        ..RequestSpec::default()
    };

    // The v2 shorthand is `"request": "https://api.test/ping"` — a bare URL, meaning a GET of
    // it. Normalised into the object form rather than handled separately, so everything below
    // (auth in particular, which a shorthand request still inherits) runs for both.
    let shorthand;
    let request = match request {
        Value::String(url) => {
            shorthand = Map::from_iter([("url".to_string(), Value::String(url.clone()))]);
            &shorthand
        }
        Value::Object(fields) => fields,
        _ => return None,
    };

    spec.method = request
        .get("method")
        .and_then(Value::as_str)
        .map(method_for)
        .unwrap_or(Method::Get);

    if let Some(url) = request.get("url") {
        let (url, query) = url_and_query(url, name, skipped);
        spec.url = url;
        spec.query = query;
    }

    spec.headers = headers_from(request.get("header"), name, skipped);

    if let Some(body) = request.get("body") {
        spec.body = body_from(body, name, skipped);
    }

    // **A request's own auth sits inside `request`, not on the item** — only a folder's sits on
    // the item, and reading it from there gave every request in a folder the collection's
    // credentials no matter what it declared. Both are consulted, request first.
    let auth = inherited(request, auth);

    // Applied after the request's own rows so an explicit `Authorization` header, which is what
    // the person actually wrote, is the one already present — and auth then does not add a
    // second. Postman resolves it the same way round.
    if let Some(auth) = auth.and_then(|auth| auth_effect(auth, name, skipped)) {
        match auth {
            AuthEffect::Header(header) => {
                if !spec
                    .headers
                    .iter()
                    .any(|existing| existing.name.eq_ignore_ascii_case(&header.name))
                {
                    spec.headers.push(header);
                }
            }
            AuthEffect::Query(param) => {
                if !spec.query.iter().any(|existing| existing.name == param.name) {
                    spec.query.push(param);
                }
            }
        }
    }

    Some(spec)
}

/// Postman allows any verb, and so does Zuno — `Method::Other` exists for exactly this, so
/// nothing here is ever a reason to skip a request.
fn method_for(verb: &str) -> Method {
    match verb.to_ascii_uppercase().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        "HEAD" => Method::Head,
        "OPTIONS" => Method::Options,
        other => Method::Other(other.to_string()),
    }
}

fn headers_from(header: Option<&Value>, name: &str, skipped: &mut Vec<String>) -> Vec<Header> {
    let mut out = Vec::new();
    match header {
        Some(Value::Array(rows)) => {
            for row in rows {
                let Some(key) = row.get("key").and_then(Value::as_str) else {
                    continue;
                };
                if key.is_empty() {
                    continue;
                }
                out.push(Header {
                    // Postman stores the negative: `disabled: true` is an unticked row.
                    enabled: !flag(row, "disabled"),
                    name: key.to_string(),
                    value: row.get("value").map(scalar).unwrap_or_default(),
                });
            }
        }
        // Older exports write the whole block as one string. Parsed rather than reported,
        // because it is six lines and the alternative is dropping every header on the request.
        Some(Value::String(text)) => {
            for line in text.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                out.push(Header::new(key.trim(), value.trim()));
            }
        }
        Some(other) if !other.is_null() => {
            skipped.push(format!("{name}: headers in an unreadable shape"));
        }
        _ => {}
    }
    out
}

/// The URL as text, with its query split into rows.
///
/// **The query is split off rather than left in the text.** `build.rs` merges enabled query rows
/// into whatever the URL already carries, so keeping `?limit=10` in both places would send it
/// twice. Splitting also means an imported request presents the same way whichever form the
/// export used, and the rows are what Zuno lets you tick off.
fn url_and_query(url: &Value, name: &str, skipped: &mut Vec<String>) -> (String, Vec<QueryParam>) {
    let raw = match url {
        Value::String(text) => text.clone(),
        Value::Object(fields) => match fields.get("raw").and_then(Value::as_str) {
            Some(raw) => raw.to_string(),
            // `raw` is what Postman shows and sends; the split fields are its parse, and some
            // exports carry only those. Rebuilt only when there is no `raw` to prefer.
            None => rebuild(fields),
        },
        other => {
            if !other.is_null() {
                skipped.push(format!("{name}: a URL in an unreadable shape"));
            }
            String::new()
        }
    };

    let (mut base, inline) = split_query(&raw);

    // Postman keeps *disabled* query rows here and not in `raw`, so when the array exists it is
    // the fuller record and the text's own pairs are a subset of it.
    let query = match url.get("query").and_then(Value::as_array) {
        Some(rows) => rows
            .iter()
            .filter_map(|row| {
                let key = row.get("key").and_then(Value::as_str)?;
                (!key.is_empty()).then(|| QueryParam {
                    enabled: !flag(row, "disabled"),
                    name: key.to_string(),
                    value: row.get("value").map(scalar).unwrap_or_default(),
                })
            })
            .collect(),
        None => inline,
    };

    // Path variables: `/users/:id` with `variable: [{"key":"id","value":"7"}]`. Substituted when
    // a value exists and left visible when it doesn't — a literal `:id` sends and 404s, which
    // shows you what to fix, where `{{id}}` would refuse to send at all. That is the wall
    // `openapi.rs` avoids for server variables, from the other direction.
    for row in array(url.get("variable")) {
        let Some(key) = row.get("key").and_then(Value::as_str) else {
            continue;
        };
        let value = row.get("value").map(scalar).unwrap_or_default();
        if !key.is_empty() && !value.is_empty() {
            base = base.replace(&format!(":{key}"), &value);
        }
    }

    (base, query)
}

/// Rebuild a URL from the split fields, for an export that carries no `raw`.
fn rebuild(fields: &Map<String, Value>) -> String {
    let protocol = fields
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or("https");

    let mut host = join(fields.get("host"), ".");
    if let Some(port) = fields.get("port").map(scalar).filter(|p| !p.is_empty()) {
        host.push(':');
        host.push_str(&port);
    }

    let path = join(fields.get("path"), "/");
    if path.is_empty() {
        format!("{protocol}://{host}")
    } else {
        format!("{protocol}://{host}/{path}")
    }
}

/// Postman writes host and path as arrays of segments, and occasionally as a plain string.
fn join(value: Option<&Value>, separator: &str) -> String {
    match value {
        Some(Value::Array(parts)) => parts
            .iter()
            .map(scalar)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(separator),
        Some(Value::String(text)) => text.trim_matches('/').to_string(),
        _ => String::new(),
    }
}

/// Split `?a=b` off a URL, decoding the pairs.
///
/// Decoded because `build.rs` re-encodes every row through `append_pair` on the way out, so
/// keeping `%20` here would send `%2520`.
fn split_query(raw: &str) -> (String, Vec<QueryParam>) {
    let Some((base, query)) = raw.split_once('?') else {
        return (raw.to_string(), Vec::new());
    };

    let params = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, value)| QueryParam::new(name, value))
        .collect();

    (base.to_string(), params)
}

fn body_from(body: &Value, name: &str, skipped: &mut Vec<String>) -> Body {
    match body.get("mode").and_then(Value::as_str).unwrap_or("none") {
        "raw" => {
            let text = body
                .get("raw")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Body::Raw {
                kind: raw_kind(body, &text),
                text,
            }
        }
        "urlencoded" => Body::Form(
            array(body.get("urlencoded"))
                .iter()
                .filter_map(|row| {
                    let key = row.get("key").and_then(Value::as_str)?;
                    (!key.is_empty()).then(|| FormField {
                        enabled: !flag(row, "disabled"),
                        name: key.to_string(),
                        value: row.get("value").map(scalar).unwrap_or_default(),
                    })
                })
                .collect(),
        ),
        "formdata" => Body::Multipart(
            array(body.get("formdata"))
                .iter()
                .filter_map(|row| {
                    let key = row.get("key").and_then(Value::as_str)?;
                    if key.is_empty() {
                        return None;
                    }
                    // A `file` row's `src` is a path on the machine that exported the
                    // collection, so it almost never exists here. Imported anyway: an empty
                    // file picker tells you nothing, and the path tells you what to re-attach.
                    let value = match row.get("type").and_then(Value::as_str) {
                        Some("file") => MultipartValue::File(
                            row.get("src").map(scalar).unwrap_or_default().into(),
                        ),
                        _ => MultipartValue::Text(row.get("value").map(scalar).unwrap_or_default()),
                    };
                    Some(MultipartField {
                        enabled: !flag(row, "disabled"),
                        name: key.to_string(),
                        value,
                    })
                })
                .collect(),
        ),
        "file" => match body
            .get("file")
            .and_then(|file| file.get("src"))
            .and_then(Value::as_str)
        {
            Some(src) if !src.is_empty() => Body::Binary(src.into()),
            _ => {
                skipped.push(format!("{name}: a file body with no path"));
                Body::Empty
            }
        },
        // GraphQL over HTTP *is* a JSON body, so it imports as the one it will become. That is
        // the whole of what Zuno needs to send these — no GraphQL model, no second body type,
        // and the query stays editable as the text it already was.
        "graphql" => {
            let graphql = body.get("graphql");
            let query = graphql
                .and_then(|graphql| graphql.get("query"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Postman stores the variables as a *string* of JSON, not as JSON.
            let variables = graphql
                .and_then(|graphql| graphql.get("variables"))
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or_else(|| Value::Object(Map::new()));

            let payload = serde_json::json!({ "query": query, "variables": variables });
            Body::Raw {
                text: serde_json::to_string_pretty(&payload).unwrap_or_default(),
                kind: RawKind::Json,
            }
        }
        "none" => Body::Empty,
        other => {
            skipped.push(format!("{name}: a {other} body"));
            Body::Empty
        }
    }
}

fn raw_kind(body: &Value, text: &str) -> RawKind {
    match body
        .get("options")
        .and_then(|options| options.get("raw"))
        .and_then(|raw| raw.get("language"))
        .and_then(Value::as_str)
    {
        Some("json") => RawKind::Json,
        Some("xml") => RawKind::Xml,
        Some("html") => RawKind::Html,
        Some(_) => RawKind::Text,
        // Postman's default language is `text`, but an export carrying no `options` at all is
        // usually an older one whose raw bodies are JSON regardless. So the text decides, which
        // is right far more often than the documented default.
        None => {
            if text.trim_start().starts_with(['{', '[']) {
                RawKind::Json
            } else {
                RawKind::Text
            }
        }
    }
}

/// What an auth block becomes. Zuno has no auth model, deliberately — a header is what goes on
/// the wire, and lowering auth to one keeps a single answer to "what will be sent".
enum AuthEffect {
    Header(Header),
    Query(QueryParam),
}

fn auth_effect(auth: &Value, name: &str, skipped: &mut Vec<String>) -> Option<AuthEffect> {
    let kind = auth.get("type").and_then(Value::as_str)?;

    // Postman stores each type's parameters as an *array* of `{key, value}` rows rather than as
    // an object, so every lookup goes through this.
    let field = |wanted: &str| -> String {
        array(auth.get(kind))
            .iter()
            .find(|row| row.get("key").and_then(Value::as_str) == Some(wanted))
            .and_then(|row| row.get("value"))
            .map(scalar)
            .unwrap_or_default()
    };

    match kind {
        "noauth" | "inherit" => None,
        "bearer" => Some(AuthEffect::Header(Header::new(
            "Authorization",
            format!("Bearer {}", field("token")),
        ))),
        "basic" => Some(AuthEffect::Header(Header::new(
            "Authorization",
            format!(
                "Basic {}",
                crate::curl::base64(format!("{}:{}", field("username"), field("password")).as_bytes())
            ),
        ))),
        "apikey" => {
            let (key, value) = (field("key"), field("value"));
            if key.is_empty() {
                return None;
            }
            match field("in").as_str() {
                "query" => Some(AuthEffect::Query(QueryParam::new(key, value))),
                // Postman's default when `in` is absent.
                _ => Some(AuthEffect::Header(Header::new(key, value))),
            }
        }
        other => {
            // OAuth 2, AWS SigV4, Digest, NTLM, Hawk. Each is a signing procedure rather than a
            // value to copy, so there is nothing to lower — and saying which one is missing
            // beats a request that looks complete and 401s.
            skipped.push(format!("{name}: {other} auth"));
            None
        }
    }
}

/// Name a request's scripts so a person knows their tests did not come across.
///
/// Reported rather than dropped, and by name rather than in full: the lines are still in the
/// file they were exported from, and recovering the common shapes — `pm.environment.set` into a
/// capture, a status check into `expect_status`, `pm.expect` into an assertion — is its own
/// slice. What must not happen is a silent loss of the half of a collection that makes it a
/// suite rather than a list.
fn note_scripts(item: &Value, name: &str, skipped: &mut Vec<String>) {
    for event in array(item.get("event")) {
        if flag(event, "disabled") {
            continue;
        }
        let lines = array(event.get("script").and_then(|script| script.get("exec"))).len();
        if lines == 0 {
            continue;
        }
        let listen = event
            .get("listen")
            .and_then(Value::as_str)
            .unwrap_or("script");
        skipped.push(format!("{name}: {listen} script, {lines} lines"));
    }
}

/// An array field, or nothing — Postman omits empty ones and occasionally writes `null`.
fn array(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A field's text, whatever scalar it was written as.
///
/// Ports, `apikey`'s `in`, and variable values all appear as numbers or booleans in real
/// exports, and `as_str` alone silently reads those as absent.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deliberately awkward export, in the shapes real ones actually use: a nested folder,
    /// collection auth with a request overriding it, a URL object whose `raw` carries the query
    /// *and* whose `query` array carries a disabled row, a path variable, one body of each mode,
    /// a script, a description, and a variable Postman has marked secret.
    const COLLECTION: &str = r##"{
      "info": {
        "name": "Billing",
        "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
      },
      "auth": { "type": "bearer", "bearer": [{ "key": "token", "value": "{{token}}" }] },
      "variable": [
        { "key": "baseUrl", "value": "https://api.test" },
        { "key": "apiKey", "value": "sk-live-1", "type": "secret" },
        { "key": "unused", "value": "x", "disabled": true }
      ],
      "event": [
        { "listen": "prerequest", "script": { "exec": ["console.log(1)"] } }
      ],
      "item": [
        {
          "name": "Invoices",
          "item": [
            {
              "name": "List",
              "description": "Every invoice",
              "request": {
                "method": "GET",
                "header": [
                  { "key": "Accept", "value": "application/json" },
                  { "key": "X-Debug", "value": "1", "disabled": true }
                ],
                "url": {
                  "raw": "{{baseUrl}}/invoices?limit=10",
                  "query": [
                    { "key": "limit", "value": "10" },
                    { "key": "cursor", "value": "", "disabled": true }
                  ]
                }
              },
              "event": [
                {
                  "listen": "test",
                  "script": { "exec": ["pm.test('ok', () => {})", "pm.response.json()"] }
                }
              ]
            },
            {
              "name": "Get",
              "request": {
                "method": "GET",
                "auth": {
                  "type": "basic",
                  "basic": [
                    { "key": "username", "value": "user" },
                    { "key": "password", "value": "pass" }
                  ]
                },
                "url": {
                  "raw": "{{baseUrl}}/invoices/:id",
                  "variable": [{ "key": "id", "value": "7" }]
                }
              }
            }
          ]
        },
        {
          "name": "Create",
          "request": {
            "method": "POST",
            "body": {
              "mode": "raw",
              "raw": "{\"amount\": 1}",
              "options": { "raw": { "language": "json" } }
            },
            "url": "{{baseUrl}}/invoices"
          }
        },
        {
          "name": "Login",
          "request": {
            "method": "POST",
            "auth": { "type": "noauth" },
            "body": {
              "mode": "urlencoded",
              "urlencoded": [
                { "key": "user", "value": "a" },
                { "key": "remember", "value": "1", "disabled": true }
              ]
            },
            "url": "{{baseUrl}}/login"
          }
        },
        {
          "name": "Upload",
          "request": {
            "method": "POST",
            "body": {
              "mode": "formdata",
              "formdata": [
                { "key": "note", "value": "hi", "type": "text" },
                { "key": "file", "type": "file", "src": "/home/them/a.pdf" }
              ]
            },
            "url": "{{baseUrl}}/uploads"
          }
        },
        {
          "name": "Graph",
          "request": {
            "method": "POST",
            "body": {
              "mode": "graphql",
              "graphql": {
                "query": "query Me { me { id } }",
                "variables": "{\"x\": 1}"
              }
            },
            "url": "{{baseUrl}}/graphql"
          }
        },
        {
          "name": "Search",
          "request": {
            "method": "PURGE",
            "auth": {
              "type": "apikey",
              "apikey": [
                { "key": "key", "value": "X-Api-Key" },
                { "key": "value", "value": "{{apiKey}}" },
                { "key": "in", "value": "header" }
              ]
            },
            "url": {
              "protocol": "https",
              "host": ["api", "test"],
              "port": 8443,
              "path": ["search"]
            }
          }
        },
        { "name": "Ping", "request": "https://api.test/ping?deep=1" }
      ]
    }"##;

    fn read(document: &str) -> Result<Import, PostmanError> {
        parse(&serde_json::from_str(document).expect("test document is valid JSON"))
    }

    fn imported() -> Import {
        read(COLLECTION).expect("parse")
    }

    fn find<'a>(import: &'a Import, name: &str) -> &'a Imported {
        import
            .requests
            .iter()
            .find(|request| request.spec.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no {name:?} in {:?}",
                    import
                        .requests
                        .iter()
                        .map(|request| &request.spec.name)
                        .collect::<Vec<_>>()
                )
            })
    }

    fn noted(import: &Import, fragment: &str) -> bool {
        import.skipped.iter().any(|note| note.contains(fragment))
    }

    #[test]
    fn the_item_tree_becomes_the_folders_it_was_organised_into() {
        let import = imported();
        assert_eq!(import.title.as_deref(), Some("Billing"));
        assert_eq!(import.requests.len(), 8);

        assert_eq!(find(&import, "List").folders, vec!["Invoices".to_string()]);
        assert_eq!(find(&import, "Get").folders, vec!["Invoices".to_string()]);
        // A request at the top level belongs to no folder but the import's own.
        assert!(find(&import, "Create").folders.is_empty());
    }

    #[test]
    fn a_query_is_split_off_the_url_so_nothing_is_sent_twice() {
        // `build.rs` merges enabled query rows into whatever the URL text already carries, so
        // leaving `?limit=10` in both places sends it twice. The disabled row is why the array
        // is preferred over the text when both exist: Postman keeps it only there.
        let import = imported();
        let list = &find(&import, "List").spec;
        assert_eq!(list.url, "{{baseUrl}}/invoices");
        assert_eq!(list.query.len(), 2);
        assert_eq!((list.query[0].name.as_str(), list.query[0].enabled), ("limit", true));
        assert_eq!((list.query[1].name.as_str(), list.query[1].enabled), ("cursor", false));

        // And a bare string URL is split the same way, so an imported request presents
        // identically whichever form the export used.
        let ping = &find(&import, "Ping").spec;
        assert_eq!(ping.url, "https://api.test/ping");
        assert_eq!(ping.query.len(), 1);
        assert_eq!(ping.query[0].value, "1");
    }

    #[test]
    fn a_path_variable_with_a_value_is_substituted_rather_than_left_to_404() {
        let import = imported();
        assert_eq!(find(&import, "Get").spec.url, "{{baseUrl}}/invoices/7");
    }

    #[test]
    fn a_disabled_row_arrives_muted_rather_than_missing() {
        // Importing it enabled sends a header someone had switched off; dropping it loses the
        // fact that they had it.
        let import = imported();
        let list = &find(&import, "List").spec;
        let debug = list
            .headers
            .iter()
            .find(|header| header.name == "X-Debug")
            .expect("a disabled header must still import");
        assert!(!debug.enabled);

        let Body::Form(fields) = &find(&import, "Login").spec.body else {
            panic!("urlencoded must import as a form");
        };
        assert_eq!(fields.len(), 2);
        assert!(!fields[1].enabled);
    }

    #[test]
    fn collection_auth_reaches_every_request_and_a_request_can_replace_it() {
        let import = imported();

        // Inherited, and `{{token}}` needs no rewriting — Postman's variable syntax is Zuno's.
        let list = &find(&import, "List").spec;
        assert_eq!(
            list.headers
                .iter()
                .find(|header| header.name == "Authorization")
                .map(|header| header.value.as_str()),
            Some("Bearer {{token}}")
        );

        // Replaced at the request.
        let get = &find(&import, "Get").spec;
        assert_eq!(
            get.headers
                .iter()
                .find(|header| header.name == "Authorization")
                .map(|header| header.value.as_str()),
            Some("Basic dXNlcjpwYXNz")
        );

        // `noauth` at the request turns the collection's off rather than being ignored.
        let login = &find(&import, "Login").spec;
        assert!(
            !login.headers.iter().any(|header| header.name == "Authorization"),
            "noauth must not inherit: {:?}",
            login.headers
        );

        // An API key lands as the header it names, not as `Authorization`.
        let search = &find(&import, "Search").spec;
        assert_eq!(
            search
                .headers
                .iter()
                .find(|header| header.name == "X-Api-Key")
                .map(|header| header.value.as_str()),
            Some("{{apiKey}}")
        );
    }

    #[test]
    fn every_body_mode_arrives_in_a_shape_zuno_can_edit() {
        let import = imported();

        assert_eq!(
            find(&import, "Create").spec.body,
            Body::Raw {
                text: "{\"amount\": 1}".to_string(),
                kind: RawKind::Json
            }
        );

        let Body::Multipart(fields) = &find(&import, "Upload").spec.body else {
            panic!("formdata must import as multipart");
        };
        assert_eq!(fields[0].value, MultipartValue::Text("hi".to_string()));
        // The path is on the machine that exported the collection and almost never exists here.
        // Imported anyway: it says what to re-attach, where an empty picker says nothing.
        assert_eq!(
            fields[1].value,
            MultipartValue::File("/home/them/a.pdf".into())
        );
    }

    #[test]
    fn a_graphql_body_imports_as_the_json_it_would_have_been_sent_as() {
        // Which is all GraphQL is over HTTP — so this needs no GraphQL model, no second body
        // type, and the query stays editable as the text it already was.
        let import = imported();
        let Body::Raw { text, kind } = &find(&import, "Graph").spec.body else {
            panic!("graphql must import as a raw body");
        };
        assert_eq!(*kind, RawKind::Json);

        let sent: serde_json::Value = serde_json::from_str(text).expect("valid JSON");
        assert_eq!(sent["query"], "query Me { me { id } }");
        // Postman stores the variables as a *string* of JSON; sending that verbatim would put a
        // quoted string where the server expects an object.
        assert_eq!(sent["variables"]["x"], 1);
    }

    #[test]
    fn a_method_zuno_has_no_variant_for_still_imports() {
        let import = imported();
        assert_eq!(
            find(&import, "Search").spec.method,
            Method::Other("PURGE".to_string())
        );
        // And the URL rebuilt from split fields keeps its port.
        assert_eq!(find(&import, "Search").spec.url, "https://api.test:8443/search");
    }

    #[test]
    fn scripts_are_named_rather_than_dropped_silently() {
        // The half of a collection that makes it a suite rather than a list. Recovering the
        // common shapes is its own slice; losing them without a word is the thing that must not
        // happen in this one.
        let import = imported();
        assert!(noted(&import, "List: test script, 2 lines"), "{:?}", import.skipped);
        assert!(noted(&import, "the collection: prerequest script"), "{:?}", import.skipped);
    }

    #[test]
    fn variables_arrive_with_postmans_own_secret_marking() {
        let import = imported();
        assert_eq!(import.variables.len(), 2, "{:?}", import.variables);

        let base = &import.variables[0];
        assert_eq!((base.name.as_str(), base.value.as_str()), ("baseUrl", "https://api.test"));
        assert!(!base.secret);

        // Marked by Postman, so the split invariant 10 draws survives the crossing rather than
        // being guessed from the name.
        assert!(import.variables[1].secret);

        // A disabled variable is dormant in Postman, and an environment has no row to switch
        // off, so importing it would turn it back on.
        assert!(
            !import.variables.iter().any(|variable| variable.name == "unused"),
            "a disabled variable must not import"
        );
    }

    #[test]
    fn a_description_is_reported_once_rather_than_per_request() {
        let import = imported();
        assert!(noted(&import, "1 descriptions"), "{:?}", import.skipped);
    }

    #[test]
    fn folders_deeper_than_the_scanner_walks_are_flattened_not_written() {
        // A request written below `collection::MAX_DEPTH` is on disk and invisible: `scan` never
        // reaches it, so the tree cannot show it and the picker cannot find it. Flattening is
        // the least-bad answer, and it has to be said out loud.
        let mut document = String::from(r#"{"info":{"name":"Deep"},"item":["#);
        let levels = MAX_FOLDER_DEPTH + 2;
        for level in 0..levels {
            document.push_str(&format!(r#"{{"name":"f{level}","item":["#));
        }
        document.push_str(r#"{"name":"leaf","request":"https://a.test/leaf"}"#);
        for _ in 0..levels {
            document.push_str("]}");
        }
        document.push_str("]}");

        let import = read(&document).expect("parse");
        let leaf = find(&import, "leaf");
        assert_eq!(leaf.folders.len(), MAX_FOLDER_DEPTH);
        assert_eq!(leaf.folders[0], "f0");
        assert!(noted(&import, "flattened into"), "{:?}", import.skipped);
    }

    #[test]
    fn a_collection_this_parser_cannot_read_is_refused_by_reason() {
        assert!(matches!(
            read(r#"{"info":{"name":"x"}}"#),
            Err(PostmanError::NotPostman)
        ));
        assert!(matches!(
            read(r#"{"info":{"name":"x","schema":"https://schema.getpostman.com/json/collection/v1.0.0/collection.json"},"item":[]}"#),
            Err(PostmanError::UnsupportedSchema(_))
        ));
        // A collection of nothing but folders has nothing to import, and saying so beats
        // creating an empty directory tree.
        assert!(matches!(
            read(r#"{"info":{"name":"x"},"item":[{"name":"f","item":[]}]}"#),
            Err(PostmanError::NoItems)
        ));
    }

}
