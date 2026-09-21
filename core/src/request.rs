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
    /// **Two deadlines, not one**: how long the response head may take, and how long the
    /// response may then go silent. It is deliberately *not* a deadline on the whole exchange,
    /// which is what it used to be — a `text/event-stream` can never meet one, so a stream
    /// answered by an HTTP request was killed at whatever this said, with no close and nothing
    /// explaining it.
    /// `run::execute` enforces the first half; `ClientKey::read_timeout` enforces the second.
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

/// The HTTP-shaped half of a request: the parts that mean nothing to a protocol
/// that isn't HTTP.
///
/// `method` lives here rather than on the spine because only some protocols have one.
/// HTTP and GraphQL do; gRPC is always POST and never shows it; MQTT has no such concept.
/// On the spine it would put a meaningless `GET` on every MQTT request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: Method,
    pub query: Vec<QueryParam>,
    pub body: Body,
}

impl HttpRequest {
    /// Only the rows that will actually go on the wire.
    pub fn enabled_query(&self) -> impl Iterator<Item = &QueryParam> {
        self.query.iter().filter(|param| param.enabled)
    }
}

/// What kind of request this is — the authoring surface, not the transport.
///
/// **A new variant is earned by needing a different authoring surface, not by being a
/// different protocol.** SOAP and JSON-RPC are HTTP POST with a particular body and
/// `Http` already sends them correctly; they would earn a variant only if someone wanted
/// WSDL- or method-driven authoring. GraphQL earns one because a query plus variables over
/// an introspected schema is nothing like a body editor.
///
/// Matched exhaustively with no catch-all everywhere it is read, for the reason
/// `Body` is: adding a kind must fail the build until someone has decided how it is
/// authored, substituted into, sent, and exported.
///
/// **Whether a request streams is deliberately not expressed here.** That is decided in
/// four different places depending on protocol — by the server for SSE, by the document
/// text for a GraphQL `subscription`, by the schema for a gRPC server-stream, by the
/// protocol itself for MQTT — so it is a property of a *run*, not of a saved request.
/// `Http` and `HttpStreaming` as sibling variants would split one saved request in two.
/// A GraphQL request: one endpoint, and a document that says what you want back.
///
/// **`query` and `variables` are both plain `String`s**, for `url`'s reason (§3.1): they are
/// invalid on most keystrokes, and a model that refuses to hold invalid text cannot back an
/// editor. `variables` is JSON *text*, parsed at the send boundary into the envelope, so a
/// malformed one is a typed error rather than an unrepresentable state.
///
/// **`operation` is only needed when the document holds more than one named operation** —
/// GraphQL's `operationName`, which tells the server which of them to run. `None` means the
/// document has one operation and the server can work it out.
///
/// `method` is here rather than on the spine for the reason `HttpRequest`'s is, and it is
/// genuinely variable: POST is the norm, GET exists so a query can be cached by ordinary HTTP
/// machinery, and `build` puts the envelope in the query string instead of the body for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphQlRequest {
    pub method: Method,
    /// The document — one or more operations, plus any fragments they use.
    pub query: String,
    /// The variables, as JSON *text*. Empty means none.
    pub variables: String,
    /// Which operation to run, when the document holds several.
    pub operation: Option<String>,
    /// How the operation reaches the server.
    ///
    /// **A choice, with a default that is usually right.** Almost every GraphQL server serves
    /// subscriptions over WebSocket and queries over POST, so `Auto` reads the document and
    /// picks — which means nobody has to answer a question about their own backend they may not
    /// know the answer to. The other two exist because "almost every" is not "every", and
    /// because a tool doing something one way is not a reason to have no opinion of your own.
    ///
    /// `#[serde(default)]` is load-bearing: every GraphQL request written before this field
    /// existed has no `transport` key, and invariant 11 means those files must keep opening.
    ///
    /// `skip_serializing_if` is the other half of the same invariant, and is *not* optional.
    /// Writing `"transport": "Auto"` unconditionally would mean the first save after an upgrade
    /// produces a diff touching every GraphQL file in the collection and saying nothing — the
    /// churn the additive format exists to avoid. Only a transport somebody chose reaches disk.
    #[serde(default, skip_serializing_if = "GraphQlTransport::is_default")]
    pub transport: GraphQlTransport,
}

/// How a GraphQL operation reaches the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphQlTransport {
    /// Read the document: a subscription opens a socket, everything else posts.
    #[default]
    Auto,
    /// Always POST. A subscription sent this way still streams if the server answers
    /// `text/event-stream` — which is graphql-sse, and needs nothing from this field.
    Http,
    /// Always open a socket and speak `graphql-transport-ws`.
    WebSocket,
}

impl GraphQlTransport {
    /// Whether this is the value a file with no `transport` key reads back as.
    ///
    /// Takes `&self` because that is the shape `skip_serializing_if` wants.
    fn is_default(&self) -> bool {
        *self == GraphQlTransport::Auto
    }

