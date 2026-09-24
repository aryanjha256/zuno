//! Copy as code: a request rendered as a runnable snippet in another tool or language.
//!
//! **Every target renders one description, `Wire`, and none of them reads a `RequestSpec`.**
//! The spec is how a request is *authored* — a JSON body with no `Content-Type` typed, a form as
//! rows, a GraphQL document and its variables — and not what goes on the wire. The engine turns
//! the one into the other, and a target that re-derived it would be a second, drifting copy of
//! that decision. It happened once already: the curl exporter read the spec directly and never
//! learnt that the engine *derives* a content type, so a JSON body exported with none, and curl
//! then sent it as `application/x-www-form-urlencoded`. `Wire` is built from the engine's own
//! functions — `resolve_url`, `graphql_url`, `graphql_envelope`, `encode_form` — so a snippet
//! sends what Zuno sends.
//!
//! **The spec is expected to be pre-resolved by the caller**, through `Resolver::without_secrets`,
//! for the reason `curl::to_command` always documented: `{{baseUrl}}` becomes your dev host while
//! `{{token}}` stays a placeholder, and substitution rules live in exactly one place.
//!
//! gRPC is the exception that proves the rule: it has no HTTP-shaped description at all, so its
//! one target, grpcurl, reads the gRPC half of the spec directly.

mod csharp;
mod curl;
mod fetch;
mod go;
mod grpcurl;
mod httpie;
mod java;
mod php;
mod python;
mod ruby;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::engine::build;
use crate::request::{
    Body, HttpRequest, Method, MultipartValue, RequestKind, RequestSettings, RequestSpec,
};

/// A language or tool a request can be copied as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Curl,
    Fetch,
    Python,
    Httpie,
    Go,
    Java,
    Ruby,
    CSharp,
    Php,
    Grpcurl,
}

impl Target {
    /// Picker order: curl first, as the command people paste into issues; then roughly by how
    /// often each is asked for.
    pub const ALL: [Target; 10] = [
        Target::Curl,
        Target::Fetch,
        Target::Python,
        Target::Httpie,
        Target::Go,
        Target::Java,
        Target::Ruby,
        Target::CSharp,
        Target::Php,
        Target::Grpcurl,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Target::Curl => "curl",
            Target::Fetch => "JavaScript — fetch",
            Target::Python => "Python — requests",
            Target::Httpie => "HTTPie",
            Target::Go => "Go — net/http",
            Target::Java => "Java — HttpClient",
            Target::Ruby => "Ruby — Net::HTTP",
            Target::CSharp => "C# — HttpClient",
            Target::Php => "PHP — curl",
            Target::Grpcurl => "grpcurl",
        }
    }

    /// How to run what gets copied — the picker's detail column, which is the question someone
    /// choosing between two JavaScript-adjacent options actually has.
    pub fn hint(self) -> &'static str {
        match self {
            Target::Curl | Target::Httpie | Target::Grpcurl => "paste into a shell",
            Target::Fetch => "node file.mjs, or a browser console",
            Target::Python => "python — needs requests",
            Target::Go => "go run main.go",
            Target::Java => "java Main.java — Java 11+",
            Target::Ruby => "ruby file.rb",
            Target::CSharp => "Program.cs — dotnet run",
            Target::Php => "php file.php — needs the curl extension",
        }
    }

    /// Whether this target can express a request of this kind at all.
    ///
    /// **Exhaustive on the kind**, so a new one has to decide what it can be copied as. A
    /// WebSocket gets nothing: it is a conversation rather than a request, and none of these has
    /// a standard way to hold one open and type into it — exporting its handshake as a plain GET
    /// would be a snippet that runs and does something else.
    pub fn supports(self, kind: &RequestKind) -> bool {
        match kind {
            RequestKind::Http(_) | RequestKind::GraphQl(_) => self != Target::Grpcurl,
            RequestKind::Grpc(_) => self == Target::Grpcurl,
            RequestKind::WebSocket(_) => false,
        }
    }

    /// The targets offered for a request, in picker order.
    pub fn offered_for(kind: &RequestKind) -> Vec<Target> {
        Target::ALL
            .into_iter()
            .filter(|target| target.supports(kind))
            .collect()
    }

    /// Render `spec` as this target, or `None` if it cannot express it.
    ///
    /// `collection` is where a gRPC request's bare `greeter.proto` lives; the HTTP targets never
    /// need it.
    pub fn render(self, spec: &RequestSpec, collection: Option<&Path>) -> Option<String> {
        if !self.supports(&spec.kind) {
            return None;
        }
        if self == Target::Grpcurl {
            return grpcurl::render(spec, collection);
        }
        let wire = Wire::of(spec)?;
        Some(match self {
            Target::Curl => curl::render(&wire),
            Target::Fetch => fetch::render(&wire),
            Target::Python => python::render(&wire),
            Target::Httpie => httpie::render(&wire),
            Target::Go => go::render(&wire),
            Target::Java => java::render(&wire),
            Target::Ruby => ruby::render(&wire),
            Target::CSharp => csharp::render(&wire),
            Target::Php => php::render(&wire),
            Target::Grpcurl => unreachable!("rendered from the gRPC half above"),
        })
    }
}

