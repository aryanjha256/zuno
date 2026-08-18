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
/// `Raw::text` is a plain `String` for now. It becomes a rope-backed
/// `TextBuffer` in M1.4, when the multi-line editor lands and edit operations
/// start mattering. Nothing edits it yet, so the swap is contained.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSettings {
    pub timeout: Option<Duration>,
    pub follow_redirects: bool,
    pub max_redirects: u8,
    pub verify_tls: bool,
    pub accept_encodings: bool,
}

impl Default for RequestSettings {
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(30)),
            follow_redirects: true,
            max_redirects: 10,
            verify_tls: true,
            accept_encodings: true,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn spec_roundtrips_through_serde() {
        let spec = RequestSpec::sample();
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: RequestSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }
}