    pub fn label(self) -> &'static str {
        match self {
            GraphQlTransport::Auto => "Auto",
            GraphQlTransport::Http => "HTTP",
            GraphQlTransport::WebSocket => "WebSocket",
        }
    }

    /// What picking it gets you, for the picker's dimmed column.
    pub fn detail(self) -> &'static str {
        match self {
            GraphQlTransport::Auto => "subscriptions over a socket, the rest over POST",
            GraphQlTransport::Http => "POST always; streams only if the server sends events",
            GraphQlTransport::WebSocket => "graphql-transport-ws, even for a query",
        }
    }

    pub const ALL: [GraphQlTransport; 3] = [
        GraphQlTransport::Auto,
        GraphQlTransport::Http,
        GraphQlTransport::WebSocket,
    ];
}

impl Default for GraphQlRequest {
    fn default() -> Self {
        Self {
            // Not `Method::default()`, which is GET: a GraphQL request is a POST unless
            // someone deliberately wants it cacheable.
            method: Method::Post,
            query: String::new(),
            variables: String::new(),
            operation: None,
            transport: GraphQlTransport::default(),
        }
    }
}

impl GraphQlRequest {
    /// Whether this will open a socket rather than post.
    ///
    /// The one place the decision is made, so the engine's routing, the UI's label and the
    /// handshake all agree — three readings of the same document would eventually disagree.
    pub fn uses_websocket(&self) -> bool {
        match self.transport {
            GraphQlTransport::WebSocket => true,
            GraphQlTransport::Http => false,
            GraphQlTransport::Auto => {
                crate::graphql::operation_kind(&self.query, self.operation.as_deref())
                    == Some(crate::graphql::OperationKind::Subscription)
            }
        }
    }
}

/// A message kept with the socket, so reconnecting does not mean retyping what you send.
///
/// Named, because the useful ones are a handful of fixed payloads — a subscribe envelope, an
/// auth frame, a heartbeat — and picking one from a list beats scrolling a transcript for the
/// last time you typed it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMessage {
    pub name: String,
    /// Sent verbatim, `{{vars}}` resolved. Text only: a binary frame you can type is a
    /// contradiction, and one you can paste is a file picker, which is not this.
    pub body: String,
}

/// A WebSocket endpoint and what to say to it.
///
/// **No method, and that is the point of the kind split.** The handshake is a GET and nothing
/// else is legal, so there is no verb to choose; `HttpRequest::method` would be a field with one
/// value. What is here instead is what only a socket has: the subprotocols to offer, and a
/// library of messages, because a socket is a conversation rather than one exchange.
///
/// Every field carries `#[serde(default)]`, unlike `GraphQlRequest`. A collection file has no
/// version to migrate on (invariant 11), so the only way a *third* field can be added later
/// without orphaning every socket written before it is for absence to already mean something.
/// Nothing is lost by starting that way; it is a promise that costs nothing until it is needed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketRequest {
    /// `Sec-WebSocket-Protocol` offers, in preference order. The server picks at most one.
    #[serde(default)]
    pub subprotocols: Vec<String>,
    #[serde(default)]
    pub messages: Vec<SavedMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestKind {
    Http(HttpRequest),
    GraphQl(GraphQlRequest),
    WebSocket(WebSocketRequest),
}

impl RequestKind {
    /// The short tag a collection row wears: `GQL`, and later `WS`, `gRPC`, `MQTT`.
    ///
    /// `None` for HTTP, where the *method* is the useful distinction and the row shows that
    /// instead. For every other kind the method is noise — every GraphQL request is a POST —
    /// so the kind is what tells two rows apart.
    pub fn badge(&self) -> Option<&'static str> {
        match self {
            RequestKind::Http(_) => None,
            RequestKind::GraphQl(_) => Some("GQL"),
            RequestKind::WebSocket(_) => Some("WS"),
        }
    }

    /// Whether sending this opens a connection that stays open.
    ///
    /// **Not the whole answer, deliberately.** This is what the *request* can promise before
    /// anything has left the machine, and only a socket can promise it. An ordinary HTTP
    /// request becomes a session whenever the server answers `text/event-stream`, and a
    /// GraphQL subscription becomes one whenever the server speaks graphql-sse — neither is
    /// knowable from here. So the response side has the final say and this is the hint that
    /// lets the UI show Connect instead of Send *before* it can possibly know.
    pub fn is_session(&self) -> bool {
        match self {
            RequestKind::WebSocket(_) => true,
            // **A GraphQL subscription is a session before it leaves**, unlike an SSE stream,
            // and that is the one place the "the server decides" rule bends. It has to: a
            // socket handshake and a POST are different requests, so the client cannot wait to
            // be told. `uses_websocket` reads the document to decide.
            RequestKind::GraphQl(graphql) => graphql.uses_websocket(),
            RequestKind::Http(_) => false,
        }
    }

    /// What this kind is called in the UI.
    pub fn label(&self) -> &'static str {
        match self {
            RequestKind::Http(_) => "HTTP",
            RequestKind::GraphQl(_) => "GraphQL",
            RequestKind::WebSocket(_) => "WebSocket",
        }
    }
}

