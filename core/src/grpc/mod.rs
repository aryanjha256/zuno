//! gRPC schema: turning a `.proto` file into something a person can compose a call against.
//!
//! **This is the expensive half of gRPC, and the transport is the cheap one.** A REST request can
//! be typed from nothing — a URL and a body — but a gRPC call cannot be composed at all without
//! knowing the service, the method, and the shape of its request message. So everything here is
//! about *learning a schema at runtime* and encoding against it, which is the opposite of how
//! protobuf is normally used in Rust (generated structs, known at build time).
//!
//! Two crates do the work and neither is `tonic`:
//!
//! - **`protox`** compiles `.proto` source to a `FileDescriptorSet`. It is a protobuf compiler
//!   written in Rust, so nothing has to be installed — `protoc` as a build-time or run-time
//!   dependency would make the `.deb` unshippable for the ordinary case of a person who has
//!   never heard of it.
//! - **`prost-reflect`** holds the descriptors and gives `DynamicMessage`, a message whose shape
//!   is decided at runtime. Its `serde` support is what lets someone type JSON and have it
//!   become protobuf bytes, and what turns the reply back into something readable.
//!
//! **JSON is the authoring format, deliberately.** protobuf's own text format exists and
//! `prost-reflect` can read it, but every other body in Zuno is JSON, the response viewer
//! already indexes and folds JSON, and `grpcurl` made JSON the thing people paste. The encoding
//! is protobuf; only what you type and what you read are JSON.

pub mod reflection;

use std::path::{Path, PathBuf};

use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor};
// The trait, for `DynamicMessage::serialize`. `prost::Message` above is the *encoding* half and
// this is the JSON half; both are traits and both are needed by name.
use serde::Serialize;

/// What went wrong between a `.proto` on disk and bytes on the wire.
///
/// Separate from `EngineError` because none of it is a network failure: every variant here is
/// answerable by editing a file or a message, and saying which is the whole value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrpcError {
    #[error("no .proto file chosen — a gRPC call cannot be composed without a schema")]
    NoSchema,
    #[error("{path} could not be read: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("{path} did not compile: {reason}")]
    Uncompilable { path: String, reason: String },
    #[error("the schema defines no services, so there is nothing to call")]
    NoServices,
    #[error("no method named {method} on {service}")]
    NoSuchMethod { service: String, method: String },
    #[error("no service named {service} in this schema")]
    NoSuchService { service: String },
    #[error("the request message is not valid JSON for {message}: {reason}")]
    BadRequest { message: String, reason: String },
    #[error("the reply could not be read as {message}: {reason}")]
    BadReply { message: String, reason: String },
}

/// One callable method, flattened for the UI.
///
/// A plain struct rather than a borrowed `MethodDescriptor` because the picker and the request
/// pane outlive any one compile, and a descriptor borrows its pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    /// `helloworld.Greeter`, which is also the first half of the `:path` header.
    pub service: String,
    /// `SayHello`, the second half.
    pub name: String,
    /// The request message's full name, shown so you know what you are filling in.
    pub input: String,
    pub output: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

impl Method {
    /// The HTTP/2 `:path` for this method.
    ///
    /// **A leading slash and exactly one separator.** gRPC does not route on anything else —
    /// there is no query string, no verb, and no content negotiation — so this one string is the
    /// entire address of a call.
    pub fn path(&self) -> String {
        format!("/{}/{}", self.service, self.name)
    }

    /// Which of gRPC's four shapes this is.
    ///
    /// Kept as a method rather than stored, because the two booleans come straight from the
    /// descriptor and a third field would be a second source of truth for the same fact.
    pub fn shape(&self) -> Shape {
        Shape::of(self.client_streaming, self.server_streaming)
    }
}

/// gRPC's four call shapes, as the method picker labels them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Unary,
    ServerStreaming,
    ClientStreaming,
    BidiStreaming,
}

impl Shape {
    /// From the two streaming flags — a descriptor's, or the copy a request stores to label
    /// itself. One mapping, so the picker and the request pane cannot name a shape differently.
    pub fn of(client_streaming: bool, server_streaming: bool) -> Self {
        match (client_streaming, server_streaming) {
            (false, false) => Shape::Unary,
            (false, true) => Shape::ServerStreaming,
            (true, false) => Shape::ClientStreaming,
            (true, true) => Shape::BidiStreaming,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Shape::Unary => "unary",
            Shape::ServerStreaming => "server streaming",
            Shape::ClientStreaming => "client streaming",
            Shape::BidiStreaming => "bidirectional streaming",
        }
    }
}