/// A request as it goes on the wire, which is what every HTTP target renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wire {
    pub method: String,
    /// Query rows included and percent-encoded, as the engine would request it.
    pub url: String,
    /// Enabled, named, in order — **plus the content type the engine derives** when none was
    /// typed. Duplicates are kept: HTTP allows them and the engine sends them.
    pub headers: Vec<(String, String)>,
    pub body: WireBody,
    pub follow_redirects: bool,
    pub verify_tls: bool,
    /// Whether the client asks for a compressed response and decodes it.
    pub compressed: bool,
    /// Only a timeout someone *chose*. Zuno's default is a local guard, not part of the request,
    /// and carrying it into every snippet would be noise — curl's exporter drew the same line.
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireBody {
    None,
    /// Raw text, an encoded form, or a GraphQL envelope — all of them text on the wire.
    Text(String),
    /// A file sent as the whole body. **The path, never the bytes**: the engine reads it at send
    /// time, and a snippet that inlined a 2GB upload would be useless.
    File(PathBuf),
    Multipart(Vec<Part>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub value: PartValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartValue {
    Text(String),
    /// Sent with the file's own name, as the engine does — servers routinely key off it.
    File(PathBuf),
}

impl PartValue {
    /// The filename the engine gives a file part: the path's last component, or `file`.
    pub fn filename(path: &Path) -> String {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string())
    }
}

impl Wire {
    /// `None` for a kind with no HTTP-shaped request: a WebSocket, a gRPC call.
    pub fn of(spec: &RequestSpec) -> Option<Wire> {
        let (method, body, derived) = match &spec.kind {
            RequestKind::Http(http) => {
                let (body, derived) = http_body(http);
                (http.method.as_str().to_string(), body, derived)
            }
            // The envelope is the body unless it is a GET, where it rides the query string —
            // the same split `build_graphql` makes, and `url_text` builds that URL.
            RequestKind::GraphQl(graphql) => {
                let body = if matches!(graphql.method, Method::Get | Method::Head) {
                    WireBody::None
                } else {
                    match build::graphql_envelope(graphql) {
                        Ok(envelope) => WireBody::Text(envelope.to_string()),
                        Err(_) => WireBody::None,
                    }
                };
                let derived = matches!(body, WireBody::Text(_)).then_some("application/json");
                (graphql.method.as_str().to_string(), body, derived)
            }
            RequestKind::WebSocket(_) | RequestKind::Grpc(_) => return None,
        };

        let multipart = matches!(body, WireBody::Multipart(_));
        let mut headers: Vec<(String, String)> = spec
            .enabled_headers()
            .filter(|header| !header.name.trim().is_empty())
            // **A typed `Content-Type` cannot survive a multipart body**, exactly as in the
            // engine: the boundary is generated by whatever builds the body, and a header
            // without it is unparseable. Every target's multipart API writes its own.
            .filter(|header| !(multipart && is_content_type(&header.name)))
            .map(|header| (header.name.trim().to_string(), header.value.clone()))
            .collect();

        // An explicit Content-Type always wins; the derived one only fills a gap — `build_http`'s
        // rule, stated here once more because this is where it was missing.
        if let Some(content_type) = derived
            && !headers.iter().any(|(name, _)| is_content_type(name))
        {
            headers.push(("Content-Type".to_string(), content_type.to_string()));
        }

        let default_timeout = RequestSettings::default().timeout;
        Some(Wire {
            method,
            url: url_text(spec),
            headers,
            body,
            follow_redirects: spec.settings.follow_redirects,
            verify_tls: spec.settings.verify_tls,
            compressed: spec.settings.accept_encodings,
            timeout: spec.settings.timeout.filter(|chosen| Some(*chosen) != default_timeout),
        })
    }

    pub fn has_body(&self) -> bool {
        !matches!(self.body, WireBody::None)
    }

    /// Headers merged by name, case-insensitively, values joined with `, `.
    ///
    /// For the targets whose API is a map — Python, PHP's option array — and so cannot carry a
    /// repeated name. Joining is what HTTP defines a repeated field to mean, for every field but
    /// `Set-Cookie`, which a request does not send.
    pub fn merged_headers(&self) -> Vec<(String, String)> {
        let mut merged: Vec<(String, String)> = Vec::new();
        for (name, value) in &self.headers {
            match merged
                .iter_mut()
                .find(|(seen, _)| seen.eq_ignore_ascii_case(name))
            {
                Some((_, joined)) => {
                    joined.push_str(", ");
                    joined.push_str(value);
                }
                None => merged.push((name.clone(), value.clone())),
            }
        }
        merged
    }
}

fn is_content_type(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("content-type")
}

/// The body, and the content type the engine would derive for it.
///
/// Exhaustive with no catch-all, for `build_body`'s reason: a new `Body` variant must fail the
/// build until someone decides how it is exported. The rules match `build_body` line for line —
/// whitespace-only text and a form with no usable field send nothing.
fn http_body(http: &HttpRequest) -> (WireBody, Option<&'static str>) {
    match &http.body {
        Body::Empty => (WireBody::None, None),
        Body::Raw { text, kind } => {
            if text.trim().is_empty() {
                (WireBody::None, None)
            } else {
                (WireBody::Text(text.clone()), Some(kind.content_type()))
            }
        }
        Body::Form(fields) => {
            let encoded = build::encode_form(fields);
            if encoded.is_empty() {
                (WireBody::None, None)
            } else {
                (
                    WireBody::Text(encoded),
                    Some("application/x-www-form-urlencoded"),
                )
            }
        }
        // No guess, as in the engine: the person sets it for a binary upload.
        Body::Binary(path) => (WireBody::File(path.clone()), None),
        Body::Multipart(fields) => {
            let parts: Vec<Part> = fields
                .iter()
                .filter(|field| field.enabled && !field.name.trim().is_empty())
                .map(|field| Part {
                    name: field.name.trim().to_string(),
                    value: match &field.value {
                        MultipartValue::Text(text) => PartValue::Text(text.clone()),
                        MultipartValue::File(path) => PartValue::File(path.clone()),
                    },
                })
                .collect();
            if parts.is_empty() {
                (WireBody::None, None)
            } else {
                (WireBody::Multipart(parts), None)
            }
        }
    }
}

/// The URL as it will appear on the wire, query rows included.
///
/// Goes through `build::resolve_url` rather than concatenating, so the exported URL is the one the
/// engine would actually request — percent-encoding and all.
///
/// It fails for a request that can't be sent, and that is a **normal** outcome here rather than an
/// error: withholding a secret leaves `{{token}}` in the URL, which `resolve_url` rejects by
/// design. The fallback appends the rows unencoded, which is the best that can be said about a
/// snippet the recipient has to finish editing anyway.
fn url_text(spec: &RequestSpec) -> String {
    // A GET GraphQL request carries its envelope in the query string, so the exported URL has
    // to be built the same way the sent one is — see `build::graphql_url`.
    if let RequestKind::GraphQl(graphql) = &spec.kind
        && let Ok(url) = build::graphql_url(spec, graphql)
    {
        return url.to_string();
    }

    if let Ok(url) = build::resolve_url(spec) {
        return url.to_string();
    }

    let mut text = spec.url.trim().to_string();
    let pairs: Vec<String> = spec
        .http()
        .into_iter()
        .flat_map(HttpRequest::enabled_query)
        .filter(|param| !param.name.trim().is_empty())
        .map(|param| format!("{}={}", param.name.trim(), param.value))
        .collect();

    if !pairs.is_empty() {
        text.push(if text.contains('?') { '&' } else { '?' });
        text.push_str(&pairs.join("&"));
    }
    text
}

/// Wrap in single quotes for a POSIX shell.
///
/// Single quotes make every other metacharacter literal, so the only thing needing care is a single
/// quote itself: close, emit an escaped one, reopen. Getting this wrong is a shell-injection bug in
/// a string the user is about to paste into a terminal.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A double-quoted string literal valid in JavaScript, Python, Go, Java and C#.
///
/// **JSON's escaping is the common subset of all five** — `\"`, `\\`, `\n`, `\t` and `\uXXXX` mean
/// the same in each — so one function serves them and each is tested against its own compiler.
/// The four characters escaped on top are line terminators in *some* of them: C# ends a line at
/// U+0085, U+2028 and U+2029, and Go refuses a byte-order mark anywhere but the start of a file.
/// Left raw, a pasted message containing one is a snippet that does not compile.
fn string_literal(text: &str) -> String {
    serde_json::to_string(text)
        .expect("a string always serializes")
        .replace('\u{85}', "\\u0085")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .replace('\u{feff}', "\\ufeff")
}

/// A single-quoted literal for Ruby and PHP, where double quotes interpolate.
///
/// **Single quotes are the choice, not a style.** In a double-quoted Ruby string `#{...}` runs
/// code and in PHP `$name` expands a variable, so a JSON body carrying either would change — or
/// execute — when pasted. Inside single quotes both languages recognise only `\\` and `\'`.
fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{FormField, GraphQlRequest, Header, MultipartField, RawKind};

    fn post(body: Body) -> RequestSpec {
        let mut spec = RequestSpec::default();
        spec.url = "https://api.test/items".to_string();
        if let Some(http) = spec.http_mut() {
            http.method = Method::Post;
            http.body = body;
        }
        spec
    }

    fn content_types(wire: &Wire) -> Vec<&str> {
        wire.headers
            .iter()
            .filter(|(name, _)| is_content_type(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// **The bug this module was written around.** The engine derives a content type for a body
    /// that has none typed; an export that did not would send JSON as a form.
    #[test]
    fn a_body_carries_the_content_type_the_engine_derives() {
        let json = Wire::of(&post(Body::Raw {
            text: r#"{"a":1}"#.into(),
            kind: RawKind::Json,
        }))
        .expect("wire");
        assert_eq!(content_types(&json), vec!["application/json"]);

        let form = Wire::of(&post(Body::Form(vec![FormField {
            enabled: true,
            name: "a".into(),
            value: "1".into(),
        }])))
        .expect("wire");
        assert_eq!(content_types(&form), vec!["application/x-www-form-urlencoded"]);

        // A binary body gets no guess, and nothing is invented for a body that is not there.
        let binary = Wire::of(&post(Body::Binary("/tmp/x.bin".into()))).expect("wire");
        assert!(content_types(&binary).is_empty());
        let empty = Wire::of(&post(Body::Empty)).expect("wire");
        assert!(content_types(&empty).is_empty());

        // GraphQL's envelope is JSON, and that is derived too.
        let mut graphql = RequestSpec::default();
        graphql.url = "https://api.test/graphql".into();
        graphql.kind = RequestKind::GraphQl(GraphQlRequest {
            method: Method::Post,
            query: "{ me { id } }".into(),
            ..GraphQlRequest::default()
        });
        assert_eq!(
            content_types(&Wire::of(&graphql).expect("wire")),
            vec!["application/json"]
        );
    }

    /// An explicit type wins, and only once — and a multipart body drops it, as the engine does.
    #[test]
    fn a_typed_content_type_wins_except_over_multipart() {
        let mut spec = post(Body::Raw {
            text: "<a/>".into(),
            kind: RawKind::Json,
        });
        spec.headers = vec![Header::new("content-type", "application/xml")];
        assert_eq!(
            content_types(&Wire::of(&spec).expect("wire")),
            vec!["application/xml"]
        );

        let mut spec = post(Body::Multipart(vec![MultipartField {
            enabled: true,
            name: "a".into(),
            value: MultipartValue::Text("1".into()),
        }]));
        spec.headers = vec![
            Header::new("Content-Type", "multipart/form-data"),
            Header::new("X-Kept", "1"),
        ];
        let wire = Wire::of(&spec).expect("wire");
        assert!(
            content_types(&wire).is_empty(),
            "a boundary-less multipart type is unparseable, so it cannot be exported"
        );
        assert_eq!(wire.headers, vec![("X-Kept".to_string(), "1".to_string())]);
    }

    /// Only what each target can express is offered — and a WebSocket gets nothing rather than a
    /// snippet that sends a plain GET.
    #[test]
    fn each_kind_is_offered_only_what_can_express_it() {
        let http = RequestKind::Http(HttpRequest::default());
        let offered = Target::offered_for(&http);
        assert_eq!(offered.first(), Some(&Target::Curl), "curl leads the list");
        assert!(!offered.contains(&Target::Grpcurl));
        assert_eq!(offered.len(), Target::ALL.len() - 1);

        let grpc = RequestKind::Grpc(crate::request::GrpcRequest::default());
        assert_eq!(Target::offered_for(&grpc), vec![Target::Grpcurl]);

        let socket = RequestKind::WebSocket(crate::request::WebSocketRequest::default());
        assert!(Target::offered_for(&socket).is_empty());
    }

    /// A chosen timeout is part of the request; Zuno's own default is not.
    #[test]
    fn only_a_chosen_timeout_is_carried() {
        let mut spec = post(Body::Empty);
        assert_eq!(Wire::of(&spec).expect("wire").timeout, None);
        spec.settings.timeout = Some(Duration::from_secs(5));
        assert_eq!(
            Wire::of(&spec).expect("wire").timeout,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn repeated_headers_merge_for_the_map_shaped_targets() {
        let mut spec = post(Body::Empty);
        spec.headers = vec![
            Header::new("Accept", "a"),
            Header::new("accept", "b"),
            Header::new("X-One", "1"),
        ];
        assert_eq!(
            Wire::of(&spec).expect("wire").merged_headers(),
            vec![
                ("Accept".to_string(), "a, b".to_string()),
                ("X-One".to_string(), "1".to_string())
            ]
        );
    }

    /// The literals are the injection surface: what a pasted snippet does with `$`, `#{}` and a
    /// quote decides whether it sends the body or runs it.
    #[test]
    fn literals_escape_what_each_language_would_otherwise_interpret() {
        assert_eq!(string_literal("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
        // Built with `format!` so the expected escape cannot itself be decoded into the raw
        // character it names — which is what happened to this line's first version.
        assert_eq!(
            string_literal("x\u{2028}y"),
            format!("\"x{}u2028y\"", '\\')
        );
        assert_eq!(single_quoted(r"it's #{x} $y \"), r"'it\'s #{x} $y \\'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
