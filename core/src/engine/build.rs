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
use crate::request::{
    Body, FormField, GraphQlRequest, HttpRequest, Method, MultipartValue, RequestKind,
    RequestSpec, WebSocketRequest,
};

/// A body that has been reduced to bytes, plus the Content-Type it implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedBody {
    None,
    Bytes {
        bytes: Vec<u8>,
        /// Applied only if the request doesn't already carry a Content-Type.
        content_type: Option<&'static str>,
    },
    /// Parts already read into memory, for `build` to hand to reqwest.
    ///
    /// Deliberately *not* a `reqwest::multipart::Form`: that type is neither `Debug`,
    /// `Clone`, nor `PartialEq`, so holding one here would cost this enum its derives and
    /// make multipart the only body that can't be asserted on in a unit test. Keeping the
    /// parts as plain data means file reading (and `BodyFileUnreadable`) stays in
    /// `build_body` with every other body, and reqwest stays confined to `build`.
    Multipart(Vec<PreparedPart>),
}

/// One part of a multipart body, already reduced to bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPart {
    pub name: String,
    /// `None` for a text field. A file part carries the name servers key off.
    pub filename: Option<String>,
    pub bytes: Vec<u8>,
}

/// Find the first unsubstituted `{{variable}}`, if any.
///
/// Applied to the URL and to header names and values — the places where `{{...}}` is
/// unambiguous and where sending the literal text would be actively harmful (a DNS
/// lookup for `{{baseurl}}`, or `Authorization: Bearer {{token}}` going to a server).
/// Deliberately *not* applied to the body: `{{` can occur legitimately inside JSON
/// strings, and a false positive that blocks sending is worse than a literal
/// placeholder in a payload the user can see.
///
/// **A WebSocket frame is a body by that reasoning**, so `send_frame` warns rather than
/// refuses — see its comment. That is what this is `pub` for.
pub fn find_unresolved_variable(text: &str) -> Option<String> {
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
    // A kind with no query table contributes none — the endpoint is then whatever the URL
    // text says, which is the honest reading for a protocol that has no such table.
    let params: Vec<_> = spec
        .http()
        .into_iter()
        .flat_map(HttpRequest::enabled_query)
        .filter(|param| !param.name.trim().is_empty())
        .collect();

    // Checked here rather than trusted: the URL and header checks predate query rows
    // existing as a separate table, so an unsubstituted `{{var}}` in a parameter used to
    // reach the wire literally — silently sending `search={{q}}` to a real server. Unlike a
    // body, `{{` in a query value is a variable and nothing else, so this can be strict.
    for param in &params {
        if let Some(name) = find_unresolved_variable(&param.name)
            .or_else(|| find_unresolved_variable(&param.value))
        {
            return Err(EngineError::UnresolvedVariable {
                name,
                location: format!("the query parameter {:?}", param.name.trim()),
            });
        }
    }

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

/// A form body's wire bytes: `a=1&b=2`, percent-encoded, disabled and unnamed fields dropped.
///
/// **Public because curl export needs the exact same string.** It emits the encoded form as one
/// `--data-raw`, so that a copied command sends byte-for-byte what Zuno sends — and the alternative
/// there, one `--data-urlencode` per field, would have let *curl* do the encoding instead, which
/// differs for a field whose **name** needs escaping (curl only encodes after the `=`). Sharing the
/// function is what keeps the two from drifting; `exported_form_body_matches_the_wire` pins it.
pub fn encode_form(fields: &[FormField]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            fields
                .iter()
                .filter(|field| field.enabled && !field.name.trim().is_empty())
                .map(|field| (field.name.trim(), field.value.as_str())),
        )
        .finish()
}