impl Default for RequestKind {
    fn default() -> Self {
        RequestKind::Http(HttpRequest::default())
    }
}

/// One request, whatever protocol it speaks.
///
/// **The spine holds only what means the same thing for every protocol** — an endpoint,
/// ordered key/value metadata, connection settings, an identity, and how the answer is
/// checked and captured from. Anything that differs by protocol lives in `kind`; anything
/// shared *between* requests (a `.proto`, a WSDL, a cached schema) is not a request field
/// at all and belongs in a reserved directory beside `environments/` and `flows/`.
///
/// `headers` is on the spine because every protocol has ordered key/value metadata —
/// HTTP headers, gRPC metadata, a WebSocket handshake, MQTT 5 user properties. The name is
/// HTTP-flavoured and stays that way: it is the field name in every collection file on
/// disk, and renaming it would rewrite every one of them to say the same thing.
///
/// `expect_status` is the one spine field that is *not* universal — HTTP and gRPC have a
/// status, MQTT does not. It is an `Option`, so `None` is the honest answer there, but it
/// is the first thing to move if a statusless protocol ever needs a verdict of its own.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "StoredSpec")]
pub struct RequestSpec {
    pub id: RequestId,
    pub name: String,
    /// Raw, unresolved, possibly invalid. Parsed only at the send boundary.
    pub url: String,
    pub headers: Vec<Header>,
    pub settings: RequestSettings,
    /// What differs by protocol.
    pub kind: RequestKind,
    /// What this request publishes into the environment after a successful send.
    ///
    /// `#[serde(default)]` per *field*, which is the pattern the note above `RequestSettings`
    /// prescribes: the container-level default is what `RequestSpec` refuses, so a corrupt file
    /// is still rejected rather than becoming an empty request, while a collection written by an
    /// older build still parses.
    #[serde(default)]
    pub captures: Vec<crate::capture::Capture>,
    /// The status a run expects. `None` means the request states none, so a run cannot fail it.
    ///
    /// Its own field rather than a row in `assertions`, because every request wants to check the
    /// status and a table row saying so would sit on every request in the collection.
    #[serde(default)]
    pub expect_status: Option<u16>,
    /// What a run checks in the response body. Defaulted per field, for `captures`' reason.
    #[serde(default)]
    pub assertions: Vec<crate::assertion::Assertion>,
}

impl Default for RequestSpec {
    fn default() -> Self {
        Self {
            id: RequestId(0),
            name: "Untitled".to_string(),
            url: String::new(),
            headers: Vec::new(),
            settings: RequestSettings::default(),
            kind: RequestKind::default(),
            captures: Vec::new(),
            expect_status: None,
            assertions: Vec::new(),
        }
    }
}

/// The shape `RequestSpec` is *read* from, which is both the current one and every one
/// written before `RequestKind` existed.
///
/// **Collection files carry no version field** — `collection::read` is a bare
/// `serde_json::from_slice::<RequestSpec>` — so there is nothing to dispatch a migration
/// on, and the two shapes have to be told apart structurally. A file written before this
/// change has `method`/`query`/`body` at the top level and no `kind`; one written after has
/// `kind` and none of the three. That is an unambiguous discriminator, which is why no
/// version field had to be added: adding one would rewrite every committed collection file
/// to say nothing, which is the diff churn invariant 9 exists to avoid.
///
/// **`TryFrom`, not `From`, and that is what keeps `RequestSpec` strict.** A file carrying
/// *neither* shape is a corrupt or unrelated document, and the note above `RequestSettings`
/// is explicit that `RequestSpec` must reject one rather than quietly become an empty
/// request. Defaulting the three legacy fields would have accepted it.
#[derive(Deserialize)]
struct StoredSpec {
    id: RequestId,
    name: String,
    url: String,
    headers: Vec<Header>,
    settings: RequestSettings,
    #[serde(default)]
    captures: Vec<crate::capture::Capture>,
    #[serde(default)]
    expect_status: Option<u16>,
    #[serde(default)]
    assertions: Vec<crate::assertion::Assertion>,

    /// Present in files written by this build and later.
    #[serde(default)]
    kind: Option<RequestKind>,

    // Present only in files written before `RequestKind` existed. Read once, here, and
    // never anywhere else in the codebase.
    #[serde(default)]
    method: Option<Method>,
    #[serde(default)]
    query: Option<Vec<QueryParam>>,
    #[serde(default)]
    body: Option<Body>,
}

