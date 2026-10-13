//! What changed in a request, field by field — the Changes tab's content.
//!
//! **Sections of key/value rows, not a text diff of the file.** The file is pretty JSON with the
//! body stored as one escaped string, so a line diff of it says "line 14 changed" where a person
//! wants `$.amount 100 → 250`. Each part of a request is turned into an ordered list of
//! `(key, value)` pairs — a header by its name, a JSON body by its paths, other text by its lines —
//! and two lists compare the same way whatever they came from. That uniformity is why every kind,
//! WebSocket and gRPC included, gets a diff without code of its own.

use serde_json::Value;

use crate::request::{Auth, Body, Header, MultipartValue, RequestKind, RequestSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub mark: Mark,
    pub key: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecDiff {
    /// Only sections with a difference, in reading order.
    pub sections: Vec<Section>,
    /// The sections that compared equal, for the one line that says so.
    pub unchanged: Vec<&'static str>,
}

impl SpecDiff {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

type Pairs = Vec<(String, String)>;

/// `old` → `new`, by section.
pub fn diff(old: &RequestSpec, new: &RequestSpec) -> SpecDiff {
    let mut result = SpecDiff::default();
    let parts: [(&'static str, fn(&RequestSpec) -> Pairs); 8] = [
        ("Request", request),
        ("Headers", |spec| rows_of(&spec.headers)),
        ("Query", query),
        ("Auth", auth),
        ("Body", body),
        ("Assertions", assertions),
        ("Captures", captures),
        ("Settings", |spec| flattened(&serde_json::to_value(&spec.settings).unwrap_or_default())),
    ];
    for (title, pairs) in parts {
        let rows = compare(&pairs(old), &pairs(new));
        if rows.is_empty() {
            result.unchanged.push(title);
        } else {
            result.sections.push(Section { title, rows });
        }
    }
    result
}

/// Method and URL, and — for the kinds whose content is not a body — everything else the kind
/// carries, flattened.
fn request(spec: &RequestSpec) -> Pairs {
    let mut pairs = vec![("kind".to_string(), spec.kind.label().to_string())];
    if let Some(method) = spec.method() {
        pairs.push(("method".into(), method.as_str().to_string()));
    }
    pairs.push(("URL".into(), spec.url.clone()));
    match &spec.kind {
        RequestKind::Http(_) => {}
        RequestKind::GraphQl(graphql) => {
            if let Some(operation) = &graphql.operation {
                pairs.push(("operation".into(), operation.clone()));
            }
            pairs.push(("transport".into(), graphql.transport.label().to_string()));
        }
        // The kind's own JSON, minus nothing: every field it has is the request.
        other => {
            let value = serde_json::to_value(other).unwrap_or_default();
            pairs.extend(flattened(&value).into_iter().filter(|(key, _)| key != "$"));
        }
    }
    pairs
}

fn query(spec: &RequestSpec) -> Pairs {
    match &spec.kind {
        RequestKind::Http(http) => keyed(http.query.iter().map(|q| (q.name.as_str(), q.value.as_str(), q.enabled))),
        _ => Vec::new(),
    }
}

fn rows_of(headers: &[Header]) -> Pairs {
    keyed(headers.iter().map(|h| (h.name.as_str(), h.value.as_str(), h.enabled)))
}

/// Named rows, keyed by name — a repeated name numbered, a muted row marked, so muting one shows
/// as a change rather than as nothing.
fn keyed<'a>(rows: impl Iterator<Item = (&'a str, &'a str, bool)>) -> Pairs {
    let mut seen: Vec<String> = Vec::new();
    rows.map(|(name, value, enabled)| {
        let lower = name.to_ascii_lowercase();
        let n = seen.iter().filter(|s| **s == lower).count();
        seen.push(lower);
        let key = if n == 0 { name.to_string() } else { format!("{name} #{}", n + 1) };
        let value = if enabled { value.to_string() } else { format!("{value} (off)") };
        (key, value)
    })
    .collect()
}

fn auth(spec: &RequestSpec) -> Pairs {
    match &spec.auth {
        Auth::None => Vec::new(),
        Auth::Basic { username, password } => vec![
            ("type".into(), "Basic".into()),
            ("username".into(), username.clone()),
            ("password".into(), password.clone()),
        ],
        Auth::Bearer { token } => vec![("type".into(), "Bearer".into()), ("token".into(), token.clone())],
    }
}

fn body(spec: &RequestSpec) -> Pairs {
    match &spec.kind {
        RequestKind::Http(http) => {
            let mut pairs = vec![("type".to_string(), http.body.label().to_string())];
            match &http.body {
                Body::Empty => {}
                Body::Raw { text, .. } => pairs.extend(text_pairs(text, "")),
                Body::Form(fields) => {
                    pairs.extend(keyed(fields.iter().map(|f| (f.name.as_str(), f.value.as_str(), f.enabled))))
                }
                Body::Multipart(parts) => pairs.extend(keyed(parts.iter().map(|p| {
                    let value = match &p.value {
                        MultipartValue::Text(text) => text.as_str(),
                        MultipartValue::File(path) => path.to_str().unwrap_or_default(),
                    };
                    (p.name.as_str(), value, p.enabled)
                }))),
                Body::Binary(path) => pairs.push(("file".into(), path.display().to_string())),
            }
            pairs
        }
        RequestKind::GraphQl(graphql) => {
            let mut pairs = text_pairs(&graphql.query, "query ");
            if !graphql.variables.trim().is_empty() {
                pairs.extend(text_pairs(&graphql.variables, "variables "));
            }
            if !graphql.connection_params.trim().is_empty() {
                pairs.extend(text_pairs(&graphql.connection_params, "connection "));
            }
            pairs
        }
        _ => Vec::new(),
    }
}

/// Text as pairs: JSON by its paths when it parses, its lines otherwise — a body holding an
/// unquoted `{{var}}` is not JSON, and still deserves a readable diff.
fn text_pairs(text: &str, prefix: &str) -> Pairs {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return flattened(&value)
            .into_iter()
            .map(|(path, value)| (format!("{prefix}{path}"), value))
            .collect();
    }
    text.lines()
        .enumerate()
        .map(|(n, line)| (format!("{prefix}line {}", n + 1), line.to_string()))
        .collect()
}

fn assertions(spec: &RequestSpec) -> Pairs {
    let mut pairs = Vec::new();
    if let Some(status) = spec.expect_status {
        pairs.push(("expected status".to_string(), status.to_string()));
    }
    pairs.extend(keyed(spec.assertions.iter().map(|a| (a.path.as_str(), a.value.as_str(), a.enabled))).into_iter().zip(&spec.assertions).map(
        |((key, value), assertion)| (format!("{key} {}", assertion.op.label()), value),
    ));
    pairs
}

fn captures(spec: &RequestSpec) -> Pairs {
    keyed(spec.captures.iter().map(|c| (c.name.as_str(), c.path.as_str(), c.enabled)))
}

/// Every leaf of a JSON value under its `$.path`, in document order. Empty containers are leaves,
/// or `{}` → `{"a": 1}` would have nothing to show on the left.
fn flattened(value: &Value) -> Pairs {
    let mut out = Vec::new();
    walk(value, "$".to_string(), &mut out);
    out
}

fn walk(value: &Value, path: String, out: &mut Pairs) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let simple = key.chars().all(|c| c.is_alphanumeric() || c == '_');
                let next = if simple { format!("{path}.{key}") } else { format!("{path}[{key:?}]") };
                walk(child, next, out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (ix, child) in items.iter().enumerate() {
                walk(child, format!("{path}[{ix}]"), out);
            }
        }
        leaf => out.push((path, leaf.to_string())),
    }
}