pub fn build_body(spec: &RequestSpec) -> Result<PreparedBody, EngineError> {
    // Exhaustive on the kind, with no catch-all: a new kind carries its own payload and
    // must decide how it reaches the wire rather than silently sending nothing.
    let http = match &spec.kind {
        RequestKind::Http(http) => http,
        // The envelope is built by `graphql_envelope` and attached in `build_graphql`, which
        // also decides whether it travels as a body at all — a GET sends it in the query
        // string, so "the body" is genuinely nothing here rather than merely empty.
        RequestKind::GraphQl(_) => return Ok(PreparedBody::None),
        // The handshake carries no body at all — RFC 6455 forbids one on the GET.
        RequestKind::WebSocket(_) => return Ok(PreparedBody::None),
        // A gRPC body is a framed protobuf message, and producing one needs the compiled
        // schema — which this function has no way to reach. `build_grpc` takes the encoded
        // bytes from the caller that does. Genuinely nothing here, not merely empty.
        RequestKind::Grpc(_) => return Ok(PreparedBody::None),
    };
    match &http.body {
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
            let encoded = encode_form(fields);

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

        Body::Multipart(fields) => {
            let mut parts = Vec::new();
            for field in fields
                .iter()
                .filter(|field| field.enabled && !field.name.trim().is_empty())
            {
                let part = match &field.value {
                    MultipartValue::Text(text) => PreparedPart {
                        name: field.name.trim().to_string(),
                        filename: None,
                        bytes: text.clone().into_bytes(),
                    },
                    MultipartValue::File(path) => {
                        // Read here rather than streamed, matching the binary body: the
                        // whole file enters memory. Fine for the uploads an API client
                        // sees; if that stops being true, `Part::stream` is the upgrade.
                        let bytes =
                            std::fs::read(path).map_err(|error| EngineError::BodyFileUnreadable {
                                path: path.clone(),
                                reason: error.to_string(),
                            })?;
                        PreparedPart {
                            name: field.name.trim().to_string(),
                            // Servers routinely key off the filename, and a part without
                            // one reads as a text field to many frameworks.
                            filename: Some(
                                path.file_name()
                                    .map(|name| name.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "file".to_string()),
                            ),
                            bytes,
                        }
                    }
                };
                parts.push(part);
            }

            // Same rule as a form body with no usable fields: nothing to send.
            if parts.is_empty() {
                return Ok(PreparedBody::None);
            }
            Ok(PreparedBody::Multipart(parts))
        }
    }
}

/// Compose the pieces into a request ready for `Client::execute`.
pub fn build(client: &Client, spec: &RequestSpec) -> Result<Request, EngineError> {
    match &spec.kind {
        RequestKind::Http(http) => build_http(client, spec, http),
        RequestKind::GraphQl(graphql) => build_graphql(client, spec, graphql),
        // **Not built here.** The handshake needs a `Sec-WebSocket-Key` that the *caller*
        // has to keep in order to check the server's `Sec-WebSocket-Accept` against it, and
        // a builder that generates one internally would throw away the only thing that
        // makes the reply verifiable. `session::connect` calls `build_websocket` directly.
        RequestKind::WebSocket(_) => Err(EngineError::Other {
            reason: "a WebSocket is opened, not sent".to_string(),
        }),
        // **Not built here either, and for a sharper reason.** A gRPC body is a protobuf
        // message, and encoding one needs the compiled `.proto` — which is on disk, is shared
        // between requests, and is not reachable from a `RequestSpec`. `build_grpc` takes the
        // already-encoded bytes from the caller that compiled the schema.
        RequestKind::Grpc(_) => Err(EngineError::Other {
            reason: "a gRPC call needs its schema, so it is built by the gRPC path".to_string(),
        }),
    }
}

/// The JSON envelope a GraphQL request sends: `query`, plus `variables` and `operationName`
/// when there are any.
///
/// **Absent rather than empty for the two optional members.** A server is entitled to reject
/// `"operationName": ""` or `"variables": null`, and several do; omitting a member it does not
/// need is what every GraphQL client does and what the spec's transport note describes.
///
/// `variables` is parsed here rather than held parsed, so the editor can carry text that is
/// invalid mid-keystroke (§3.1). It must be a JSON **object**: the spec defines it as a map,
/// and a bare array or number is a mistake worth naming rather than forwarding.
pub fn graphql_envelope(graphql: &GraphQlRequest) -> Result<serde_json::Value, EngineError> {
    if graphql.query.trim().is_empty() {
        return Err(EngineError::EmptyGraphQlQuery);
    }

    let mut envelope = serde_json::Map::new();
    envelope.insert("query".to_string(), graphql.query.clone().into());

    let variables = graphql.variables.trim();
    if !variables.is_empty() {
        let parsed: serde_json::Value = serde_json::from_str(variables).map_err(|error| {
            EngineError::InvalidGraphQlVariables {
                reason: error.to_string(),
            }
        })?;
        if !parsed.is_object() {
            return Err(EngineError::InvalidGraphQlVariables {
                reason: "expected a JSON object, like {\"id\": 1}".to_string(),
            });
        }
        envelope.insert("variables".to_string(), parsed);
    }

    if let Some(operation) = graphql.operation.as_ref().map(|name| name.trim())
        && !operation.is_empty()
    {
        envelope.insert("operationName".to_string(), operation.into());
    }

    Ok(serde_json::Value::Object(envelope))
}