/// What `RequestSpec` is *written* as — and it is deliberately **not** the in-memory shape.
///
/// **An HTTP request serializes to the exact bytes 0.2.9 wrote**: `method`, `query` and `body`
/// at the top level, no `kind`. `kind` appears only for a request that genuinely is not HTTP.
/// That makes the format change **additive** rather than a rewrite, and buys two things that a
/// straight rename of the fields would have cost:
///
/// - **An older Zuno keeps reading files this one writes.** Forward compatibility is not
///   symmetric with backward: a released build's behaviour is fixed, and 0.2.9 responds to a
///   session it cannot parse by falling back to the sample and then *overwriting the file on
///   quit*. So a shape it cannot read is not an inconvenience, it is data loss — and bumping
///   `session::CURRENT_VERSION` does not help, because its "written by a newer Zuno" arm fails
///   in exactly the same way. Writing what it already understands is the only fix that works
///   from this side. This is not hypothetical: it destroyed a workspace's open tabs once.
/// - **Zero diff churn.** Collection files exist to be committed and reviewed (§12). Re-saving a
///   request must not rewrite its shape, or the first save after an upgrade is a diff touching
///   every file in the collection and saying nothing.
///
/// Field order matches 0.2.9's exactly, because "byte-identical" is what the test asserts and
/// serde writes fields in declaration order. `kind` sits after `body` so that adding it cannot
/// move anything that precedes it.
///
/// Borrowed rather than owned: `session::save` serializes every open buffer including its whole
/// body, so a `#[serde(into = ...)]` conversion would clone all of it on every save.
#[derive(Serialize)]
struct StoredSpecRef<'a> {
    id: RequestId,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'a Method>,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<&'a Vec<QueryParam>>,
    headers: &'a Vec<Header>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a Body>,
    /// Absent for an HTTP request, which is the whole point of this type.
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a RequestKind>,
    settings: &'a RequestSettings,
    captures: &'a Vec<crate::capture::Capture>,
    expect_status: Option<u16>,
    assertions: &'a Vec<crate::assertion::Assertion>,
}

impl Serialize for RequestSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Exhaustive with no catch-all: a new kind has to decide how it reaches disk, and
        // whether an older build can still read the file it lands in.
        let (method, query, body, kind) = match &self.kind {
            RequestKind::Http(http) => {
                (Some(&http.method), Some(&http.query), Some(&http.body), None)
            }
            // Written as `kind`, because there is no older shape to be compatible with — a
            // GraphQL request could not exist before this field did. An older Zuno cannot read
            // it, which is correct and unavoidable; what matters is that it cannot be *confused*
            // for something else, and a file with `kind` and no `method` is refused outright.
            RequestKind::GraphQl(_) => (None, None, None, Some(&self.kind)),
            // Same reasoning, and the same absence of a flat shape to imitate.
            RequestKind::WebSocket(_) => (None, None, None, Some(&self.kind)),
        };

        StoredSpecRef {
            id: self.id,
            name: &self.name,
            method,
            url: &self.url,
            query,
            headers: &self.headers,
            body,
            kind,
            settings: &self.settings,
            captures: &self.captures,
            expect_status: self.expect_status,
            assertions: &self.assertions,
        }
        .serialize(serializer)
    }
}

impl TryFrom<StoredSpec> for RequestSpec {
    type Error = String;

    fn try_from(stored: StoredSpec) -> Result<Self, Self::Error> {
        let kind = match (stored.kind, stored.method, stored.query, stored.body) {
            // Written by this build: the kind says everything.
            (Some(kind), None, None, None) => kind,
            // Written before `RequestKind` existed. `query` and `body` were not optional
            // then, but an absent one is still readable as its empty value — only `method`
            // has to be there, since its absence is what says the file is neither shape.
            (None, Some(method), query, body) => RequestKind::Http(HttpRequest {
                method,
                query: query.unwrap_or_default(),
                body: body.unwrap_or_default(),
            }),
            _ => {
                return Err(
                    "a request must carry either `kind` or a top-level `method`, not both \
                     and not neither"
                        .to_string(),
                );
            }
        };

        Ok(Self {
            id: stored.id,
            name: stored.name,
            url: stored.url,
            headers: stored.headers,
            settings: stored.settings,
            kind,
            captures: stored.captures,
            expect_status: stored.expect_status,
            assertions: stored.assertions,
        })
    }
}

impl RequestSpec {
    /// Only the rows that will actually go on the wire.
    pub fn enabled_headers(&self) -> impl Iterator<Item = &Header> {
        self.headers.iter().filter(|header| header.enabled)
    }

    /// The HTTP half, when this is an HTTP request.
    ///
    /// Deliberately an `Option` rather than a panicking accessor: a caller that has no
    /// answer for another kind should be made to say so. Code that must *behave*
    /// differently per kind matches on `kind` instead, so that adding one is a compile
    /// error there — this is only for the places where a non-HTTP kind genuinely has
    /// nothing to contribute.
    pub fn http(&self) -> Option<&HttpRequest> {
        match &self.kind {
            RequestKind::Http(http) => Some(http),
            RequestKind::GraphQl(_) | RequestKind::WebSocket(_) => None,
        }
    }

