//! Turning a `RequestSpec` into a `reqwest::Request`.
//!
//! This is the boundary the model was designed around (architecture.md §3.1): the spec
//! holds a raw URL string that may be invalid mid-keystroke, and *here* is where it
//! either becomes a real `Url` or a typed error naming what's wrong.
//!
//! Split into pure functions so URL resolution, header building, and body preparation
//! are unit-testable without a network or even a live client.

use std::borrow::Cow;

use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Request, Url};

use crate::engine::error::EngineError;
use crate::request::{Body, Method, RequestSpec};

/// A body that has been reduced to bytes, plus the Content-Type it implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedBody {
    None,
    Bytes {
        bytes: Vec<u8>,
        /// Applied only if the request doesn't already carry a Content-Type.
        content_type: Option<&'static str>,
    },
}

/// Find the first unsubstituted `{{variable}}`, if any.
///
/// Applied to the URL and to header names and values — the places where `{{...}}` is
/// unambiguous and where sending the literal text would be actively harmful (a DNS
/// lookup for `{{baseurl}}`, or `Authorization: Bearer {{token}}` going to a server).
/// Deliberately *not* applied to the body: `{{` can occur legitimately inside JSON
/// strings, and a false positive that blocks sending is worse than a literal
/// placeholder in a payload the user can see.
fn find_unresolved_variable(text: &str) -> Option<String> {
    let start = text.find("{{")?;
    let rest = &text[start + 2..];
    let end = rest.find("}}")?;
    Some(rest[..end].trim().to_string())
}

/// Does the string already start with `scheme://`?
fn has_scheme(url: &str) -> bool {
    let Some(colon) = url.find("://") else {
        return false;
    };
    let scheme = &url[..colon];
    !scheme.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Resolve the raw URL text into a real `Url`, merging enabled query params.
///
/// A missing scheme is filled in with `https://` rather than rejected — typing
/// `localhost:3000/health` is normal, and refusing it would be pedantry.
pub fn resolve_url(spec: &RequestSpec) -> Result<Url, EngineError> {
    let raw = spec.url.trim();
    if raw.is_empty() {
        return Err(EngineError::EmptyUrl);
    }

    // Must precede parsing: `Url::parse` treats `{{baseUrl}}` as a valid hostname.
    if let Some(name) = find_unresolved_variable(raw) {
        return Err(EngineError::UnresolvedVariable {
            name,
            location: "the URL".to_string(),
        });
    }

    let candidate: Cow<'_, str> = if has_scheme(raw) {
        Cow::Borrowed(raw)
    } else {
        Cow::Owned(format!("https://{raw}"))
    };

    let mut url = Url::parse(&candidate).map_err(|error| EngineError::InvalidUrl {
        url: raw.to_string(),
        reason: error.to_string(),
    })?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(EngineError::UnsupportedScheme {
            scheme: url.scheme().to_string(),
        });
    }

    // Params from the table are appended on top of anything already written into the
    // URL text, so both places work and neither silently wins.
    let params: Vec<_> = spec
        .enabled_query()
        .filter(|param| !param.name.trim().is_empty())
        .collect();

    if !params.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for param in params {
            pairs.append_pair(param.name.trim(), &param.value);
        }
        pairs.finish();
    }

    // `query_pairs_mut` can leave a bare trailing `?`.
    if url.query() == Some("") {
        url.set_query(None);
    }

    Ok(url)
}

pub fn build_method(method: &Method) -> Result<http::Method, EngineError> {
    http::Method::from_bytes(method.as_str().as_bytes()).map_err(|_| EngineError::InvalidMethod {
        method: method.as_str().to_string(),
    })
}