/// Compose a GraphQL request.
///
/// **A GET carries the envelope in the query string, not the body.** That is the whole reason
/// the method is variable here: a GET GraphQL request is cacheable by ordinary HTTP machinery,
/// and a body on a GET is ignored by enough intermediaries that sending one would fail in ways
/// nobody could debug.
pub fn graphql_carries_a_body(graphql: &GraphQlRequest) -> bool {
    !matches!(graphql.method, Method::Get | Method::Head)
}

/// The URL a GraphQL request is actually sent to — which for a GET carries the whole envelope
/// in the query string.
///
/// **Shared with curl export rather than done twice.** `to_command` has to reproduce what goes
/// on the wire, and a GET whose envelope lived only inside `build_graphql` would export a bare
/// endpoint that returns an error when pasted — the copied command has to be runnable, which is
/// the whole point of the feature.
pub fn graphql_url(spec: &RequestSpec, graphql: &GraphQlRequest) -> Result<Url, EngineError> {
    let mut url = resolve_url(spec)?;
    if graphql_carries_a_body(graphql) {
        return Ok(url);
    }

    let envelope = graphql_envelope(graphql)?;
    let mut pairs = url.query_pairs_mut();
    pairs.append_pair("query", &graphql.query);
    if let Some(variables) = envelope.get("variables") {
        pairs.append_pair("variables", &variables.to_string());
    }
    if let Some(operation) = envelope.get("operationName").and_then(|v| v.as_str()) {
        pairs.append_pair("operationName", operation);
    }
    pairs.finish();
    drop(pairs);

    Ok(url)
}

fn build_graphql(
    client: &Client,
    spec: &RequestSpec,
    graphql: &GraphQlRequest,
) -> Result<Request, EngineError> {
    let envelope = graphql_envelope(graphql)?;
    let url = graphql_url(spec, graphql)?;
    let method = build_method(&graphql.method)?;
    let headers = build_headers(spec)?;

    let carries_body = graphql_carries_a_body(graphql);

    let mut builder = client.request(method, url).headers(headers.clone());

    if carries_body {
        // An explicit Content-Type still wins, as it does for every body except multipart —
        // a gateway that wants `application/graphql+json` is entitled to say so.
        if !headers.contains_key(CONTENT_TYPE) {
            builder = builder.header(CONTENT_TYPE, "application/json");
        }
        builder = builder.body(
            serde_json::to_vec(&envelope)
                .map_err(|error| EngineError::Build { reason: error.to_string() })?,
        );
    }

    builder
        .build()
        .map_err(|error| EngineError::Build { reason: error.to_string() })
}