    pub fn http_mut(&mut self) -> Option<&mut HttpRequest> {
        match &mut self.kind {
            RequestKind::Http(http) => Some(http),
            RequestKind::GraphQl(_) | RequestKind::WebSocket(_) => None,
        }
    }

    /// The GraphQL half, when this is a GraphQL request.
    pub fn graphql(&self) -> Option<&GraphQlRequest> {
        match &self.kind {
            RequestKind::GraphQl(graphql) => Some(graphql),
            RequestKind::Http(_) | RequestKind::WebSocket(_) => None,
        }
    }

    pub fn graphql_mut(&mut self) -> Option<&mut GraphQlRequest> {
        match &mut self.kind {
            RequestKind::GraphQl(graphql) => Some(graphql),
            RequestKind::Http(_) | RequestKind::WebSocket(_) => None,
        }
    }

    /// The WebSocket half, when this is a WebSocket request.
    pub fn websocket(&self) -> Option<&WebSocketRequest> {
        match &self.kind {
            RequestKind::WebSocket(socket) => Some(socket),
            RequestKind::Http(_) | RequestKind::GraphQl(_) => None,
        }
    }

    pub fn websocket_mut(&mut self) -> Option<&mut WebSocketRequest> {
        match &mut self.kind {
            RequestKind::WebSocket(socket) => Some(socket),
            RequestKind::Http(_) | RequestKind::GraphQl(_) => None,
        }
    }

    /// The method this request will be sent with, when its kind has one.
    ///
    /// gRPC is always POST and never shows it; MQTT has no method at all. `None` is the
    /// honest answer for those, and the reason `method` is not on the spine.
    pub fn method(&self) -> Option<&Method> {
        match &self.kind {
            RequestKind::Http(http) => Some(&http.method),
            // GraphQL rides HTTP, so it has one and it is variable: POST normally, GET when
            // the query should be cacheable.
            RequestKind::GraphQl(graphql) => Some(&graphql.method),
            // The handshake is a GET and nothing else is legal, so there is no choice to
            // show. Reporting one would put a control on screen that cannot be changed.
            RequestKind::WebSocket(_) => None,
        }
    }