/// A compiled `.proto`, ready to encode against.
#[derive(Debug, Clone)]
pub struct Schema {
    pool: DescriptorPool,
}

impl Schema {
    /// Compile a `.proto` file and everything it imports.
    ///
    /// **The file's own directory is always an include path.** A `.proto` that imports a sibling
    /// is the ordinary case, and requiring the person to also name the directory they just
    /// picked a file out of would be a question with one possible answer.
    pub fn compile(path: impl AsRef<Path>) -> Result<Self, GrpcError> {
        let path = path.as_ref();
        let shown = path.display().to_string();

        if !path.exists() {
            return Err(GrpcError::Unreadable {
                path: shown,
                reason: "no such file".to_string(),
            });
        }

        // **Two kinds of schema file, told apart by extension.** A `.proto` is source and goes
        // through the compiler; a descriptor set is already compiled, which is what server
        // reflection returns and what `Engine::reflect` writes into `protos/`. Distinguishing
        // them by extension rather than by sniffing the bytes is the honest choice: a
        // descriptor set has no magic number, and "does this parse as protobuf" is a question
        // almost any file answers yes to by accident.
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("desc" | "protoset" | "pb")
        ) {
            let bytes = std::fs::read(path).map_err(|error| GrpcError::Unreadable {
                path: shown.clone(),
                reason: error.to_string(),
            })?;
            return Self::from_descriptor_set(&bytes).map_err(|error| match error {
                GrpcError::Uncompilable { reason, .. } => {
                    GrpcError::Uncompilable { path: shown, reason }
                }
                other => other,
            });
        }

        let include = path.parent().unwrap_or(Path::new("."));
        let files = protox::compile([path], [include]).map_err(|error| GrpcError::Uncompilable {
            path: shown.clone(),
            // protox's error carries the line and column, which is most of its value — a
            // compiler error with no position in it is a compiler error you have to hunt for.
            reason: error.to_string(),
        })?;

        let pool =
            DescriptorPool::from_file_descriptor_set(files).map_err(|error| {
                GrpcError::Uncompilable {
                    path: shown,
                    reason: error.to_string(),
                }
            })?;

