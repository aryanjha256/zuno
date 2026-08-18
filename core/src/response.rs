//! The response model. See architecture.md §3.2.
//!
//! `body` is `Bytes`, never `String`. Binary payloads and invalid UTF-8 are
//! normal, not edge cases, and `Bytes` is what lets the JSON viewer hold byte
//! spans into the original buffer instead of copied strings (§6).

use std::time::Duration;

use bytes::Bytes;

use crate::request::Header;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HttpVersion {
    Http09,
    Http10,
    #[default]
    Http11,
    Http2,
    Http3,
}

impl HttpVersion {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpVersion::Http09 => "HTTP/0.9",
            HttpVersion::Http10 => "HTTP/1.0",
            HttpVersion::Http11 => "HTTP/1.1",
            HttpVersion::Http2 => "HTTP/2",
            HttpVersion::Http3 => "HTTP/3",
        }
    }
}

/// Per-stage timings. Everything before `ttfb` is optional because a reused
/// connection skips DNS, connect, and TLS entirely.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timing {
    pub dns: Option<Duration>,
    pub connect: Option<Duration>,
    pub tls: Option<Duration>,
    pub ttfb: Duration,
    pub total: Duration,
}

/// Wire size and decoded size are both interesting — the ratio is how you spot
/// whether compression actually happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SizeInfo {
    pub wire: u64,
    pub decoded: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Informational,
    Success,
    Redirect,
    ClientError,
    ServerError,
    Unknown,
}

impl StatusClass {
    pub fn of(status: u16) -> Self {
        match status {
            100..=199 => StatusClass::Informational,
            200..=299 => StatusClass::Success,
            300..=399 => StatusClass::Redirect,
            400..=499 => StatusClass::ClientError,
            500..=599 => StatusClass::ServerError,
            _ => StatusClass::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResponseData {
    pub status: u16,
    pub status_text: String,
    pub version: HttpVersion,
    /// Ordered, exactly as received — duplicates included.
    pub headers: Vec<Header>,
    pub body: Bytes,
    pub timing: Timing,
    pub size: SizeInfo,
}

impl ResponseData {
    pub fn status_class(&self) -> StatusClass {
        StatusClass::of(self.status)
    }

    /// Case-insensitive lookup of the first matching header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// Attempt a zero-copy text view. `None` means the body isn't valid UTF-8,
    /// which is a normal outcome, not an error.
    pub fn body_as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }

    /// A populated response for the M1.0 shell to render. Replaced by the real
    /// engine in M1.2.
    pub fn sample() -> Self {
        let body = Bytes::from_static(
            b"{\n  \"data\": {\n    \"viewer\": {\n      \"login\": \"thearyankumar\",\n      \"repositories\": {\n        \"totalCount\": 42\n      }\n    }\n  }\n}",
        );
        let decoded = body.len() as u64;

        Self {
            status: 200,
            status_text: "OK".to_string(),
            version: HttpVersion::Http2,
            headers: vec![
                Header::new("content-type", "application/json; charset=utf-8"),
                Header::new("content-encoding", "gzip"),
                Header::new("x-ratelimit-remaining", "4998"),
                Header::new("cache-control", "no-cache"),
            ],
            body,
            timing: Timing {
                dns: Some(Duration::from_micros(2_400)),
                connect: Some(Duration::from_millis(31)),
                tls: Some(Duration::from_millis(48)),
                ttfb: Duration::from_millis(126),
                total: Duration::from_millis(142),
            },
            size: SizeInfo { wire: 96, decoded },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classes_cover_the_boundaries() {
        assert_eq!(StatusClass::of(200), StatusClass::Success);
        assert_eq!(StatusClass::of(299), StatusClass::Success);
        assert_eq!(StatusClass::of(300), StatusClass::Redirect);
        assert_eq!(StatusClass::of(404), StatusClass::ClientError);
        assert_eq!(StatusClass::of(503), StatusClass::ServerError);
        assert_eq!(StatusClass::of(0), StatusClass::Unknown);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let response = ResponseData::sample();
        assert_eq!(
            response.header("Content-Type"),
            Some("application/json; charset=utf-8")
        );
    }

    #[test]
    fn non_utf8_bodies_are_not_an_error() {
        let mut response = ResponseData::sample();
        response.body = Bytes::from_static(&[0xff, 0xfe, 0x00]);
        assert!(response.body_as_str().is_none());
    }
}