/// Rows for `new` against `old`: in `new`'s order, each key changed or added, then whatever `old`
/// had that `new` does not.
fn compare(old: &Pairs, new: &Pairs) -> Vec<Row> {
    let mut rows = Vec::new();
    for (key, value) in new {
        match old.iter().find(|(k, _)| k == key) {
            Some((_, before)) if before == value => {}
            Some((_, before)) => rows.push(Row {
                mark: Mark::Changed,
                key: key.clone(),
                old: Some(before.clone()),
                new: Some(value.clone()),
            }),
            None => rows.push(Row { mark: Mark::Added, key: key.clone(), old: None, new: Some(value.clone()) }),
        }
    }
    for (key, value) in old {
        if !new.iter().any(|(k, _)| k == key) {
            rows.push(Row { mark: Mark::Removed, key: key.clone(), old: Some(value.clone()), new: None });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{HttpRequest, RawKind};

    fn http(body: &str) -> RequestSpec {
        let mut spec = RequestSpec::default();
        spec.url = "{{base}}/orders".into();
        spec.headers = vec![Header::new("Content-Type", "application/json"), Header::new("X-Debug", "1")];
        spec.kind = RequestKind::Http(HttpRequest {
            body: Body::Raw { text: body.into(), kind: RawKind::Json },
            ..Default::default()
        });
        spec
    }

    #[test]
    fn a_json_body_differs_by_path_and_headers_by_name() {
        let old = http(r#"{"amount": 100, "items": [{"qty": 1}]}"#);
        let mut new = http(r#"{"amount": 250, "currency": "INR", "items": [{"qty": 2}]}"#);
        new.headers.retain(|h| h.name != "X-Debug");
        new.headers.push(Header::new("Idempotency-Key", "{{key}}"));
        new.expect_status = Some(201);

        let diff = diff(&old, &new);
        let body = diff.sections.iter().find(|s| s.title == "Body").expect("body");
        let keys: Vec<(&str, Mark)> = body.rows.iter().map(|r| (r.key.as_str(), r.mark)).collect();
        assert_eq!(
            keys,
            vec![("$.amount", Mark::Changed), ("$.currency", Mark::Added), ("$.items[0].qty", Mark::Changed)]
        );
        assert_eq!(body.rows[0].old.as_deref(), Some("100"));
        assert_eq!(body.rows[0].new.as_deref(), Some("250"));

        let headers = diff.sections.iter().find(|s| s.title == "Headers").expect("headers");
        assert!(headers.rows.iter().any(|r| r.key == "Idempotency-Key" && r.mark == Mark::Added));
        assert!(headers.rows.iter().any(|r| r.key == "X-Debug" && r.mark == Mark::Removed));
        assert!(diff.sections.iter().any(|s| s.title == "Assertions"));
        assert!(diff.unchanged.contains(&"Request") && diff.unchanged.contains(&"Auth"));
    }

    #[test]
    fn a_body_that_is_not_json_differs_by_line() {
        let old = http("{\"a\": {{n}}}\nsecond");
        let new = http("{\"a\": {{n}}}\nchanged");
        let body = diff(&old, &new).sections.into_iter().find(|s| s.title == "Body").expect("body");
        assert_eq!(body.rows, vec![Row {
            mark: Mark::Changed,
            key: "line 2".into(),
            old: Some("second".into()),
            new: Some("changed".into()),
        }]);
    }

    #[test]
    fn muting_a_header_is_a_change() {
        let old = http("{}");
        let mut new = http("{}");
        new.headers[1].enabled = false;
        let headers = diff(&old, &new).sections.into_iter().find(|s| s.title == "Headers").expect("headers");
        assert_eq!(headers.rows[0].new.as_deref(), Some("1 (off)"));
    }

    #[test]
    fn identical_requests_have_no_sections() {
        assert!(diff(&http("{}"), &http("{}")).is_empty());
    }
}