/// Compose an HTTP request. Split from `build` so that the kind is matched exactly once,
/// where adding one is a compile error rather than a silently skipped branch.
fn build_http(
    client: &Client,
    spec: &RequestSpec,
    http: &HttpRequest,
) -> Result<Request, EngineError> {
    let url = resolve_url(spec)?;
    let method = build_method(&http.method)?;
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

    match body {
        PreparedBody::Bytes { bytes, .. } => builder = builder.body(bytes),
        PreparedBody::Multipart(parts) => {
            let mut form = reqwest::multipart::Form::new();
            for part in parts {
                let mut piece = reqwest::multipart::Part::bytes(part.bytes);
                if let Some(filename) = part.filename {
                    piece = piece.file_name(filename);
                }
                form = form.part(part.name, piece);
            }
            // **Unlike every other body, an explicit Content-Type cannot win here.**
            // `multipart` generates a boundary and writes the header itself, and a
            // user-supplied `multipart/form-data` without that boundary is unparseable —
            // so overriding it would produce a request no server can read.
            builder = builder.multipart(form);
        }
        PreparedBody::None => {}
    }
    // **No `builder.timeout` here, deliberately.** reqwest's request timeout is a deadline on
    // the *whole* exchange, body included, which a stream can never meet — a `text/event-stream`
    // answered by an HTTP request died at whatever the setting said, with no close and nothing
    // on screen explaining it.
    //
    // **Only this builder ever set one**, which is worth saying because the symptom looked
    // wider than it was: `build_graphql` has never carried a timeout, so a graphql-sse
    // subscription was never killed this way and whatever ended one early had another cause.
    // `timeout` now means "answer within N, and do not go silent for N": `run::execute` puts
    // the first half on the response head itself, and `ClientKey::read_timeout` puts the second
    // half on each read. See `ClientKey`.

    builder.build().map_err(|error| EngineError::Build {
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{FormField, Header, MultipartField, QueryParam, RawKind};

    /// **A scheme-less gRPC address is plaintext only on loopback**, and the narrowness is the
    /// point: the wide version would send credentials out readable.
    ///
    /// The failure this prevents is silent in both directions — a TLS client against a plaintext
    /// server gets rustls complaining about a corrupt message, and a plaintext client against a
    /// TLS one gets no error naming the version at all.
    #[test]
    fn a_scheme_less_grpc_address_is_plaintext_only_on_loopback() {
        let path = "/pkg.Svc/Method";
        let url = |raw: &str| grpc_url(&spec_with_url(raw), path).expect("url");

        // Local development, which is essentially never TLS.
        assert_eq!(url("localhost:50051").scheme(), "http");
        assert_eq!(url("127.0.0.1:50051").scheme(), "http");
        assert_eq!(url("[::1]:50051").scheme(), "http");
        assert_eq!(url("LocalHost:50051").scheme(), "http");

        // Anything else keeps the secure default. A public plaintext server — there are some,
        // including on 443 — needs `http://` typed, and the connect error now says so.
        assert_eq!(url("grpc.example.com:443").scheme(), "https");
        assert_eq!(url("10.0.0.5:50051").scheme(), "https");

        // **A typed scheme is always obeyed**, which is what makes the rule a default rather
        // than a policy: it must be possible to say plainly what you meant.
        assert_eq!(url("http://grpc.example.com:443").scheme(), "http");
        assert_eq!(url("https://localhost:50051").scheme(), "https");

        // And the method's path replaces whatever was typed, since gRPC routes on it alone.
        assert_eq!(url("localhost:50051/ignored").path(), path);
    }

    /// **A typed `Content-Type` is replaced, not joined.** gRPC's pane calls headers Metadata,
    /// which invites exactly this entry, and two content types reach the server as one joined
    /// value that nothing routes.
    #[test]
    fn grpc_metadata_cannot_duplicate_a_protocol_header() {
        let mut spec = spec_with_url("http://127.0.0.1:1");
        spec.headers = vec![
            Header::new("Content-Type", "application/json"),
            Header::new("TE", "gzip"),
            Header::new("authorization", "Bearer x"),
        ];

        let headers = grpc_headers(&spec).expect("headers");
        let all = |name: &str| {
            headers
                .get_all(name)
                .iter()
                .map(|value| value.to_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(all("content-type"), vec!["application/grpc+proto"]);
        assert_eq!(all("te"), vec!["trailers"]);
        assert_eq!(all("authorization"), vec!["Bearer x"], "the person's own metadata stays");
    }

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
    fn an_unresolved_query_variable_never_reaches_a_server() {
        // The gap: URL and header checks existed, query rows had none, so `search={{q}}`
        // was sent verbatim.
        let mut spec = RequestSpec {
            url: "https://api.test/search".to_string(),
            ..RequestSpec::default()
        };
        spec.http_mut().unwrap().query = vec![QueryParam::new("search", "{{q}}")];

        let error = resolve_url(&spec).expect_err("must refuse to send");
        assert!(
            matches!(&error, EngineError::UnresolvedVariable { name, .. } if name == "q"),
            "{error:?}"
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
        spec.http_mut().unwrap().body = Body::Raw {
            text: "{\"nested\":{{\"a\":1}}}".into(),
            kind: RawKind::Json,
        };
        assert!(matches!(build_body(&spec), Ok(PreparedBody::Bytes { .. })));
    }

    #[test]
    fn query_params_merge_with_params_already_in_the_url() {
        let mut spec = spec_with_url("https://x.test/search?q=rust");
        spec.http_mut().unwrap().query = vec![
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
        spec.http_mut().unwrap().body = Body::Raw {
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
        spec.http_mut().unwrap().body = Body::Raw {
            text: "  \n ".into(),
            kind: RawKind::Json,
        };
        assert_eq!(build_body(&spec).unwrap(), PreparedBody::None);
    }

    #[test]
    fn form_bodies_are_urlencoded_and_skip_disabled_fields() {
        let mut spec = RequestSpec::default();
        spec.http_mut().unwrap().body = Body::Form(vec![
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
        spec.http_mut().unwrap().body = Body::Multipart(vec![]);
        // Same rule as a form with no usable fields: nothing to send, rather than an empty
        // multipart envelope with a boundary and no parts.
        assert_eq!(build_body(&spec).unwrap(), PreparedBody::None);
    }

    #[test]
    fn multipart_text_and_file_parts_are_both_prepared() {
        let dir = std::env::temp_dir().join(format!("zuno-mp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("avatar.png");
        std::fs::write(&file, b"PNGDATA").expect("write");

        let mut spec = RequestSpec::default();
        spec.http_mut().unwrap().body = Body::Multipart(vec![
            MultipartField {
                enabled: true,
                name: "caption".into(),
                value: MultipartValue::Text("hello".into()),
            },
            MultipartField {
                enabled: true,
                name: "avatar".into(),
                value: MultipartValue::File(file.clone()),
            },
            // Disabled and blank-named parts are dropped, as in every other table.
            MultipartField {
                enabled: false,
                name: "skipped".into(),
                value: MultipartValue::Text("no".into()),
            },
            MultipartField {
                enabled: true,
                name: "   ".into(),
                value: MultipartValue::Text("nameless".into()),
            },
        ]);

        let PreparedBody::Multipart(parts) = build_body(&spec).unwrap() else {
            panic!("expected multipart");
        };
        assert_eq!(parts.len(), 2, "{parts:?}");

        assert_eq!(parts[0].name, "caption");
        assert_eq!(parts[0].filename, None, "a text part has no filename");
        assert_eq!(parts[0].bytes, b"hello");

        assert_eq!(parts[1].name, "avatar");
        assert_eq!(
            parts[1].filename.as_deref(),
            Some("avatar.png"),
            "a file part carries the name servers key off"
        );
        assert_eq!(parts[1].bytes, b"PNGDATA");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_multipart_file_is_reported_by_path() {
        let missing = std::env::temp_dir().join("zuno-no-such-part.bin");
        let mut spec = RequestSpec::default();
        spec.http_mut().unwrap().body = Body::Multipart(vec![MultipartField {
            enabled: true,
            name: "avatar".into(),
            value: MultipartValue::File(missing.clone()),
        }]);

        let error = build_body(&spec).expect_err("must refuse to send");
        assert!(
            matches!(&error, EngineError::BodyFileUnreadable { path, .. } if *path == missing),
            "{error:?}"
        );
    }

    #[test]
    fn an_explicit_content_type_is_not_overridden() {
        let client = Client::new();
        let mut spec = spec_with_url("https://x.test/");
        spec.http_mut().unwrap().method = Method::Post;
        spec.headers = vec![Header::new("content-type", "application/vnd.custom+json")];
        spec.http_mut().unwrap().body = Body::Raw {
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
        spec.http_mut().unwrap().method = Method::Post;
        spec.http_mut().unwrap().body = Body::Raw {
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

    fn graphql_spec(query: &str) -> RequestSpec {
        RequestSpec {
            url: "https://api.test/graphql".to_string(),
            kind: RequestKind::GraphQl(GraphQlRequest {
                query: query.to_string(),
                ..GraphQlRequest::default()
            }),
            ..RequestSpec::default()
        }
    }

    #[test]
    fn a_graphql_envelope_omits_what_it_does_not_have() {
        // Absent, not empty: a server is entitled to reject `"operationName": ""`, and several
        // do. Asserted on the key set rather than on a value, because writing `null` would pass
        // any assertion that only looked at what `query` contained.
        let spec = graphql_spec("{ viewer { login } }");
        let graphql = spec.graphql().expect("a GraphQL request");
        let envelope = graphql_envelope(graphql).expect("an envelope");

        let keys: Vec<&str> = envelope.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["query"], "variables and operationName must be absent, not empty");
    }

    #[test]
    fn a_graphql_envelope_carries_variables_as_json_not_as_text() {
        let mut spec = graphql_spec("query R($n: Int!) { repos(first: $n) { id } }");
        let graphql = spec.graphql_mut().expect("GraphQL");
        graphql.variables = r#"{"n": 50}"#.to_string();
        graphql.operation = Some("R".to_string());

        let envelope = graphql_envelope(spec.graphql().unwrap()).expect("an envelope");
        // The number must arrive as a number. Sending `"50"` is the mistake a naive
        // string-concatenated envelope makes, and a server rejects it against an `Int!`.
        assert_eq!(envelope["variables"]["n"], serde_json::json!(50));
        assert_eq!(envelope["operationName"], "R");
    }

    #[test]
    fn graphql_variables_that_are_not_an_object_are_named_rather_than_forwarded() {
        let mut spec = graphql_spec("{ a }");
        spec.graphql_mut().unwrap().variables = "[1, 2]".to_string();
        assert!(matches!(
            graphql_envelope(spec.graphql().unwrap()),
            Err(EngineError::InvalidGraphQlVariables { .. })
        ));

        spec.graphql_mut().unwrap().variables = "{oops".to_string();
        assert!(matches!(
            graphql_envelope(spec.graphql().unwrap()),
            Err(EngineError::InvalidGraphQlVariables { .. })
        ));
    }

    #[test]
    fn an_empty_graphql_query_is_refused_rather_than_sent() {
        // An empty *body* is legal HTTP; an empty GraphQL document is not a request at all.
        let spec = graphql_spec("   ");
        assert!(matches!(
            graphql_envelope(spec.graphql().unwrap()),
            Err(EngineError::EmptyGraphQlQuery)
        ));
    }

    /// **The whole reason `method` is variable on a GraphQL request.** A GET carries the
    /// envelope in the query string; a body on a GET is dropped by enough intermediaries that
    /// sending one fails in ways nobody can debug.
    #[test]
    fn a_get_graphql_request_puts_the_envelope_in_the_url_and_a_post_does_not() {
        let mut spec = graphql_spec("query R { a }");
        spec.graphql_mut().unwrap().variables = r#"{"n":1}"#.to_string();

        spec.graphql_mut().unwrap().method = Method::Post;
        let posted = graphql_url(&spec, spec.graphql().unwrap()).expect("a URL");
        assert_eq!(posted.query(), None, "a POST sends the envelope as a body");
        assert!(build_body(&spec).is_ok());

        spec.graphql_mut().unwrap().method = Method::Get;
        let got = graphql_url(&spec, spec.graphql().unwrap()).expect("a URL");
        let query = got.query().expect("a GET must carry the envelope in the URL");
        assert!(query.contains("query="), "got {query}");
        assert!(query.contains("variables="), "got {query}");
    }

    /// `build_body` returning `None` for GraphQL is deliberate — the envelope is attached by
    /// `build_graphql`, which also decides whether it travels as a body at all. Pinned so that
    /// "GraphQL sends no body" cannot be read as "GraphQL bodies are unimplemented".
    #[test]
    fn graphql_does_not_go_through_the_http_body_path() {
        assert_eq!(build_body(&graphql_spec("{ a }")).unwrap(), PreparedBody::None);
    }
}

/// The handshake URL: `ws`/`wss` mapped onto the schemes reqwest can actually resolve.
///
/// **RFC 6455's own mapping**, and it has to happen here rather than being typed by the user:
/// `ws://` is the scheme every WebSocket document uses and the one people paste, and reqwest
/// cannot resolve it at all — it is not an HTTP scheme, so `resolve_url` rejects it outright.
/// The handshake underneath *is* ordinary HTTP, with the same default ports.
///
/// Separate from `resolve_url` rather than a flag on it because a socket has no query table to
/// merge: `WebSocketRequest` carries no `QueryParam`s, so half of that function would be
/// answering a question this kind does not ask.
pub fn websocket_url(spec: &RequestSpec) -> Result<Url, EngineError> {
    let raw = spec.url.trim();
    if raw.is_empty() {
        return Err(EngineError::EmptyUrl);
    }
    // Before parsing, for `resolve_url`'s reason: `Url::parse` reads `{{baseUrl}}` as a host.
    if let Some(name) = find_unresolved_variable(raw) {
        return Err(EngineError::UnresolvedVariable {
            name,
            location: "the URL".to_string(),
        });
    }

    let candidate = match raw.split_once("://") {
        Some(("ws", rest)) => format!("http://{rest}"),
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(_) => raw.to_string(),
        // No scheme typed. `wss` is the default rather than `ws` for the same reason
        // `resolve_url` defaults to `https`.
        None => format!("https://{raw}"),
    };

    let url = Url::parse(&candidate).map_err(|error| EngineError::InvalidUrl {
        url: raw.to_string(),
        reason: error.to_string(),
    })?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(EngineError::UnsupportedScheme {
            scheme: url.scheme().to_string(),
        });
    }

    Ok(url)
}

/// The opening GET, with the six headers that turn it into an upgrade.
///
/// `key` is passed in rather than generated here, and that is the whole reason this is not a
/// `build` arm: the caller has to keep it to check the server's `Sec-WebSocket-Accept` against
/// it afterwards. A builder that made its own would leave the reply unverifiable.
pub fn build_websocket(
    spec: &RequestSpec,
    socket: &WebSocketRequest,
    key: &str,
) -> Result<Request, EngineError> {
    use http::header::{CONNECTION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE};

    let mut request = Request::new(http::Method::GET, websocket_url(spec)?);

    // The user's own headers first, so the upgrade set below cannot be overwritten by one of
    // them — a typed `Connection: keep-alive` would otherwise quietly break the handshake.
    let headers = request.headers_mut();
    *headers = build_headers(spec)?;
    headers.insert(CONNECTION, http::HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, http::HeaderValue::from_static("websocket"));
    headers.insert(SEC_WEBSOCKET_VERSION, http::HeaderValue::from_static("13"));
    headers.insert(
        SEC_WEBSOCKET_KEY,
        http::HeaderValue::from_str(key).map_err(|_| EngineError::InvalidHeaderValue {
            name: "sec-websocket-key".to_string(),
            value: key.to_string(),
        })?,
    );

    let offered: Vec<&str> = socket
        .subprotocols
        .iter()
        .map(|protocol| protocol.trim())
        .filter(|protocol| !protocol.is_empty())
        .collect();
    if !offered.is_empty() {
        let joined = offered.join(", ");
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            http::HeaderValue::from_str(&joined).map_err(|_| EngineError::InvalidHeaderValue {
                name: "sec-websocket-protocol".to_string(),
                value: joined.clone(),
            })?,
        );
    }

    // **HTTP/1.1, pinned, not negotiated.** There is no 101 in HTTP/2 — the upgrade mechanism
    // was replaced by extended CONNECT — so a `wss://` URL whose ALPN happens to settle on h2
    // would produce a handshake that can never succeed, with nothing on screen saying why.
    // `tests/websocket_upgrade.rs` pins that the request goes out as 1.1.
    *request.version_mut() = http::Version::HTTP_11;

    Ok(request)
}

/// The endpoint a gRPC call is made against, with the method's path replacing whatever the URL
/// text carried.
///
/// **gRPC routes entirely on `:path`**, and that path is `/package.Service/Method` — there is no
/// query string, no verb and no content negotiation. So the URL a person types is only ever an
/// *origin*: a host, a port, and a scheme. Anything they left in the path is discarded rather
/// than joined, because a base of `http://host/v1` plus a method would produce
/// `/v1/pkg.Service/Method`, which no gRPC server routes.
pub fn grpc_url(spec: &RequestSpec, path: &str) -> Result<Url, EngineError> {
    let mut url = resolve_url(spec)?;
    url.set_path(path);
    url.set_query(None);

    // **A scheme-less loopback address means plaintext**, and only loopback.
    //
    // `resolve_url` fills in `https://`, which is the right default for HTTP — you are usually
    // calling a public API — and the wrong one for a gRPC server on your own machine, which is
    // essentially never TLS. Getting it wrong is not a polite failure either: the server's
    // HTTP/2 preface arrives where rustls expects a TLS record and the error names neither the
    // cause nor the fix.
    //
    // Narrowed to loopback on purpose. Defaulting *everything* to plaintext would be a security
    // regression — credentials typed into a request would go out readable, and the whole reason
    // a secure default is right is that the insecure one fails silently in the safe direction.
    // On loopback there is no path to expose them on. A public plaintext server, such as
    // `grpc.postman-echo.com`, still needs `http://` typed, which the error now says.
    if !has_scheme(spec.url.trim()) && is_loopback(url.host_str()) {
        // Infallible for a known scheme on a URL that already parsed.
        let _ = url.set_scheme("http");
    }

    Ok(url)
}

/// Whether a host is this machine, by name or by address.
///
/// Name as well as address because `localhost` is what people type; the numeric forms are what
/// a container or a compose file tends to produce.
fn is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // **`Url::host_str` keeps the brackets on an IPv6 literal** — `[::1]`, not `::1` — and
    // `IpAddr` will not parse that, so `::1` read as "not loopback" and a local IPv6 gRPC server
    // got TLS. Written here first as a comment claiming the opposite; the test caught it.
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// Build one gRPC call from an already-encoded message.
///
/// **The bytes come in rather than being built here**, because encoding needs the compiled
/// `.proto` — which lives on disk, is shared between requests, and is not reachable from a
/// `RequestSpec`. Keeping the schema out of this function is what lets it stay pure and testable
/// the way every other builder here is.
pub fn build_grpc(
    client: &Client,
    spec: &RequestSpec,
    grpc: &crate::request::GrpcRequest,
    message: &[u8],
) -> Result<Request, EngineError> {
    build_grpc_body(client, spec, grpc, crate::grpc::frame(message).into())
}

/// The same, with a body that stays open.
///
/// **What client-streaming actually needs.** A unary call's body is a fixed run of bytes; a
/// client-streaming one is a body that accepts more messages for as long as the person keeps
/// sending, and ends when they say so. `reqwest::Body::wrap_stream` is what makes that a body at
/// all, and HTTP/2 is what makes it work — the request headers go out immediately and DATA
/// frames follow, which is exactly the shape gRPC was designed around.
pub fn build_grpc_body(
    client: &Client,
    spec: &RequestSpec,
    grpc: &crate::request::GrpcRequest,
    body: reqwest::Body,
) -> Result<Request, EngineError> {
    if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
        return Err(EngineError::Other {
            reason: "choose a service and a method before sending".to_string(),
        });
    }

    let path = format!("/{}/{}", grpc.service.trim(), grpc.method.trim());
    let url = grpc_url(spec, &path)?;

    client
        .post(url)
        .headers(grpc_headers(spec)?)
        .body(body)
        .build()
        .map_err(|error| EngineError::Build {
            reason: error.to_string(),
        })
}

/// The person's metadata plus the three headers the protocol requires.
///
/// **`insert`, which replaces, and not `RequestBuilder::header`, which appends.** gRPC calls
/// these *metadata* and they are ordinary HTTP/2 headers, so someone can type a `content-type`
/// into them — and appending ours after theirs sent both, which HTTP/2 joins into
/// `application/json, application/grpc+proto` and no server routes. Shared with reflection so
/// the two cannot disagree about it.
pub fn grpc_headers(spec: &RequestSpec) -> Result<HeaderMap, EngineError> {
    let mut headers = build_headers(spec)?;
    // `+proto` names the codec. A bare `application/grpc` is legal and means the same, but
    // being explicit is what makes a proxy's logs readable.
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/grpc+proto"),
    );
    // **Required by the spec, and the whole reason trailers arrive at all.** `TE: trailers` is
    // how a client says it will read them; gRPC puts its status there.
    headers.insert(http::header::TE, HeaderValue::from_static("trailers"));
    // Nothing here negotiates message compression, and `unframe` refuses a compressed frame
    // rather than decoding it as plaintext — so say so instead of letting a server pick
    // something we would then reject.
    headers.insert(
        HeaderName::from_static("grpc-accept-encoding"),
        HeaderValue::from_static("identity"),
    );
    Ok(headers)
}