        Ok(Self { pool })
    }

    /// Build a schema from an already-compiled `FileDescriptorSet`.
    ///
    /// What server reflection hands back, and what `compile` reads from a `.desc` file. The
    /// whole set at once rather than a file at a time, because a file cannot be added before
    /// the files it imports and reflection answers in whatever order it likes.
    pub fn from_descriptor_set(bytes: &[u8]) -> Result<Self, GrpcError> {
        let mut pool = DescriptorPool::new();
        pool.decode_file_descriptor_set(bytes)
            .map_err(|error| GrpcError::Uncompilable {
                path: "the schema from the server".to_string(),
                reason: error.to_string(),
            })?;
        Ok(Self { pool })
    }

    /// Every method the schema offers, in file order.
    ///
    /// Flat rather than grouped by service: the picker ranks across everything, and a person
    /// looking for `SayHello` rarely knows which service it is on — which is the same reason the
    /// request picker does not group by folder.
    pub fn methods(&self) -> Vec<Method> {
        // Collected per service rather than returned as a lazy iterator: `methods()` borrows the
        // `ServiceDescriptor`, which is a local, so the mapping has to finish inside the closure.
        self.pool
            .services()
            .flat_map(|service| {
                let service_name = service.full_name().to_string();
                service
                    .methods()
                    .map(|method| Method {
                        service: service_name.clone(),
                        name: method.name().to_string(),
                        input: method.input().full_name().to_string(),
                        output: method.output().full_name().to_string(),
                        client_streaming: method.is_client_streaming(),
                        server_streaming: method.is_server_streaming(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Encode a JSON request body into protobuf bytes for `method`.
    ///
    /// **An empty body means the default message, not an error.** Plenty of RPCs take
    /// `google.protobuf.Empty` or a message with every field optional, and making someone type
    /// `{}` to call one would be ceremony.
    pub fn encode(&self, method: &Method, json: &str) -> Result<Vec<u8>, GrpcError> {
        let descriptor = self.method(method)?;
        let input = descriptor.input();

        let trimmed = json.trim();
        if trimmed.is_empty() {
            return Ok(DynamicMessage::new(input).encode_to_vec());
        }

        let mut deserializer = serde_json::Deserializer::from_str(trimmed);
        let message = DynamicMessage::deserialize(input.clone(), &mut deserializer).map_err(
            |error| GrpcError::BadRequest {
                message: input.full_name().to_string(),
                reason: error.to_string(),
            },
        )?;
        // **Checked rather than ignored.** `Deserializer::end` is what catches trailing rubbish
        // after a complete message — `{"a":1} oops` deserializes fine and would otherwise be
        // sent as if the tail had never been typed.
        deserializer
            .end()
            .map_err(|error| GrpcError::BadRequest {
                message: input.full_name().to_string(),
                reason: error.to_string(),
            })?;

        Ok(message.encode_to_vec())
    }

    /// Decode a protobuf reply into JSON.
    ///
    /// Pretty-printed, because this is what lands in the response viewer and the viewer's whole
    /// value — folding, search, row selection — is over a structure a person can read.
    pub fn decode(&self, method: &Method, bytes: &[u8]) -> Result<String, GrpcError> {
        let descriptor = self.method(method)?;
        let output = descriptor.output();

        let message = DynamicMessage::decode(output.clone(), bytes).map_err(|error| {
            GrpcError::BadReply {
                message: output.full_name().to_string(),
                reason: error.to_string(),
            }
        })?;

        let mut buffer = Vec::new();
        let mut serializer = serde_json::Serializer::pretty(&mut buffer);
        message
            .serialize(&mut serializer)
            .map_err(|error| GrpcError::BadReply {
                message: output.full_name().to_string(),
                reason: error.to_string(),
            })?;

        String::from_utf8(buffer).map_err(|error| GrpcError::BadReply {
            message: output.full_name().to_string(),
            reason: error.to_string(),
        })
    }

    fn method(&self, method: &Method) -> Result<MethodDescriptor, GrpcError> {
        let service = self
            .pool
            .services()
            .find(|service| service.full_name() == method.service)
            .ok_or_else(|| GrpcError::NoSuchService {
                service: method.service.clone(),
            })?;

        service
            .methods()
            .find(|candidate| candidate.name() == method.name)
            .ok_or_else(|| GrpcError::NoSuchMethod {
                service: method.service.clone(),
                method: method.name.clone(),
            })
    }
}

/// The reserved directory a collection keeps its schemas in.
///
/// **Reserved like `environments/` and `flows/`**, and for the reason those are: `collection::scan`
/// walks everything, and a directory of `.proto` files is not a directory of requests.
pub const DIRECTORY: &str = "protos";

/// Where a request's `.proto` actually lives.
///
/// **A bare filename means the collection's `protos/`**, and that is what keeps a committed
/// collection portable: an absolute path into one person's home directory is broken for every
/// teammate who clones the repo — the same failure invariant 10 exists to prevent for secrets.
/// Anything carrying a separator is used as written, so a scratch tab that has never been saved
/// into a collection can still point at a file anywhere.
pub fn resolve_proto(proto: &str, collection: Option<&Path>) -> PathBuf {
    let proto = proto.trim();
    let bare = !proto.contains(std::path::MAIN_SEPARATOR) && !proto.contains('/');
    match collection.filter(|_| bare) {
        Some(root) => root.join(DIRECTORY).join(proto),
        None => PathBuf::from(proto),
    }
}

/// Compile the schema and find the method a request names.
///
/// Separate from `call` so the app can reuse it to fill a method picker without sending
/// anything — and so the failure a person is most likely to hit, a `.proto` that does not
/// compile, is reportable before a socket is opened.
pub fn prepare(grpc: &crate::request::GrpcRequest, collection: Option<&Path>) -> Result<(Schema, Method), GrpcError> {
    if grpc.proto.trim().is_empty() {
        return Err(GrpcError::NoSchema);
    }

    let schema = Schema::compile(resolve_proto(&grpc.proto, collection))?;
    let methods = schema.methods();
    if methods.is_empty() {
        return Err(GrpcError::NoServices);
    }

    let method = methods
        .into_iter()
        .find(|candidate| candidate.service == grpc.service && candidate.name == grpc.method)
        .ok_or_else(|| GrpcError::NoSuchMethod {
            service: grpc.service.clone(),
            method: grpc.method.clone(),
        })?;

    Ok((schema, method))
}

/// The 5-byte prefix in front of every gRPC message on the wire.
///
/// **One compression flag and a big-endian u32 length**, and that is the whole of gRPC's framing.
/// Written here rather than taken from a dependency because it is nine lines and owning it is
/// what keeps `tonic` — with its own client, its own codegen and its own connection pool — out
/// of a tree that already has a working HTTP client.
pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + message.len());
    out.push(0);
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// A ceiling on one gRPC message, matching the SSE parser's and the WebSocket's.
///
/// **A stream declares no length**, so nothing upstream bounds a server-streaming reply: the
/// engine's `max_body_bytes` guard watches a buffer that a stream never fills. A server that
/// declares a 4 GiB message in a five-byte header would otherwise have that reserved for it.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Reassemble gRPC messages from a stream of chunks.
///
/// **Streaming needs this and a whole-body `unframe` cannot do it**: a message is length-prefixed
/// and arrives split across HTTP/2 DATA frames however the network felt like splitting it, so
/// half a header at the end of one chunk has to be held until the next. The same shape as
/// `sse::Parser`, and bounded for the same reason.
pub struct Reader {
    pending: Vec<u8>,
    limit: usize,
    refused: Option<Refused>,
}

/// Why a stream stopped being read.
///
/// **One flag with two reasons, not two flags.** Both are terminal and both mean the same thing
/// to the caller — tear the stream down and say why — so a pair of booleans would allow a state
/// where the stream is both and neither answer is the one reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// A message declared more bytes than will be held. Carries the declared length, which is
    /// the useful number: it is what the *server* claimed, not what arrived.
    TooLarge(usize),
    /// The compression flag was set. Nothing negotiates compression — `grpc-accept-encoding:
    /// identity` goes out on every call — so this means the server ignored it, and decoding the
    /// message as plaintext would produce convincing rubbish rather than an error.
    Compressed,
}

impl Refused {
    pub fn reason(self) -> String {
        match self {
            Refused::TooLarge(len) => {
                format!("a message declared {len} bytes, which is more than will be held")
            }
            Refused::Compressed => {
                "the server sent a compressed message, which was not negotiated".to_string()
            }
        }
    }
}

impl Default for Reader {
    fn default() -> Self {
        Self::with_limit(MAX_MESSAGE_BYTES)
    }
}

impl Reader {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            limit,
            refused: None,
        }
    }

    /// Why the stream stopped, once it has.
    ///
    /// **Read after every `push`.** Terminal: nothing more is parsed, and the caller tears the
    /// stream down. The same contract `sse::Parser::too_large` has, for the same reason — there
    /// is no error channel out of a function that returns the messages a chunk completed.
    pub fn refused(&self) -> Option<Refused> {
        self.refused
    }

    /// Feed one chunk. Returns whichever messages it completed, usually none or one.
    ///
    /// Owned `Vec<u8>` rather than borrowed slices, unlike `unframe`: a message that spanned two
    /// chunks does not exist contiguously in either of them, so there is nothing to borrow from.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        if self.refused.is_some() {
            return Vec::new();
        }
        self.pending.extend_from_slice(chunk);

        let mut messages = Vec::new();
        let mut at = 0;
        loop {
            let Some(head) = self.pending.get(at..at + 5) else {
                break;
            };
            if head[0] != 0 {
                self.refused = Some(Refused::Compressed);
                self.pending.clear();
                return messages;
            }
            let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;

            // **Checked against the declared length, before anything is held.** The whole point
            // of a cap here is the header that claims more than will ever arrive; waiting for
            // the bytes to show up first is waiting for the thing being prevented.
            if len > self.limit {
                self.refused = Some(Refused::TooLarge(len));
                self.pending.clear();
                return messages;
            }

            let Some(message) = self.pending.get(at + 5..at + 5 + len) else {
                break;
            };
            messages.push(message.to_vec());
            at += 5 + len;
        }

        self.pending.drain(..at);
        messages
    }
}

