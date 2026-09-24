//! gRPC server reflection: asking a server to describe itself.
//!
//! **The point is that a `.proto` file is often not to hand.** A server you are debugging is
//! frequently somebody else's, and reflection is how `grpcurl` and Postman call one without
//! being given a schema first. It is also why reflection was left until last: the protocol is
//! itself a *bidirectional streaming* call, so implementing it first would have meant building
//! gRPC's hardest shape before any simpler one worked.
//!
//! **Only the envelope is hand-decoded here, and that is deliberate.** The reflection messages
//! are tiny — a request is one string field, a response is one of four — and writing a protobuf
//! decoder for them is about sixty lines. What comes back *inside* them is a
//! `FileDescriptorProto`, which is anything but tiny, and that goes straight to
//! `DescriptorPool::decode_file_descriptor_proto`. So the hard parsing stays in prost-reflect
//! and the part written by hand is small enough to check against the spec by eye.
//!
//! The service is `grpc.reflection.v1.ServerReflection`, with `v1alpha` as the older name that
//! a great many deployed servers still answer on and nothing else.

/// The v1 service path. Tried first.
pub const V1_PATH: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";

/// The original name, which is still what most deployed servers answer on.
///
/// **Tried when v1 answers `UNIMPLEMENTED`**, which is exactly how a server that only speaks the
/// older one refuses — so the fallback costs one round trip and no guessing.
pub const V1ALPHA_PATH: &str =
    "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo";

/// `ServerReflectionRequest { list_services: "*" }`.
///
/// Field 7, wire type 2. **The value is `"*"` and not the empty string**, which the spec says is
/// ignored and reality says is not: a server whose handler checks the field for truthiness — a
/// JavaScript implementation, say — sees `""` as *unset* and answers nothing at all. Measured
/// against `grpc.postman-echo.com`, which returns an empty body for `""` and its full service
/// list for `"*"`, with `grpc-status: 0` both times. So the failure is silent.
///
/// `"*"` is what the clients that work against that server send, and it is meaningless to any
/// implementation that reads the spec literally — which makes it the value that works with both.
pub fn list_services() -> Vec<u8> {
    vec![(7 << 3) | 2, 1, b'*']
}

/// `ServerReflectionRequest { file_containing_symbol: name }`.
///
/// Field 4. Asking by *symbol* rather than by filename is what makes this work without knowing
/// anything about the server's file layout: the response carries the file that defines the
/// symbol **and every file it depends on**, which is the whole schema for that service.
pub fn file_containing_symbol(name: &str) -> Vec<u8> {
    let mut out = vec![(4 << 3) | 2];
    put_varint(&mut out, name.len() as u64);
    out.extend_from_slice(name.as_bytes());
    out
}

/// Wrap collected `FileDescriptorProto`s into a `FileDescriptorSet`.
///
/// **A `FileDescriptorSet` is `repeated FileDescriptorProto file = 1`, and nothing else** — so
/// building one is wrapping each element in a field-1 header rather than re-encoding anything.
/// The descriptors themselves stay opaque, which is what keeps the hand-written protobuf here
/// to the envelope.
///
/// Given to `DescriptorPool::decode_file_descriptor_set` rather than added one at a time,
/// because a file cannot be added before the files it imports and reflection answers in whatever
/// order it likes. Handing over the whole set lets prost-reflect sort that out.
pub fn descriptor_set(files: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for file in files {
        put_varint(&mut out, (1 << 3) | 2);
        put_varint(&mut out, file.len() as u64);
        out.extend_from_slice(file);
    }
    out
}

