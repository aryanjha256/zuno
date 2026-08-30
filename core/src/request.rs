//! The request model. See architecture.md §3.1 for the reasoning behind the
//! three load-bearing decisions here:
//!
//! 1. Headers and query params are ordered `Vec`s with a per-row `enabled` flag,
//!    not maps. Duplicate keys, typed order, and disable-without-delete are all
//!    requirements, and a map makes all three impossible.
//! 2. `url` stays a raw `String`. Users type invalid URLs on every keystroke and
//!    `{{baseUrl}}/users` will never parse. Parsing happens at the send boundary.
//! 3. `Method::Other` exists because custom verbs and typos both need to be sendable.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Method {
    #[default]
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Other(String),
}

impl Method {
    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
            Method::Other(verb) => verb,
        }
    }

    /// The methods offered in the picker, in the order they should appear.
    pub fn common() -> [Method; 7] {
        [
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Patch,
            Method::Delete,
            Method::Head,
            Method::Options,
        ]
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single header row. `enabled` is what lets you mute a header without losing
/// what you typed — half of how people actually debug a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub enabled: bool,
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { enabled: true, name: name.into(), value: value.into() }
    }

    pub fn disabled(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { enabled: false, name: name.into(), value: value.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryParam {
    pub enabled: bool,
    pub name: String,
    pub value: String,
}

impl QueryParam {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { enabled: true, name: name.into(), value: value.into() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawKind {
    #[default]
    Json,
    Text,
    Xml,
    Html,
}

impl RawKind {
    pub fn content_type(&self) -> &'static str {
        match self {
            RawKind::Json => "application/json",
            RawKind::Text => "text/plain",
            RawKind::Xml => "application/xml",
            RawKind::Html => "text/html",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            RawKind::Json => "JSON",
            RawKind::Text => "Text",
            RawKind::Xml => "XML",
            RawKind::Html => "HTML",
        }
    }
}

/// The request body.
///
/// `Raw::text` is a plain `String`, and stays one — the rope was dropped in M1.4 after
/// measuring that a line-index rescan is ~10µs on a 100KB body. See architecture.md §7.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    #[default]
    Empty,
    Raw {
        text: String,
        kind: RawKind,
    },
    Form(Vec<FormField>),
    Multipart(Vec<MultipartField>),
    Binary(PathBuf),
}

impl Body {
    pub fn label(&self) -> &'static str {
        match self {
            Body::Empty => "None",
            Body::Raw { kind, .. } => kind.label(),
            Body::Form(_) => "Form",
            Body::Multipart(_) => "Multipart",
            Body::Binary(_) => "Binary",
        }
    }

    /// The text of a raw body, if this is one. Used by the editor and by the
    /// shell's read-only preview.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Body::Raw { text, .. } => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormField {
    pub enabled: bool,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartField {
    pub enabled: bool,
    pub name: String,
    pub value: MultipartValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultipartValue {
    Text(String),
    File(PathBuf),
}

/// `#[serde(default)]` at the container level is load-bearing for persistence: any field
/// missing from a saved session is filled from `Default`, so adding a setting can't break
/// files written by an older build. Adding `cookie_store` without this made every existing
/// session fail to deserialize and silently fall back to the sample request.
///
/// `RequestSpec` deliberately does *not* do this — it must stay strict enough that a
/// corrupt file is rejected rather than quietly becoming an empty request. New fields
/// there need a per-field `#[serde(default)]` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestSettings {
    pub timeout: Option<Duration>,
    pub follow_redirects: bool,
    pub max_redirects: u8,
    pub verify_tls: bool,
    pub accept_encodings: bool,
    /// Whether responses' cookies are remembered and replayed on later requests.
    ///
    /// Defaults to `true`, matching Postman and browsers — it's usually what you want
    /// when the second request depends on the first one's login. But it does make
    /// requests non-independent, so it has to be *visible and switchable* rather than
    /// silently hardcoded on, which is what it was before.
    pub cookie_store: bool,
}

impl Default for RequestSettings {
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(30)),
            follow_redirects: true,
            max_redirects: 10,
            verify_tls: true,
            accept_encodings: true,
            cookie_store: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSpec {
    pub id: RequestId,
    pub name: String,
    pub method: Method,
    /// Raw, unresolved, possibly invalid. Parsed only at the send boundary.
    pub url: String,
    pub query: Vec<QueryParam>,
    pub headers: Vec<Header>,
    pub body: Body,
    pub settings: RequestSettings,
}

impl Default for RequestSpec {
    fn default() -> Self {
        Self {
            id: RequestId(0),
            name: "Untitled".to_string(),
            method: Method::Get,
            url: String::new(),
            query: Vec::new(),
            headers: Vec::new(),
            body: Body::Empty,
            settings: RequestSettings::default(),
        }
    }
}

impl RequestSpec {
    /// Only the rows that will actually go on the wire.
    pub fn enabled_headers(&self) -> impl Iterator<Item = &Header> {
        self.headers.iter().filter(|header| header.enabled)
    }

    pub fn enabled_query(&self) -> impl Iterator<Item = &QueryParam> {
        self.query.iter().filter(|param| param.enabled)
    }

    /// A populated request for the M1.0 shell to render. Replaced by real
    /// editing in M1.1.
    pub fn sample() -> Self {
        Self {
            id: RequestId(1),
            name: "List repositories".to_string(),
            method: Method::Post,
            url: "https://api.github.com/graphql".to_string(),
            query: vec![QueryParam::new("per_page", "50")],
            headers: vec![
                Header::new("Content-Type", "application/json"),
                Header::new("Accept", "application/vnd.github+json"),
                Header::new("User-Agent", "zuno/0.1.0"),
                Header::disabled("Authorization", "Bearer {{token}}"),
            ],
            body: Body::Raw {
                text: "{\n  \"query\": \"{ viewer { login } }\"\n}".to_string(),
                kind: RawKind::Json,
            },
            settings: RequestSettings::default(),
        }
    }
}

/// A short label for a tab strip or a picker row.
///
/// Takes the raw strings rather than a `&RequestSpec` because the caller that matters is
/// a tab strip, and assembling a spec per tab per frame would clone every header — see
/// `RequestView::spec`. Borrows, so the caller decides whether to allocate.
///
/// Derived from the **current URL** in preference to `name`, because nothing can edit
/// `name` yet: it is only ever set from the URL at import time, so a request since pointed
/// elsewhere would otherwise keep advertising its old target. `name` wins only when there's
/// no URL to derive from — a brand-new buffer, where "Untitled" is the honest answer.
///
/// When a rename action exists this should prefer a user-set `name`, which needs a way to
/// tell "the user typed this" from "the importer guessed it". There isn't one today.
pub fn label_for<'a>(url: &'a str, name: &'a str) -> &'a str {
    let derived = label_from_url(url);
    if !derived.is_empty() {
        derived
    } else if !name.is_empty() {
        name
    } else {
        "Untitled"
    }
}

/// Shorten a label to `max_chars`, ending it in `…` when anything was dropped.
///
/// **Done here rather than left to gpui's `truncate()`**, which shipped twice not working. That
/// helper only ellipsizes text it has been handed a *definite* width and caches its first
/// measurement, so whether it fires depends on layout details several elements away — and when it
/// doesn't fire there is no error, just the hard cut it was supposed to remove. This is a pure
/// function over a string: it either shortened the label or it didn't, and a unit test can say
/// which without a window.
///
/// Counting characters rather than measuring pixels is the deliberate imprecision. Real widths
/// need the shaping font, the test platform's font is not the one that ships, and a label region
/// is a fixed size chosen by us — so the caller picks a budget for its own width and the ellipsis
/// lands a little early on narrow glyphs. `truncate()` stays on the element underneath as the
/// backstop for a label of pathologically wide characters.
pub fn elide(label: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if max_chars == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    // `chars().count()` walks the string, but a tab label is tens of bytes and this runs per tab
    // per frame — the alternative is `char_indices` bookkeeping for no measurable gain.
    if label.chars().count() <= max_chars {
        return std::borrow::Cow::Borrowed(label);
    }
    // `max_chars - 1` because the ellipsis occupies one of the budgeted characters. Slicing by
    // `char_indices` rather than by byte keeps this correct for multi-byte labels, which a URL
    // path segment can certainly be.
    let end = label
        .char_indices()
        .nth(max_chars - 1)
        .map(|(ix, _)| ix)
        .unwrap_or(label.len());
    let mut out = String::with_capacity(end + 3);
    out.push_str(&label[..end]);
    out.push('…');
    std::borrow::Cow::Owned(out)
}

/// The last meaningful piece of a URL — its final path segment, or the host when there
/// isn't one. Empty when nothing usable is there.
///
/// **A colon only means `host:port` in the *first* segment.** Treating it as authority evidence
/// anywhere made every Google-style `:verb` endpoint — `/v1/files:batchUpdate`,
/// `/v1/models/x:predict`, and everything gRPC transcoding produces — label as the bare host, so
/// each one showed the same tab title and derived the same filename as the last.
fn label_from_url(url: &str) -> &str {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path = without_scheme.split(['?', '#']).next().unwrap_or("");

    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let first = segments.next().unwrap_or("");
    // `next_back` after `next` yields the last of what *remains*, so `None` means `first` was the
    // only segment — which is the one case where a colon really is a port.
    let (segment, only_segment) = match segments.next_back() {
        Some(last) => (last, false),
        None => (first, true),
    };

    if segment.is_empty() || (only_segment && segment.contains(':')) {
        // Bare host, or host:port — the segment found was the authority, not a path.
        without_scheme.split(['/', '?', '#']).next().unwrap_or("")
    } else {
        segment
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_label_shorter_than_the_budget_is_untouched() {
        assert_eq!(elide("posts", 22), "posts");
        // Exactly at the budget still fits — off-by-one here would ellipsize a label that
        // needed no shortening, which is the visible half of getting this wrong.
        assert_eq!(elide("abcde", 5), "abcde");
    }

    #[test]
    fn a_long_label_is_ellipsised_within_its_budget() {
        let out = elide("shadcnschemaregistry.json", 22);
        assert!(out.ends_with('…'), "{out}");
        assert_eq!(
            out.chars().count(),
            22,
            "the ellipsis has to come out of the budget, not be added to it: {out}"
        );
        assert_eq!(out, "shadcnschemaregistry.…");
    }

    #[test]
    fn eliding_splits_on_characters_and_never_inside_one() {
        // A path segment can be multi-byte, and slicing by byte would panic rather than
        // shorten. Each of these is 3 bytes, so a byte-based cut lands mid-character.
        let out = elide("日本語のパス", 4);
        assert_eq!(out, "日本語…");
        assert_eq!(out.chars().count(), 4);
    }

    #[test]
    fn a_budget_of_zero_yields_nothing_rather_than_panicking() {
        assert_eq!(elide("anything", 0), "");
        // One character of budget is all ellipsis — degenerate, but it must not underflow
        // `max_chars - 1`.
        assert_eq!(elide("anything", 1), "…");
    }
    use super::*;

    #[test]
    fn a_label_comes_from_the_last_path_segment() {
        assert_eq!(label_for("https://api.example.com/v1/users", ""), "users");
        // A trailing slash must not produce an empty label — real pasted URLs have them.
        assert_eq!(label_for("https://api.example.com/v1/users/", ""), "users");
        // Query and fragment are not part of the name.
        assert_eq!(label_for("https://api.example.com/posts?page=2#top", ""), "posts");
    }

    #[test]
    fn a_path_segment_may_contain_a_colon() {
        // Google-style REST and gRPC transcoding use `:verb` throughout. Reading the colon as
        // host:port labelled all of them as the host, so every such endpoint on one host shared a
        // tab title and derived the same collection filename.
        assert_eq!(
            label_for("https://api.test/v1/files:batchUpdate", ""),
            "files:batchUpdate"
        );
        assert_eq!(label_for("https://api.test/v1/models/x:predict", ""), "x:predict");
        // Still a port when it is the only segment there is.
        assert_eq!(label_for("http://localhost:8080", ""), "localhost:8080");
        assert_eq!(label_for("localhost:3000/health", ""), "health");
    }

    #[test]
    fn a_bare_host_labels_as_the_host() {
        assert_eq!(label_for("https://api.example.com", ""), "api.example.com");
        assert_eq!(label_for("http://localhost:8080", ""), "localhost:8080");
    }

    #[test]
    fn a_label_tracks_the_url_rather_than_a_stale_name() {
        // The case that motivated deriving: a request imported as one thing and since
        // pointed somewhere else must not keep advertising the old target.
        assert_eq!(
            label_for("https://jsonplaceholder.typicode.com/posts", "anchorsForUser"),
            "posts"
        );
    }

    #[test]
    fn nothing_to_derive_from_falls_back_to_the_name_then_to_untitled() {
        // A brand-new buffer has no URL at all.
        assert_eq!(label_for("", ""), "Untitled");
        assert_eq!(label_for("", "Scratch"), "Scratch");
        // A URL that is only a scheme is as good as empty.
        assert_eq!(label_for("https://", ""), "Untitled");
        // Partially typed, which is what the strip sees on most keystrokes.
        assert_eq!(label_for("https://api.exa", ""), "api.exa");
    }

    #[test]
    fn disabled_rows_are_excluded_from_the_wire() {
        let spec = RequestSpec::sample();
        assert_eq!(spec.headers.len(), 4);
        assert_eq!(spec.enabled_headers().count(), 3);
        assert!(spec.enabled_headers().all(|header| header.name != "Authorization"));
    }

    #[test]
    fn duplicate_header_names_are_preserved_in_order() {
        let mut spec = RequestSpec::default();
        spec.headers.push(Header::new("Set-Cookie", "a=1"));
        spec.headers.push(Header::new("Set-Cookie", "b=2"));

        let values: Vec<&str> = spec.enabled_headers().map(|h| h.value.as_str()).collect();
        assert_eq!(values, vec!["a=1", "b=2"], "a map-backed model would lose one of these");
    }

    #[test]
    fn unparseable_urls_are_representable() {
        // The model must hold what the user typed, however broken.
        let mut spec = RequestSpec::default();
        spec.url = "{{baseUrl}}/users?id=".to_string();
        assert_eq!(spec.url, "{{baseUrl}}/users?id=");
    }

    #[test]
    fn a_session_missing_a_newer_setting_still_loads() {
        // Regression: exactly the shape written before `cookie_store` existed. Without
        // the container-level serde default this fails with "missing field", and a real
        // saved session gets silently discarded.
        let json = r#"{
            "id": 1,
            "name": "Saved earlier",
            "method": "Get",
            "url": "https://jsonplaceholder.typicode.com/posts",
            "query": [],
            "headers": [],
            "body": "Empty",
            "settings": {
                "timeout": { "secs": 30, "nanos": 0 },
                "follow_redirects": true,
                "max_redirects": 10,
                "verify_tls": true,
                "accept_encodings": true
            }
        }"#;

        let spec: RequestSpec = serde_json::from_str(json).expect("an older session should load");
        assert_eq!(spec.url, "https://jsonplaceholder.typicode.com/posts");
        assert!(
            spec.settings.cookie_store,
            "a missing setting should take its default, not fail the load"
        );
    }

    #[test]
    fn spec_roundtrips_through_serde() {
        let spec = RequestSpec::sample();
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: RequestSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }
}