/// Build the header map from enabled rows.
///
/// Uses `append`, not `insert`, so duplicate names survive — the whole reason the
/// model stores headers as an ordered `Vec`. Rows with a blank name are skipped
/// rather than rejected: "+ add" creates an empty row, and sending shouldn't fail
/// because you haven't filled it in yet.
pub fn build_headers(spec: &RequestSpec) -> Result<HeaderMap, EngineError> {
    let mut headers = HeaderMap::new();

    for header in spec.enabled_headers() {
        let name = header.name.trim();
        if name.is_empty() {
            continue;
        }

        if let Some(variable) = find_unresolved_variable(name)
            .or_else(|| find_unresolved_variable(&header.value))
        {
            return Err(EngineError::UnresolvedVariable {
                name: variable,
                location: format!("header {name}"),
            });
        }

        let header_name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| EngineError::InvalidHeaderName {
                name: name.to_string(),
            })?;
        let header_value =
            HeaderValue::from_str(&header.value).map_err(|_| EngineError::InvalidHeaderValue {
                name: name.to_string(),
                value: header.value.clone(),
            })?;

        headers.append(header_name, header_value);
    }

    Ok(headers)
}

pub fn build_body(spec: &RequestSpec) -> Result<PreparedBody, EngineError> {
    match &spec.body {
        Body::Empty => Ok(PreparedBody::None),

        Body::Raw { text, kind } => {
            if text.trim().is_empty() {
                return Ok(PreparedBody::None);
            }
            Ok(PreparedBody::Bytes {
                bytes: text.as_bytes().to_vec(),
                content_type: Some(kind.content_type()),
            })
        }

        Body::Form(fields) => {
            let encoded = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(
                    fields
                        .iter()
                        .filter(|field| field.enabled && !field.name.trim().is_empty())
                        .map(|field| (field.name.trim(), field.value.as_str())),
                )
                .finish();

            if encoded.is_empty() {
                return Ok(PreparedBody::None);
            }
            Ok(PreparedBody::Bytes {
                bytes: encoded.into_bytes(),
                content_type: Some("application/x-www-form-urlencoded"),
            })
        }

        Body::Binary(path) => {
            let bytes = std::fs::read(path).map_err(|error| EngineError::BodyFileUnreadable {
                path: path.clone(),
                reason: error.to_string(),
            })?;
            // No content type guess — the user sets it explicitly for binary uploads.
            Ok(PreparedBody::Bytes {
                bytes,
                content_type: None,
            })
        }

        // Needs reqwest's `multipart` feature plus streaming file parts. Explicitly
        // unsupported beats silently sending the wrong thing.
        Body::Multipart(_) => Err(EngineError::UnsupportedBody { kind: "multipart" }),
    }
}