/// Split a gRPC body into its messages.
///
/// **A body can hold more than one**, which is what makes server streaming work without a
/// second code path: a unary reply is simply a body with one message in it. Returns what it
/// could read — a truncated tail is a broken stream, and the messages before it still happened.
pub fn unframe(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut messages = Vec::new();
    while let Some((head, rest)) = bytes.split_at_checked(5) {
        // A set flag means the message is compressed, and nothing here negotiates compression —
        // `grpc-encoding` is never sent, so a server answering with one is out of contract.
        // Stopping is right: decoding it as plaintext would produce convincing rubbish.
        if head[0] != 0 {
            break;
        }
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let Some(message) = rest.get(..len) else {
            break;
        };
        messages.push(message);
        bytes = &rest[len..];
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_filename_resolves_into_the_collections_protos_directory() {
        let root = Path::new("/home/someone/api");

        // The portable case: what a committed collection file should carry.
        assert_eq!(
            resolve_proto("greeter.proto", Some(root)),
            PathBuf::from("/home/someone/api/protos/greeter.proto")
        );

        // **A path is honoured as written**, so a scratch tab works before it has a collection.
        assert_eq!(
            resolve_proto("/tmp/elsewhere/g.proto", Some(root)),
            PathBuf::from("/tmp/elsewhere/g.proto")
        );
        assert_eq!(
            resolve_proto("sub/g.proto", Some(root)),
            PathBuf::from("sub/g.proto")
        );

        // No collection at all: nothing to resolve against, so it stays as typed.
        assert_eq!(
            resolve_proto("greeter.proto", None),
            PathBuf::from("greeter.proto")
        );
    }


    /// A schema written to a scratch dir, because `protox` reads files rather than strings.
    ///
    /// Named per test and per process, the same as `collection.rs`'s `scratch` — the suite runs
    /// tests in parallel and two of these writing one path would be a flake with no obvious
    /// cause.
    fn fixture(name: &str, source: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zuno-grpc-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("test.proto");
        std::fs::write(&path, source).expect("write");
        path
    }

    const GREETER: &str = r#"
        syntax = "proto3";
        package helloworld;

        message HelloRequest { string name = 1; int32 times = 2; }
        message HelloReply { string message = 1; }

        service Greeter {
          rpc SayHello (HelloRequest) returns (HelloReply);
          rpc StreamHellos (HelloRequest) returns (stream HelloReply);
          rpc SendHellos (stream HelloRequest) returns (HelloReply);
          rpc Chat (stream HelloRequest) returns (stream HelloReply);
        }
    "#;

    #[test]
    fn a_schema_lists_every_method_with_its_shape() {
        let path = fixture("shapes", GREETER);
        let schema = Schema::compile(&path).expect("the fixture must compile");
        let methods = schema.methods();

        assert_eq!(methods.len(), 4);
        assert_eq!(methods[0].service, "helloworld.Greeter");
        assert_eq!(methods[0].name, "SayHello");
        assert_eq!(methods[0].input, "helloworld.HelloRequest");
        assert_eq!(methods[0].path(), "/helloworld.Greeter/SayHello");

        // **All four shapes, read off the descriptor rather than guessed.** This is the one
        // place in Zuno where a call's lifecycle is knowable before connecting — SSE has to
        // wait for a content type and a WebSocket promises it in the URL scheme.
        assert_eq!(methods[0].shape(), Shape::Unary);
        assert_eq!(methods[1].shape(), Shape::ServerStreaming);
        assert_eq!(methods[2].shape(), Shape::ClientStreaming);
        assert_eq!(methods[3].shape(), Shape::BidiStreaming);
    }

    /// **JSON in, protobuf out, JSON back** — the whole authoring loop, without a socket.
    ///
    /// Asserted on the *bytes*, not only on a round trip. A round trip through one schema agrees
    /// with itself whatever it does, which is the shape of weak assertion this repo has been
    /// caught by six times; the wire format is fixed and small enough to check by hand.
    #[test]
    fn a_json_body_becomes_protobuf_and_reads_back() {
        let path = fixture("roundtrip", GREETER);
        let schema = Schema::compile(&path).expect("compile");
        let method = schema.methods().into_iter().next().expect("a method");

        let encoded = schema
            .encode(&method, r#"{"name": "zuno", "times": 3}"#)
            .expect("a valid body must encode");

        // Field 1, wire type 2 (`0x0a`), four bytes, `zuno`; then field 2, wire type 0
        // (`0x10`), varint 3.
        assert_eq!(
            encoded,
            vec![0x0a, 0x04, b'z', b'u', b'n', b'o', 0x10, 0x03],
            "protobuf's wire format is fixed, so this is checkable rather than merely stable"
        );

        // **Decoding uses the method's `output`, not its input**, which is why this feeds bytes
        // shaped like a `HelloReply` rather than re-reading what was just encoded.
        let decoded = schema
            .decode(&method, &[0x0a, 0x02, b'h', b'i'])
            .expect("a well-formed reply must decode");
        assert!(
            decoded.contains("\"message\""),
            "the reply has to come back as readable JSON: {decoded}"
        );
        assert!(decoded.contains("hi"));
    }

    /// **An empty body is a real request**, not a mistake. Plenty of RPCs take no arguments.
    #[test]
    fn an_empty_body_encodes_as_the_default_message() {
        let path = fixture("empty", GREETER);
        let schema = Schema::compile(&path).expect("compile");
        let method = schema.methods().into_iter().next().expect("a method");

        // An empty protobuf message encodes to zero bytes, which is the point: the wire says
        // "all defaults" by saying nothing.
        let empty: Vec<u8> = Vec::new();
        assert_eq!(schema.encode(&method, "").expect("empty is valid"), empty);
        assert_eq!(schema.encode(&method, "   ").expect("blank is valid"), empty);
        assert_eq!(schema.encode(&method, "{}").expect("{} is valid"), empty);
    }

    /// Every way the authoring loop can be wrong, and each one says which.
    #[test]
    fn a_bad_schema_or_body_says_what_is_wrong() {
        let path = fixture("bad", "syntax = \"proto3\"; this is not protobuf");
        let error = Schema::compile(&path).expect_err("rubbish must not compile");
        assert!(matches!(error, GrpcError::Uncompilable { .. }), "{error:?}");

        let missing = Schema::compile("/nonexistent/none.proto").expect_err("no file");
        assert!(matches!(missing, GrpcError::Unreadable { .. }), "{missing:?}");

        let path = fixture("errors", GREETER);
        let schema = Schema::compile(&path).expect("compile");
        let method = schema.methods().into_iter().next().expect("a method");

        let bad = schema.encode(&method, "{ not json").expect_err("invalid JSON");
        assert!(matches!(bad, GrpcError::BadRequest { .. }), "{bad:?}");

        // A field the message does not have is a typo, and protobuf JSON rejects it rather than
        // dropping it — which is what you want when the field you meant is one letter away.
        let unknown = schema
            .encode(&method, r#"{"nmae": "typo"}"#)
            .expect_err("an unknown field is a typo, not a no-op");
        assert!(matches!(unknown, GrpcError::BadRequest { .. }), "{unknown:?}");

        // **Trailing rubbish after a complete message.** Without `Deserializer::end` this
        // encodes happily and sends only the first object.
        let trailing = schema
            .encode(&method, r#"{"name":"a"} and then some"#)
            .expect_err("trailing input must not be ignored");
        assert!(matches!(trailing, GrpcError::BadRequest { .. }), "{trailing:?}");

        let wrong = Method {
            name: "NoSuchThing".to_string(),
            ..method.clone()
        };
        assert!(matches!(
            schema.encode(&wrong, "{}").expect_err("no such method"),
            GrpcError::NoSuchMethod { .. }
        ));
    }

    /// **The case a whole-body `unframe` cannot handle**: a message split across chunks.
    ///
    /// HTTP/2 splits DATA frames wherever it likes, so a five-byte header landing across a
    /// boundary is ordinary rather than pathological. Driven one byte at a time, which is the
    /// worst split there is and the one that catches an implementation that assumed a header
    /// arrives whole.
    #[test]
    fn a_reader_reassembles_messages_split_across_chunks() {
        let mut body = frame(b"one");
        body.extend_from_slice(&frame(b"two"));

        let mut reader = Reader::default();
        let mut got: Vec<Vec<u8>> = Vec::new();
        for byte in &body {
            got.extend(reader.push(&[*byte]));
        }
        assert_eq!(got, vec![b"one".to_vec(), b"two".to_vec()]);
        assert!(reader.refused().is_none());

        // And the whole body in one go, which must agree.
        let mut whole = Reader::default();
        assert_eq!(whole.push(&body), vec![b"one".to_vec(), b"two".to_vec()]);

        // A trailing partial message is held, not guessed at.
        let mut partial = Reader::default();
        assert!(partial.push(&[0, 0, 0, 0, 9, b'h', b'i']).is_empty());
        assert!(partial.refused().is_none(), "incomplete is not refused");
    }

    /// **The two ways a stream is refused, and both are terminal.**
    ///
    /// The size check reads the *declared* length rather than waiting for the bytes: a header
    /// claiming four gigabytes is exactly what the cap exists for, and waiting to see whether
    /// they arrive is waiting for the thing being prevented.
    #[test]
    fn a_reader_refuses_an_oversized_or_compressed_message() {
        let mut big = Reader::with_limit(64);
        assert!(big.push(&[0, 0, 0, 4, 0]).is_empty(), "declares 1024 bytes");
        assert_eq!(big.refused(), Some(Refused::TooLarge(1024)));
        assert!(
            big.push(&frame(b"later")).is_empty(),
            "once refused it stays refused"
        );

        let mut zipped = Reader::default();
        assert!(zipped.push(&[1, 0, 0, 0, 2, b'h', b'i']).is_empty());
        assert_eq!(zipped.refused(), Some(Refused::Compressed));

        // A message exactly at the limit is fine — the cap is a ceiling, not a target.
        let mut edge = Reader::with_limit(2);
        assert_eq!(edge.push(&frame(b"hi")), vec![b"hi".to_vec()]);
        assert!(edge.refused().is_none());
    }

    /// **`unframe` and `Reader` must agree**, because they are two implementations of one wire
    /// format and nothing else would notice them drifting apart.
    #[test]
    fn the_whole_body_and_streaming_readers_agree() {
        let mut body = frame(b"a");
        body.extend_from_slice(&frame(b""));
        body.extend_from_slice(&frame(b"ccc"));

        let borrowed: Vec<Vec<u8>> = unframe(&body).into_iter().map(<[u8]>::to_vec).collect();
        let streamed = Reader::default().push(&body);
        assert_eq!(borrowed, streamed);

        // Including where they stop: a truncated tail keeps what arrived.
        let mut cut = frame(b"kept");
        cut.extend_from_slice(&[0, 0, 0, 0, 99, b'x']);
        let borrowed: Vec<Vec<u8>> = unframe(&cut).into_iter().map(<[u8]>::to_vec).collect();
        assert_eq!(borrowed, Reader::default().push(&cut));
    }

    /// **The framing, both ways, including the case that makes streaming free.**
    #[test]
    fn framing_round_trips_and_a_body_may_hold_several_messages() {
        assert_eq!(frame(b"hi"), vec![0, 0, 0, 0, 2, b'h', b'i']);
        assert_eq!(unframe(&frame(b"hi")), vec![&b"hi"[..]]);

        // Two messages in one body is exactly what a server-streaming reply looks like, and it
        // is why a unary reply needs no separate path — it is this with one element.
        let mut body = frame(b"one");
        body.extend_from_slice(&frame(b"two"));
        assert_eq!(unframe(&body), vec![&b"one"[..], &b"two"[..]]);

        // A truncated tail keeps what arrived rather than discarding the lot.
        let mut cut = frame(b"kept");
        cut.extend_from_slice(&[0, 0, 0, 0, 99, b'x']);
        assert_eq!(unframe(&cut), vec![&b"kept"[..]]);

        // A compression flag we never negotiated stops the read. Decoding it as plaintext
        // would hand the viewer convincing rubbish.
        assert!(unframe(&[1, 0, 0, 0, 2, b'h', b'i']).is_empty());

        assert!(unframe(&[]).is_empty());
        assert!(unframe(&[0, 0]).is_empty(), "a partial header is not a message");
    }
}