    /// A populated request for the M1.0 shell to render. Replaced by real
    /// editing in M1.1.
    pub fn sample() -> Self {
        Self {
            id: RequestId(1),
            name: "List repositories".to_string(),
            url: "https://api.github.com/graphql".to_string(),
            headers: vec![
                Header::new("Content-Type", "application/json"),
                Header::new("Accept", "application/vnd.github+json"),
                Header::new("User-Agent", concat!("zuno/", env!("CARGO_PKG_VERSION"))),
                Header::disabled("Authorization", "Bearer {{token}}"),
            ],
            settings: RequestSettings::default(),
            kind: RequestKind::Http(HttpRequest {
                method: Method::Post,
                query: vec![QueryParam::new("per_page", "50")],
                body: Body::Raw {
                    text: "{\n  \"query\": \"{ viewer { login } }\"\n}".to_string(),
                    kind: RawKind::Json,
                },
            }),
            captures: Vec::new(),
            expect_status: None,
            assertions: Vec::new(),
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
/// Characters, not pixels: real widths need the shipping font, which no test has. Done in Rust
/// rather than by gpui's `truncate()`, which cannot be relied on — see CLAUDE.md.
pub fn elide(label: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if max_chars == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    if label.chars().count() <= max_chars {
        return std::borrow::Cow::Borrowed(label);
    }
    // `- 1` for the ellipsis; by char, not byte, since a path segment can be multi-byte.
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

/// Shorten a label by dropping its **head**, keeping the tail.
///
/// The mirror of `elide`, and which one a column wants depends on where its information sits.
/// A path's head names the collection and its tail is one more request, so it keeps the head.
/// A URL's head is the `http://host:port` every row in a list repeats and its tail is the
/// endpoint that tells them apart, so it keeps the tail — trimmed the other way, every row in a
/// collection reads `http://localhost:8080/api/notif…`.
pub fn elide_front(label: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    let count = label.chars().count();
    if max_chars == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    if count <= max_chars {
        return std::borrow::Cow::Borrowed(label);
    }

    // `- 1` for the ellipsis; by char, not byte, since a path segment can be multi-byte.
    let keep = max_chars - 1;
    let start = label
        .char_indices()
        .nth(count - keep)
        .map(|(ix, _)| ix)
        .unwrap_or(label.len());

    let mut out = String::with_capacity(3 + (label.len() - start));
    out.push('…');
    out.push_str(&label[start..]);
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
    use super::*;

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
    }

    #[test]
    fn eliding_the_front_keeps_the_end_of_a_url() {
        let url = "http://localhost:8080/api/notification/v1/blob/internal/presigned-url";

        let short = elide_front(url, 50);
        // The endpoint is what tells one row from another; `http://localhost:8080` is the same
        // on every row in the collection, so trimming the *end* would leave them identical.
        assert!(short.starts_with('…'), "{short}");
        assert!(short.ends_with("presigned-url"), "{short}");
        assert_eq!(short.chars().count(), 50, "the budget is a budget: {short}");

        assert_eq!(elide_front("short", 20), "short");
        assert!(matches!(elide_front("short", 20), std::borrow::Cow::Borrowed(_)));
        assert_eq!(elide_front("anything", 0), "");
    }

    #[test]
    fn eliding_the_front_never_splits_a_character() {
        let path = "日本語のフォルダー/コントローラー/リクエストの名前";
        for budget in 0..path.chars().count() + 2 {
            let out = elide_front(path, budget);
            assert!(out.chars().count() <= budget.max(1), "budget {budget}: {out}");
        }
    }

    #[test]
    fn a_budget_of_one_is_all_ellipsis_rather_than_an_underflow() {
        // Degenerate, but both directions compute `max_chars - 1` and neither may underflow.
        assert_eq!(elide("anything", 1), "…");
        assert_eq!(elide_front("anything", 1), "…");
    }

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

    /// **A collection file written before `RequestKind` existed still opens.**
    ///
    /// These bytes are not hand-written — they were emitted by the 0.2.9 build (the commit
    /// before this change) serializing `RequestSpec::sample()`, so they are what is actually
    /// sitting in people's collections rather than what this change assumes is. Hand-writing
    /// the fixture would let a wrong assumption about the old shape pass as coverage.
    ///
    /// Break `TryFrom<StoredSpec>`'s legacy arm and this fails; a round-trip test would not,
    /// because the new code agrees with itself either way.
    const LEGACY_SAMPLE: &str = r#"{
      "id": 0,
      "name": "List repositories",
      "method": "Post",
      "url": "https://api.github.com/graphql",
      "query": [{ "enabled": true, "name": "per_page", "value": "50" }],
      "headers": [
        { "enabled": true, "name": "Content-Type", "value": "application/json" },
        { "enabled": false, "name": "Authorization", "value": "Bearer {{token}}" }
      ],
      "body": {
        "Raw": { "text": "{ viewer { login } }", "kind": "Json" }
      },
      "settings": {
        "timeout": { "secs": 30, "nanos": 0 },
        "follow_redirects": true,
        "max_redirects": 10,
        "verify_tls": true,
        "accept_encodings": true,
        "cookie_store": true
      },
      "captures": [],
      "expect_status": null,
      "assertions": []
    }"#;

    #[test]
    fn a_request_written_before_kinds_existed_still_opens() {
        let spec: RequestSpec = serde_json::from_str(LEGACY_SAMPLE).expect("a 0.2.9 file parses");

        assert_eq!(spec.url, "https://api.github.com/graphql");
        assert_eq!(spec.headers.len(), 2);

        let http = spec.http().expect("a legacy file is an HTTP request");
        assert_eq!(http.method, Method::Post);
        assert_eq!(http.query, vec![QueryParam::new("per_page", "50")]);
        assert_eq!(
            http.body,
            Body::Raw {
                text: "{ viewer { login } }".to_string(),
                kind: RawKind::Json,
            }
        );
    }

    /// The same, for a body that is not `Raw` — the variant whose shape the old and new
    /// files share least obviously.
    #[test]
    fn a_legacy_form_body_survives_the_kind_split() {
        let legacy = r#"{
          "id": 0,
          "name": "Untitled",
          "method": "Post",
          "url": "https://auth.test/token",
          "query": [],
          "headers": [],
          "body": {
            "Form": [{ "enabled": true, "name": "grant_type", "value": "client_credentials" }]
          },
          "settings": {
            "timeout": { "secs": 30, "nanos": 0 },
            "follow_redirects": true,
            "max_redirects": 10,
            "verify_tls": true,
            "accept_encodings": true,
            "cookie_store": true
          },
          "captures": [],
          "expect_status": null,
          "assertions": []
        }"#;

        let spec: RequestSpec = serde_json::from_str(legacy).expect("a 0.2.9 form file parses");
        assert_eq!(
            spec.http().expect("HTTP").body,
            Body::Form(vec![FormField {
                enabled: true,
                name: "grant_type".to_string(),
                value: "client_credentials".to_string(),
            }])
        );
    }

    /// **The strictness the container-level `#[serde(default)]` note protects.**
    ///
    /// A document carrying neither shape is not an old request, it is a corrupt or unrelated
    /// file, and `RequestSpec` has to reject it rather than quietly become an empty request —
    /// which is exactly what defaulting the three legacy fields would have done. This is why
    /// the shim is `TryFrom` rather than `From`.
    #[test]
    fn a_document_carrying_neither_shape_is_refused() {
        let neither = r#"{
          "id": 0,
          "name": "Untitled",
          "url": "https://a.test",
          "headers": [],
          "settings": {
            "timeout": { "secs": 30, "nanos": 0 },
            "follow_redirects": true,
            "max_redirects": 10,
            "verify_tls": true,
            "accept_encodings": true,
            "cookie_store": true
          }
        }"#;
        assert!(serde_json::from_str::<RequestSpec>(neither).is_err());
    }