/// Compose the pieces into a request ready for `Client::execute`.
pub fn build(client: &Client, spec: &RequestSpec) -> Result<Request, EngineError> {
    let url = resolve_url(spec)?;
    let method = build_method(&spec.method)?;
    let mut headers = build_headers(spec)?;
    let body = build_body(spec)?;

    // An explicit Content-Type always wins; this only fills a gap.
    if let PreparedBody::Bytes {
        content_type: Some(content_type),
        ..
    } = &body
        && !headers.contains_key(CONTENT_TYPE)
    {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    }

    let mut builder = client.request(method, url).headers(headers);

    if let PreparedBody::Bytes { bytes, .. } = body {
        builder = builder.body(bytes);
    }
    if let Some(timeout) = spec.settings.timeout {
        builder = builder.timeout(timeout);
    }

    builder.build().map_err(|error| EngineError::Build {
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{FormField, Header, QueryParam, RawKind};

    fn spec_with_url(url: &str) -> RequestSpec {
        RequestSpec {
            url: url.to_string(),
            ..RequestSpec::default()
        }
    }

    #[test]
    fn empty_url_is_its_own_error() {
        assert_eq!(resolve_url(&spec_with_url("   ")), Err(EngineError::EmptyUrl));
    }

    #[test]
    fn a_missing_scheme_defaults_to_https() {
        let url = resolve_url(&spec_with_url("api.example.com/v1")).unwrap();
        assert_eq!(url.as_str(), "https://api.example.com/v1");
    }

    #[test]
    fn host_port_without_scheme_is_not_mistaken_for_one() {
        // "localhost:3000" has a colon but no "://" — it's a host and port.
        let url = resolve_url(&spec_with_url("localhost:3000/health")).unwrap();
        assert_eq!(url.as_str(), "https://localhost:3000/health");
        assert_eq!(url.port(), Some(3000));
    }

    #[test]
    fn explicit_scheme_is_preserved() {
        let url = resolve_url(&spec_with_url("http://insecure.test/x")).unwrap();
        assert_eq!(url.scheme(), "http");
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        assert_eq!(
            resolve_url(&spec_with_url("ftp://files.test/x")),
            Err(EngineError::UnsupportedScheme {
                scheme: "ftp".to_string()
            })
        );
    }

    #[test]
    fn an_unresolved_url_variable_is_caught_before_dns() {
        // Regression guard: `Url::parse("https://{{baseUrl}}/users")` *succeeds*,
        // reading the placeholder as a hostname. Without the pre-parse check this
        // reached the network and failed with "could not connect to {{baseurl}}".
        assert_eq!(
            resolve_url(&spec_with_url("{{baseUrl}}/users")),
            Err(EngineError::UnresolvedVariable {
                name: "baseUrl".to_string(),
                location: "the URL".to_string(),
            })
        );
    }

    #[test]
    fn an_unresolved_header_variable_never_reaches_a_server() {
        // Sending `Authorization: Bearer {{token}}` literally is worse than failing.
        let mut spec = RequestSpec::default();
        spec.headers = vec![Header::new("Authorization", "Bearer {{token}}")];

        assert_eq!(
            build_headers(&spec),
            Err(EngineError::UnresolvedVariable {
                name: "token".to_string(),
                location: "header Authorization".to_string(),
            })
        );
    }

    #[test]
    fn a_disabled_row_with_a_variable_does_not_block_sending() {
        // The sample request ships exactly this: a disabled Authorization header
        // holding a placeholder. It must not stop an otherwise valid request.
        let mut spec = RequestSpec::default();
        spec.headers = vec![
            Header::new("Accept", "application/json"),
            Header::disabled("Authorization", "Bearer {{token}}"),
        ];
        assert!(build_headers(&spec).is_ok());
    }

    #[test]
    fn braces_in_a_body_are_left_alone() {
        // `{{` inside JSON must not be mistaken for a template.
        let mut spec = RequestSpec::default();
        spec.body = Body::Raw {
            text: "{\"nested\":{{\"a\":1}}}".into(),
            kind: RawKind::Json,
        };
        assert!(matches!(build_body(&spec), Ok(PreparedBody::Bytes { .. })));
    }

    #[test]
    fn query_params_merge_with_params_already_in_the_url() {
        let mut spec = spec_with_url("https://x.test/search?q=rust");
        spec.query = vec![
            QueryParam::new("page", "2"),
            QueryParam {
                enabled: false,
                name: "debug".into(),
                value: "1".into(),
            },
        ];

        let url = resolve_url(&spec).unwrap();
        let pairs: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        assert_eq!(
            pairs,
            vec![
                ("q".to_string(), "rust".to_string()),
                ("page".to_string(), "2".to_string()),
            ],
            "disabled params must not be sent"
        );
    }

    #[test]
    fn no_query_params_leaves_no_trailing_question_mark() {
        let url = resolve_url(&spec_with_url("https://x.test/path")).unwrap();
        assert_eq!(url.as_str(), "https://x.test/path");
    }

    #[test]
    fn duplicate_headers_both_survive() {
        let mut spec = RequestSpec::default();
        spec.headers = vec![
            Header::new("Set-Cookie", "a=1"),
            Header::new("Set-Cookie", "b=2"),
        ];

        let headers = build_headers(&spec).unwrap();
        let values: Vec<_> = headers
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(values, vec!["a=1", "b=2"]);
    }

    #[test]
    fn disabled_and_blank_header_rows_are_skipped_not_rejected() {
        let mut spec = RequestSpec::default();
        spec.headers = vec![
            Header::new("Accept", "application/json"),
            Header::disabled("Authorization", "Bearer x"),
            // The row "+ add" creates before you type anything.
            Header::new("", ""),
        ];

        let headers = build_headers(&spec).unwrap();
        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key("accept"));
    }

    #[test]
    fn an_invalid_header_name_names_itself() {
        let mut spec = RequestSpec::default();
        spec.headers = vec![Header::new("has space", "v")];

        assert_eq!(
            build_headers(&spec),
            Err(EngineError::InvalidHeaderName {
                name: "has space".to_string()
            })
        );
    }

    #[test]
    fn raw_json_body_carries_its_content_type() {
        let mut spec = RequestSpec::default();
        spec.body = Body::Raw {
            text: "{\"a\":1}".into(),
            kind: RawKind::Json,
        };

        assert_eq!(
            build_body(&spec).unwrap(),
            PreparedBody::Bytes {
                bytes: b"{\"a\":1}".to_vec(),
                content_type: Some("application/json"),
            }
        );
    }

    #[test]
    fn a_whitespace_only_raw_body_sends_nothing() {
        let mut spec = RequestSpec::default();
        spec.body = Body::Raw {
            text: "  \n ".into(),
            kind: RawKind::Json,
        };
        assert_eq!(build_body(&spec).unwrap(), PreparedBody::None);
    }

    #[test]
    fn form_bodies_are_urlencoded_and_skip_disabled_fields() {
        let mut spec = RequestSpec::default();
        spec.body = Body::Form(vec![
            FormField {
                enabled: true,
                name: "name".into(),
                value: "a b".into(),
            },
            FormField {
                enabled: false,
                name: "secret".into(),
                value: "x".into(),
            },
        ]);

        let PreparedBody::Bytes { bytes, content_type } = build_body(&spec).unwrap() else {
            panic!("expected bytes");
        };
        assert_eq!(String::from_utf8(bytes).unwrap(), "name=a+b");
        assert_eq!(content_type, Some("application/x-www-form-urlencoded"));
    }

    #[test]
    fn multipart_is_explicitly_unsupported_rather_than_silently_wrong() {
        let mut spec = RequestSpec::default();
        spec.body = Body::Multipart(vec![]);
        assert_eq!(
            build_body(&spec),
            Err(EngineError::UnsupportedBody { kind: "multipart" })
        );
    }

    #[test]
    fn an_explicit_content_type_is_not_overridden() {
        let client = Client::new();
        let mut spec = spec_with_url("https://x.test/");
        spec.method = Method::Post;
        spec.headers = vec![Header::new("content-type", "application/vnd.custom+json")];
        spec.body = Body::Raw {
            text: "{}".into(),
            kind: RawKind::Json,
        };

        let request = build(&client, &spec).unwrap();
        assert_eq!(
            request.headers().get(CONTENT_TYPE).unwrap(),
            "application/vnd.custom+json"
        );
    }

    #[test]
    fn a_missing_content_type_is_filled_in_from_the_body_kind() {
        let client = Client::new();
        let mut spec = spec_with_url("https://x.test/");
        spec.method = Method::Post;
        spec.body = Body::Raw {
            text: "{}".into(),
            kind: RawKind::Json,
        };

        let request = build(&client, &spec).unwrap();
        assert_eq!(
            request.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn custom_methods_are_sendable() {
        let method = build_method(&Method::Other("PROPFIND".into())).unwrap();
        assert_eq!(method.as_str(), "PROPFIND");
    }

    #[test]
    fn nonsense_methods_are_rejected() {
        assert!(build_method(&Method::Other("bad method".into())).is_err());
    }
}