/// What one `ServerReflectionResponse` carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// `list_services_response` — the fully-qualified service names.
    Services(Vec<String>),
    /// `file_descriptor_response` — serialized `FileDescriptorProto`s, which prost-reflect reads.
    Files(Vec<Vec<u8>>),
    /// `error_response`. Carries gRPC's own status codes, so `NOT_FOUND` here means the symbol
    /// is unknown rather than that the call failed.
    Error { code: i32, message: String },
    /// A response shape we do not ask for. Ignored rather than refused, for the reason
    /// `session::step` ignores unknown envelope types: a client that dies on something it has
    /// not heard of breaks when the server is upgraded.
    Other,
}

/// Read one `ServerReflectionResponse`.
pub fn response(bytes: &[u8]) -> Response {
    let mut at = 0;
    while let Some((field, wire, next)) = tag(bytes, at) {
        at = next;
        let Some((value, next)) = payload(bytes, at, wire) else {
            return Response::Other;
        };
        at = next;

        match field {
            // `file_descriptor_response`
            4 => return Response::Files(repeated_bytes(value)),
            // `list_services_response`: repeated ServiceResponse, each with a `name` at field 1.
            6 => {
                return Response::Services(
                    repeated_bytes(value)
                        .into_iter()
                        .filter_map(|service| {
                            first_string(&service).filter(|name| !name.is_empty())
                        })
                        .collect(),
                );
            }
            // `error_response`
            7 => return error(value),
            _ => {}
        }
    }
    Response::Other
}

/// Every occurrence of field 1, as bytes.
///
/// Both `FileDescriptorResponse` and `ListServiceResponse` are a single repeated field 1, which
/// is why one helper reads both.
fn repeated_bytes(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some((field, wire, next)) = tag(bytes, at) {
        at = next;
        let Some((value, next)) = payload(bytes, at, wire) else {
            break;
        };
        at = next;
        if field == 1 && wire == 2 {
            out.push(value.to_vec());
        }
    }
    out
}

/// The first length-delimited field 1, as a string — a `ServiceResponse`'s `name`.
fn first_string(bytes: &[u8]) -> Option<String> {
    let mut at = 0;
    while let Some((field, wire, next)) = tag(bytes, at) {
        at = next;
        let (value, next) = payload(bytes, at, wire)?;
        at = next;
        if field == 1 && wire == 2 {
            return String::from_utf8(value.to_vec()).ok();
        }
    }
    None
}

/// `ErrorResponse { int32 error_code = 1; string error_message = 2; }`.
///
/// One pass, taking whichever of the two fields it meets. Unknown fields are skipped by wire
/// type rather than assumed absent, which is what keeps the position in step when a server adds
/// one.
fn error(bytes: &[u8]) -> Response {
    let mut code = 0;
    let mut message = String::new();
    let mut at = 0;

    while let Some((field, wire, next)) = tag(bytes, at) {
        at = next;
        let Some((value, next)) = payload(bytes, at, wire) else {
            break;
        };
        at = next;

        match (field, wire) {
            (1, 0) => {
                if let Some((number, _)) = varint(value, 0) {
                    code = number as i32;
                }
            }
            (2, 2) => message = String::from_utf8_lossy(value).into_owned(),
            _ => {}
        }
    }

    Response::Error { code, message }
}

/// Read a tag: the field number and wire type.
fn tag(bytes: &[u8], at: usize) -> Option<(u64, u8, usize)> {
    let (value, next) = varint(bytes, at)?;
    Some((value >> 3, (value & 7) as u8, next))
}

/// Read whatever a wire type says follows, returning it as bytes and the new position.
///
/// **Total over the five wire types that exist**, including the two group markers that were
/// deprecated decades ago — a decoder that guesses at an unknown one desynchronises and then
/// reads rubbish confidently, which is worse than stopping.
fn payload(bytes: &[u8], at: usize, wire: u8) -> Option<(&[u8], usize)> {
    match wire {
        // varint
        0 => {
            let (_, next) = varint(bytes, at)?;
            Some((&bytes[at..next], next))
        }
        // 64-bit
        1 => Some((bytes.get(at..at + 8)?, at + 8)),
        // length-delimited
        2 => {
            let (len, next) = varint(bytes, at)?;
            let end = next.checked_add(len as usize)?;
            Some((bytes.get(next..end)?, end))
        }
        // 32-bit
        5 => Some((bytes.get(at..at + 4)?, at + 4)),
        // Groups: legal on the wire, never emitted by anything here, and not worth decoding.
        _ => None,
    }
}