    /// And a file carrying *both* is ambiguous rather than generous: the two would disagree
    /// the moment either was edited, and silently preferring one is how a saved change goes
    /// missing.
    #[test]
    fn a_document_carrying_both_shapes_is_refused() {
        let both = r#"{
          "id": 0,
          "name": "Untitled",
          "url": "https://a.test",
          "headers": [],
          "settings": {
            "timeout": { "secs": 30, "nanos": 0 },
            "follow_redirects": true,
            "max_redirects": 10,
            "verify_tls": true,
            "accept_encodings": true,
            "cookie_store": true
          },
          "method": "Get",
          "kind": { "Http": { "method": "Post", "query": [], "body": "Empty" } }
        }"#;
        assert!(serde_json::from_str::<RequestSpec>(both).is_err());
    }

    /// **The guard that makes a format change reviewable without reading the diff.**
    ///
    /// Pins the *bytes* an HTTP request is written as, against the exact shape 0.2.9 emitted.
    /// Any future change to `RequestSpec`'s on-disk form fails here rather than needing a human
    /// to notice it — which is the point, because the cost of missing one is not a wrong value
    /// but an older build discarding the file and overwriting it.
    ///
    /// Paired with `a_request_written_before_kinds_existed_still_opens`, the two together say
    /// the format is unchanged for HTTP in **both** directions. Backward compatibility alone is
    /// what shipped the bug this exists to prevent: it was tested thoroughly, and forward
    /// compatibility was never considered at all.
    #[test]
    fn an_http_request_is_written_in_the_shape_an_older_zuno_reads() {
        let mut spec = RequestSpec::default();
        spec.url = "https://a.test/one".to_string();
        spec.headers = vec![Header::new("Accept", "application/json")];
        spec.http_mut().expect("HTTP").method = Method::Delete;
        spec.http_mut().expect("HTTP").query = vec![QueryParam::new("v", "2")];

        // Compared as a string, not through `serde_json::Value`: without the `preserve_order`
        // feature a `Value` is a sorted map, so it cannot see field order at all — and order is
        // half of what "byte-identical" means.
        let written = serde_json::to_string(&spec).expect("serialize");

        assert_eq!(
            written,
            concat!(
                r#"{"id":0,"name":"Untitled","method":"Delete","url":"https://a.test/one","#,
                r#""query":[{"enabled":true,"name":"v","value":"2"}],"#,
                r#""headers":[{"enabled":true,"name":"Accept","value":"application/json"}],"#,
                r#""body":"Empty","#,
                r#""settings":{"timeout":{"secs":30,"nanos":0},"follow_redirects":true,"#,
                r#""max_redirects":10,"verify_tls":true,"accept_encodings":true,"#,
                r#""cookie_store":true},"#,
                r#""captures":[],"expect_status":null,"assertions":[]}"#,
            ),
            "an HTTP request must be written in 0.2.9's exact shape, field order included"
        );
        assert!(
            !written.contains(r#""kind""#),
            "`kind` must not reach disk for an HTTP request — an older Zuno cannot read it, \
             and responds by discarding the file and overwriting it"
        );
    }

    /// And the round trip through those legacy bytes is lossless, so "readable by an older
    /// build" does not quietly mean "readable, minus something".
    #[test]
    fn writing_then_reading_an_http_request_changes_nothing() {
        let mut spec = RequestSpec::sample();
        spec.expect_status = Some(201);
        spec.http_mut().expect("HTTP").body = Body::Form(vec![FormField {
            enabled: true,
            name: "grant_type".to_string(),
            value: "client_credentials".to_string(),
        }]);

        let bytes = serde_json::to_vec(&spec).expect("serialize");
        let back: RequestSpec = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(spec, back);
    }

    /// A GraphQL request is written as `kind`, and round-trips.
    ///
    /// There is no older shape to stay compatible with here — a GraphQL request could not exist
    /// before the field did — so unlike an HTTP request this one *does* change the bytes, and
    /// that is correct. What matters is that the two shapes stay mutually exclusive, which the
    /// refusal tests above pin from the other side.
    #[test]
    fn a_graphql_request_is_written_as_a_kind_and_round_trips() {
        let spec = RequestSpec {
            url: "https://api.test/graphql".to_string(),
            kind: RequestKind::GraphQl(GraphQlRequest {
                method: Method::Post,
                query: "query R($n: Int!) { repos(first: $n) { id } }".to_string(),
                variables: r#"{"n": 50}"#.to_string(),
                operation: Some("R".to_string()),
                transport: Default::default(),
            }),
            ..RequestSpec::default()
        };

        let written = serde_json::to_string(&spec).expect("serialize");
        assert!(written.contains(r#""kind""#), "a GraphQL request must carry its kind");
        assert!(
            !written.contains(r#""method":"Post","url""#),
            "and must not also carry the flat HTTP shape, which would be ambiguous"
        );

        let back: RequestSpec = serde_json::from_str(&written).expect("deserialize");
        assert_eq!(spec, back);
    }

    /// A GraphQL request still has a method, and it is POST by default rather than GET.
    ///
    /// Worth pinning because `Method::default()` is GET, so the obvious `#[derive(Default)]`
    /// would have produced a GraphQL request that sends its envelope in the URL — which works,
    /// and is not what anyone means by "a new GraphQL request".
    #[test]
    fn a_new_graphql_request_is_a_post() {
        let spec = RequestSpec {
            kind: RequestKind::GraphQl(GraphQlRequest::default()),
            ..RequestSpec::default()
        };
        assert_eq!(spec.method(), Some(&Method::Post));
        assert!(spec.http().is_none(), "a GraphQL request has no HTTP body table");
    }

    /// **A socket survives the round trip with every field it was given.**
    ///
    /// Invariant 11's territory rather than a formality: a collection file carries no version
    /// to migrate on, so the only protection a new kind has is that what is written comes back.
    /// Asserted on the whole `RequestSpec` for `bundle`'s reason — a field-by-field check
    /// passes while a field added later goes quietly missing.
    #[test]
    fn a_websocket_request_is_written_as_a_kind_and_round_trips() {
        let spec = RequestSpec {
            url: "wss://api.test/subscribe".to_string(),
            kind: RequestKind::WebSocket(WebSocketRequest {
                subprotocols: vec!["graphql-transport-ws".to_string()],
                messages: vec![SavedMessage {
                    name: "subscribe".to_string(),
                    body: r#"{"type":"connection_init"}"#.to_string(),
                }],
            }),
            ..RequestSpec::default()
        };

        let written = serde_json::to_string(&spec).expect("serialize");
        assert!(written.contains(r#""kind""#), "a socket must carry its kind");
        assert!(
            !written.contains(r#""method""#),
            "and must not carry the flat HTTP shape, which would be ambiguous: {written}"
        );

        let back: RequestSpec = serde_json::from_str(&written).expect("deserialize");
        assert_eq!(spec, back);
    }

    /// **A default transport writes nothing**, so upgrading does not rewrite every GraphQL file.
    ///
    /// Invariant 11's second clause: a collection is committed and reviewed, so re-saving a
    /// request must not change its bytes for a field nobody set. Asserted on the serialized
    /// text rather than a round trip, because a round trip agrees with itself either way.
    #[test]
    fn a_default_graphql_transport_is_not_written() {
        let mut spec = RequestSpec::default();
        spec.url = "https://api.test/graphql".to_string();
        spec.kind = RequestKind::GraphQl(GraphQlRequest::default());

        let written = serde_json::to_string(&spec).expect("serialize");
        assert!(
            !written.contains("transport"),
            "an untouched transport must leave no trace in the file: {written}"
        );

        spec.kind = RequestKind::GraphQl(GraphQlRequest {
            transport: GraphQlTransport::WebSocket,
            ..GraphQlRequest::default()
        });
        let chosen = serde_json::to_string(&spec).expect("serialize");
        assert!(
            chosen.contains("WebSocket"),
            "one that was chosen has to survive the trip: {chosen}"
        );
        let back: RequestSpec = serde_json::from_str(&chosen).expect("deserialize");
        assert_eq!(back, spec);
    }

    /// The handshake is a GET and there is no choice to show, so nothing asks for one.
    #[test]
    fn a_websocket_request_reports_no_method() {
        let spec = RequestSpec {
            kind: RequestKind::WebSocket(WebSocketRequest::default()),
            ..RequestSpec::default()
        };
        assert_eq!(spec.method(), None);
        assert!(spec.http().is_none());
        assert_eq!(spec.kind.badge(), Some("WS"));
        assert!(spec.kind.is_session());
    }

    /// A field added to `WebSocketRequest` later must not orphan every socket written today.
    ///
    /// The `#[serde(default)]` on each field is what makes that possible, and a default is
    /// exactly the kind of attribute that gets dropped in a refactor without anything noticing
    /// — so this reads the *older* shape back rather than trusting the attribute is still there.
    #[test]
    fn a_socket_written_with_fewer_fields_still_opens() {
        let older = r#"{
            "id": 0,
            "name": "sub",
            "url": "wss://api.test/ws",
            "headers": [],
            "settings": {},
            "kind": { "WebSocket": {} }
        }"#;

        let spec: RequestSpec = serde_json::from_str(older).expect("an older socket must open");
        let socket = spec.websocket().expect("still a socket");
        assert!(socket.subprotocols.is_empty() && socket.messages.is_empty());
    }
}
