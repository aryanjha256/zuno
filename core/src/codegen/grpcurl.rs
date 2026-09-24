//! A `grpcurl` command — the one runnable export for a gRPC call.
//!
//! Read from the gRPC half of the spec rather than a `Wire`: there is no HTTP-shaped description
//! of a gRPC call worth having, since its body is a protobuf message only a schema can encode.
//! The decisions that matter are made by the engine's own functions so the command reaches the
//! same place — `grpc_url` for the address and its loopback-plaintext rule, `resolve_proto` for
//! where a bare `greeter.proto` lives.

use std::path::Path;

use super::shell_quote;
use crate::engine::build;
use crate::request::{RequestSettings, RequestSpec};

pub(super) fn render(spec: &RequestSpec, collection: Option<&Path>) -> Option<String> {
    let grpc = spec.grpc()?;
    let (service, method) = (grpc.service.trim(), grpc.method.trim());
    if service.is_empty() || method.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = vec!["grpcurl".to_string()];

    let (address, plaintext) = address(spec)?;
    if plaintext {
        parts.push("-plaintext".to_string());
    } else if !spec.settings.verify_tls {
        parts.push("-insecure".to_string());
    }

    // No schema means grpcurl asks the server by reflection, which is what an empty field means
    // here too. A descriptor set and a `.proto` are different flags, told apart by extension for
    // the reason `Schema::compile` gives.
    let proto = grpc.proto.trim();
    if !proto.is_empty() {
        let path = crate::grpc::resolve_proto(proto, collection);
        let descriptor_set = matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("desc" | "protoset" | "pb")
        );
        if descriptor_set {
            parts.push(format!("-protoset {}", shell_quote(&path.display().to_string())));
        } else {
            // The file's own directory as the import path, as `Schema::compile` does — so a
            // `.proto` importing a sibling resolves the same way in both.
            let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
            if let Some(dir) = dir {
                parts.push(format!("-import-path {}", shell_quote(&dir.display().to_string())));
            }
            let file = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            parts.push(format!("-proto {}", shell_quote(&file)));
        }
    }

    for header in spec.enabled_headers() {
        if header.name.trim().is_empty() {
            continue;
        }
        parts.push(format!(
            "-H {}",
            shell_quote(&format!("{}: {}", header.name.trim(), header.value))
        ));
    }

    if let Some(timeout) = spec.settings.timeout
        && Some(timeout) != RequestSettings::default().timeout
    {
        parts.push(format!("-max-time {}", timeout.as_secs()));
    }

    if !grpc.message.trim().is_empty() {
        parts.push(format!("-d {}", shell_quote(grpc.message.trim())));
    }

    parts.push(shell_quote(&address));
    parts.push(shell_quote(&format!("{service}/{method}")));
    Some(parts.join(" \\\n  "))
}

/// `host:port`, and whether it is plaintext.
///
/// Through `grpc_url` so a scheme-less loopback address is plaintext here exactly when the engine
/// makes it plaintext. That fails for a URL still holding a withheld `{{secret}}`, which is a
/// normal outcome for a copied command; the fallback reads the text as typed.
fn address(spec: &RequestSpec) -> Option<(String, bool)> {
    if let Ok(url) = build::grpc_url(spec, "/") {
        let host = url.host_str()?;
        let port = url.port_or_known_default()?;
        return Some((format!("{host}:{port}"), url.scheme() == "http"));
    }

    let raw = spec.url.trim();
    let (rest, plaintext) = match raw.split_once("://") {
        Some((scheme, rest)) => (rest, scheme.eq_ignore_ascii_case("http")),
        None => (raw, false),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    (!authority.is_empty()).then(|| (authority.to_string(), plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{GrpcRequest, Header, RequestKind};

    fn spec(url: &str, proto: &str) -> RequestSpec {
        let mut spec = RequestSpec::default();
        spec.url = url.to_string();
        spec.headers = vec![Header::new("authorization", "Bearer {{token}}")];
        spec.kind = RequestKind::Grpc(GrpcRequest {
            proto: proto.to_string(),
            service: "helloworld.Greeter".into(),
            method: "SayHello".into(),
            message: r#"{"name": "zuno"}"#.into(),
            client_streaming: false,
            server_streaming: false,
        });
        spec
    }

    #[test]
    fn a_call_names_its_schema_address_and_method() {
        let out = render(&spec("localhost:50051", "greeter.proto"), Some(Path::new("/work/api")))
            .expect("a gRPC call renders");
        assert_eq!(
            out,
            "grpcurl \\\n  -plaintext \\\n  -import-path '/work/api/protos' \\\n  \
             -proto 'greeter.proto' \\\n  -H 'authorization: Bearer {{token}}' \\\n  \
             -d '{\"name\": \"zuno\"}' \\\n  'localhost:50051' \\\n  'helloworld.Greeter/SayHello'"
        );
    }

    /// A reflected schema is a descriptor set, which grpcurl reads with a different flag; and no
    /// schema at all means grpcurl reflects, so nothing is emitted for one.
    #[test]
    fn a_descriptor_set_or_no_schema_is_said_the_way_grpcurl_reads_it() {
        let out = render(&spec("https://api.test", "api.test.desc"), Some(Path::new("/w")))
            .expect("renders");
        assert!(out.contains("-protoset '/w/protos/api.test.desc'"), "{out}");
        assert!(out.contains("'api.test:443'") && !out.contains("-plaintext"), "{out}");

        let out = render(&spec("https://api.test", ""), None).expect("renders");
        assert!(!out.contains("-proto") && !out.contains("-protoset"), "{out}");
    }

    #[test]
    fn a_call_with_no_method_has_no_command() {
        let mut unchosen = spec("localhost:50051", "greeter.proto");
        if let RequestKind::Grpc(grpc) = &mut unchosen.kind {
            grpc.method.clear();
        }
        assert_eq!(render(&unchosen, None), None);
    }
}