fn varint(bytes: &[u8], mut at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(at)?;
        at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, at));
        }
        shift += 7;
        // A varint longer than ten bytes cannot fit a u64 and means the stream is desynchronised.
        if shift >= 64 {
            return None;
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A length-delimited field, for building fixtures the way a server would.
    fn field(number: u64, value: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        put_varint(&mut out, (number << 3) | 2);
        put_varint(&mut out, value.len() as u64);
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn a_request_is_encoded_the_way_the_spec_describes() {
        // Field 7, wire type 2, one byte, `*`. Checkable by hand, which is the point of
        // asserting on bytes rather than round-tripping through our own decoder — and this is
        // the assertion that would have caught sending an empty value.
        assert_eq!(list_services(), vec![0x3a, 0x01, b'*']);

        // Field 4, wire type 2, seven bytes.
        assert_eq!(
            file_containing_symbol("pkg.Svc"),
            vec![0x22, 0x07, b'p', b'k', b'g', b'.', b'S', b'v', b'c']
        );

        // A name long enough to need a two-byte length, which is where a hand-rolled varint
        // goes wrong.
        let long = "a".repeat(200);
        let encoded = file_containing_symbol(&long);
        assert_eq!(&encoded[..3], &[0x22, 0xc8, 0x01]);
        assert_eq!(encoded.len(), 3 + 200);
    }

    #[test]
    fn a_service_list_is_read_back() {
        // `ListServiceResponse { service: [ServiceResponse{name}, …] }` — each service is a
        // nested message at field 1, whose own field 1 is the name.
        let list = [
            field(1, &field(1, b"a.B")),
            field(1, &field(1, b"c.D")),
        ]
        .concat();

        assert_eq!(
            response(&field(6, &list)),
            Response::Services(vec!["a.B".to_string(), "c.D".to_string()])
        );
    }

    #[test]
    fn file_descriptors_come_back_as_opaque_bytes() {
        // FileDescriptorResponse { file_descriptor_proto: [b"one", b"two"] } at field 4.
        let inner = [field(1, b"one"), field(1, b"two")].concat();
        assert_eq!(
            response(&field(4, &inner)),
            Response::Files(vec![b"one".to_vec(), b"two".to_vec()])
        );
    }

    /// **An error is a response, not a failure.** A server that does not know a symbol answers
    /// `NOT_FOUND` *inside* a successful stream, so treating the call's status as the verdict
    /// would report "the schema has no such service" as "the server is broken".
    #[test]
    fn an_error_response_carries_its_code_and_message() {
        let mut inner = vec![];
        put_varint(&mut inner, 1 << 3); // field 1, varint
        put_varint(&mut inner, 5); // NOT_FOUND
        inner.extend_from_slice(&field(2, b"symbol not found"));

        assert_eq!(
            response(&field(7, &inner)),
            Response::Error {
                code: 5,
                message: "symbol not found".to_string(),
            }
        );
    }

    /// A shape we never ask for, and rubbish, both read as `Other` rather than panicking.
    #[test]
    fn an_unknown_or_malformed_response_is_ignored_rather_than_fatal() {
        // `all_extension_numbers_response`, field 5, which nothing here requests.
        assert_eq!(response(&field(5, b"whatever")), Response::Other);
        assert_eq!(response(&[]), Response::Other);
        // A length that runs off the end.
        assert_eq!(response(&[0x22, 0x7f, b'a']), Response::Other);
        // A varint with the continuation bit set forever.
        assert_eq!(response(&[0xff; 12]), Response::Other);
    }
}
