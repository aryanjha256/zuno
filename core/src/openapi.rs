//! Turning an OpenAPI document into requests.
//!
//! **A hand-written walk over `serde_json::Value`, not a typed model of the specification.**
//! `openapiv3` was the obvious choice and was rejected for a reason its own README states: it
//! covers 3.0.x and "does not cover OpenAPI v3.1 which was an incompatible change". The parts
//! this module reads — `servers`, `paths`, an operation's method, name, parameters and JSON
//! request body — are *identical* across 3.0 and 3.1; the incompatibility is in schema
//! semantics, which is validation Zuno never performs. So modelling the whole specification
//! would buy a large dependency, lock out half the specs in the wild, and still leave `$ref`
//! resolution to be written by hand, which is the only genuinely awkward part.
//!
//! **Permissive by construction, like `curl.rs`.** An operation Zuno cannot make sense of is
//! skipped and *named* in `Import::skipped`, never fatal. A spec is a document written for many
//! tools; refusing to import ninety requests because one of them uses a feature we don't read
//! would break the feature exactly where it is most useful.
//!
//! Deliberately absent: YAML (most published specs use it — this reads JSON only for now, and
//! the YAML crate landscape is a graveyard), remote `$ref`s, and OpenAPI 2.0 / Swagger, which is
//! a different document shape rather than an older version of this one.

use serde_json::Value;

use crate::{Body, Header, Method, QueryParam, RawKind, RequestId, RequestSpec};

/// How deep to follow a schema when inventing an example body.
///
/// A bound rather than a cycle check: `$ref` cycles are legal and common in real schemas (a
/// `User` with a `manager: User`), and a depth cap handles those *and* the merely enormous with
/// one rule. Five levels is past where a generated example stops helping anyone.
const MAX_SCHEMA_DEPTH: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum OpenApiError {
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not an OpenAPI document — no \"openapi\" version field")]
    NotOpenApi,
    #[error(
        "OpenAPI {0} is not supported — Zuno reads 3.x, and 2.0/Swagger is a different format"
    )]
    UnsupportedVersion(String),
    #[error("the document has no paths, so there is nothing to import")]
    NoPaths,
}

/// One operation, ready to be written into a collection.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    /// The operation's first tag, which becomes the folder it is filed under.
    pub folder: Option<String>,
    pub spec: RequestSpec,
}

/// Everything an import yielded, and everything it could not.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Import {
    pub requests: Vec<Imported>,
    /// `info.title`, for naming the folder the import lands in.
    pub title: Option<String>,
    /// What was skipped, in words. Surfaced rather than logged, for `curl.rs`'s reason: an
    /// import that silently drops part of a document is worse than one that says so.
    pub skipped: Vec<String>,
}

/// Read a spec.
pub fn parse(bytes: &[u8]) -> Result<Import, OpenApiError> {
    let root: Value = serde_json::from_slice(bytes)?;

    let version = root
        .get("openapi")
        .and_then(Value::as_str)
        .ok_or(OpenApiError::NotOpenApi)?;
    if !version.starts_with("3.") {
        return Err(OpenApiError::UnsupportedVersion(version.to_string()));
    }

    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or(OpenApiError::NoPaths)?;

    let base = base_url(&root);
    let title = root
        .get("info")
        .and_then(|info| info.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut import = Import {
        title,
        ..Import::default()
    };

    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            import.skipped.push(format!("{path}: not an object"));
            continue;
        };

        // Parameters declared on the path apply to every operation under it, and an operation
        // may add its own. Collected once rather than per method.
        let shared = item.get("parameters").cloned().unwrap_or(Value::Null);

        for (verb, operation) in item {
            let Some(method) = method_for(verb) else {
                // `parameters`, `summary`, `servers`, `$ref` — real keys that are not verbs.
                continue;
            };
            let Some(operation) = operation.as_object() else {
                import.skipped.push(format!("{verb} {path}: not an object"));
                continue;
            };

            import
                .requests
                .push(operation_to_request(&root, &base, path, method, operation, &shared, &mut import.skipped));
        }
    }

    Ok(import)
}

/// The first server's URL, with a trailing slash removed so joining a path is unambiguous.
///
/// Server *variables* are left exactly as written — `https://{region}.api.test` imports with the
/// braces visible. Rewriting them into Zuno's `{{region}}` was considered and dropped: an
/// unresolved `{{…}}` is refused at the send boundary, so a spec using server variables would
/// import a collection where nothing can be sent until every one of them is defined. A literal
/// brace is a URL you can edit; a `{{…}}` is a wall.
fn base_url(root: &Value) -> String {
    root.get("servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(|server| server.get("url"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

fn method_for(verb: &str) -> Option<Method> {
    match verb {
        "get" => Some(Method::Get),
        "post" => Some(Method::Post),
        "put" => Some(Method::Put),
        "patch" => Some(Method::Patch),
        "delete" => Some(Method::Delete),
        "head" => Some(Method::Head),
        "options" => Some(Method::Options),
        // Legal in the specification and sendable by Zuno, so it imports rather than being
        // skipped — `Method::Other` exists for exactly this.
        "trace" => Some(Method::Other("TRACE".to_string())),
        _ => None,
    }
}

fn operation_to_request(
    root: &Value,
    base: &str,
    path: &str,
    method: Method,
    operation: &serde_json::Map<String, Value>,
    shared_parameters: &Value,
    skipped: &mut Vec<String>,
) -> Imported {
    let name = operation
        .get("operationId")
        .and_then(Value::as_str)
        .or_else(|| operation.get("summary").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} {path}", method.as_str()));

    let folder = operation
        .get("tags")
        .and_then(Value::as_array)
        .and_then(|tags| tags.first())
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut query = Vec::new();
    let mut headers = Vec::new();
    for source in [shared_parameters, operation.get("parameters").unwrap_or(&Value::Null)] {
        for parameter in source.as_array().map(Vec::as_slice).unwrap_or_default() {
            let resolved = resolve(root, parameter);
            let Some(parameter) = resolved.as_object() else { continue };
            let Some(name) = parameter.get("name").and_then(Value::as_str) else {
                continue;
            };
            // Only *required* parameters arrive enabled. An optional one is still worth
            // importing — it documents what the endpoint accepts — but sending every optional
            // filter a spec mentions is not what anyone means by "import this API".
            let enabled = parameter
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let value = parameter_example(parameter);

            match parameter.get("in").and_then(Value::as_str) {
                Some("query") => query.push(QueryParam {
                    enabled,
                    name: name.to_string(),
                    value,
                }),
                Some("header") => headers.push(Header {
                    enabled,
                    name: name.to_string(),
                    value,
                }),
                // Path parameters are already visible in the URL as `{id}`, and cookie
                // parameters have no row type. Both are silently fine rather than skipped-with-
                // a-notice, because neither is lost information.
                Some("path") | Some("cookie") | None => {}
                Some(other) => skipped.push(format!("{} {path}: parameter in \"{other}\"", method.as_str())),
            }
        }
    }

    let (body, content_type) = request_body(root, operation, path, &method, skipped);
    if let Some(content_type) = content_type {
        headers.push(Header {
            enabled: true,
            name: "Content-Type".to_string(),
            value: content_type,
        });
    }

    Imported {
        folder,
        spec: RequestSpec {
            // Collection files always store 0; a live handle is assigned when a buffer opens.
            id: RequestId(0),
            name,
            method,
            url: format!("{base}{path}"),
            query,
            headers,
            body,
            settings: Default::default(),
        },
    }
}

/// A parameter's `example`, or its schema's `default`, or nothing.
///
/// Never invented from the type: a query parameter pre-filled with `0` or `""` is a value that
/// will be *sent*, and a wrong value sent silently is worse than an empty one you have to fill.
/// That is the opposite of the rule for a body, below, and the difference is that a body is
/// obviously a draft while a filled-in parameter row looks like a decision someone made.
fn parameter_example(parameter: &serde_json::Map<String, Value>) -> String {
    let value = parameter
        .get("example")
        .or_else(|| parameter.get("schema").and_then(|schema| schema.get("default")));

    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// The JSON request body, if the operation declares one, and the content type to send it as.
fn request_body(
    root: &Value,
    operation: &serde_json::Map<String, Value>,
    path: &str,
    method: &Method,
    skipped: &mut Vec<String>,
) -> (Body, Option<String>) {
    let Some(request_body) = operation.get("requestBody") else {
        return (Body::Empty, None);
    };
    let resolved = resolve(root, request_body);
    let Some(content) = resolved.get("content").and_then(Value::as_object) else {
        return (Body::Empty, None);
    };

    // JSON only. A spec offering `multipart/form-data` or `application/xml` is describing a body
    // Zuno can author but this importer would have to invent, and an invented XML document is
    // less use than an empty editor plus the knowledge that a body is expected.
    let Some((content_type, media)) = content
        .iter()
        .find(|(name, _)| name.starts_with("application/json") || name.ends_with("+json"))
    else {
        let offered: Vec<&str> = content.keys().map(String::as_str).collect();
        skipped.push(format!(
            "{} {path}: body is {} — imported empty",
            method.as_str(),
            offered.join(", ")
        ));
        return (Body::Empty, None);
    };

    // An `example` written by the spec's author beats one this module invents, always.
    let example = media
        .get("example")
        .cloned()
        .or_else(|| {
            media
                .get("examples")
                .and_then(Value::as_object)
                .and_then(|examples| examples.values().next())
                .and_then(|first| first.get("value"))
                .cloned()
        })
        .or_else(|| {
            media
                .get("schema")
                .map(|schema| sample_of(root, schema, 0))
        });

    match example {
        Some(value) => (
            Body::Raw {
                text: serde_json::to_string_pretty(&value).unwrap_or_default(),
                kind: RawKind::Json,
            },
            Some(content_type.clone()),
        ),
        None => (Body::Empty, Some(content_type.clone())),
    }
}

/// A minimal JSON document matching a schema's shape.
///
/// **Shape, not plausible data.** `""` and `0` are placeholders a person will replace; the point
/// is that the keys are right and nested, so the editor opens on something with the correct
/// structure instead of an empty buffer. Inventing realistic values would be guessing about the
/// API's semantics, and a body that looks filled-in is one nobody checks.
fn sample_of(root: &Value, schema: &Value, depth: usize) -> Value {
    if depth >= MAX_SCHEMA_DEPTH {
        return Value::Null;
    }
    let schema = resolve(root, schema);

    // An author-supplied example anywhere in the schema wins over anything derived.
    if let Some(example) = schema.get("example") {
        return example.clone();
    }
    if let Some(default) = schema.get("default") {
        return default.clone();
    }
    // `oneOf`/`anyOf`/`allOf` — take the first branch rather than merging. A merge would be
    // right for `allOf` and wrong for the other two, and one rule that is sometimes incomplete
    // beats three that are sometimes wrong.
    for key in ["allOf", "oneOf", "anyOf"] {
        if let Some(first) = schema.get(key).and_then(Value::as_array).and_then(|all| all.first()) {
            return sample_of(root, first, depth + 1);
        }
    }
    if let Some(first) = schema.get("enum").and_then(Value::as_array).and_then(|all| all.first()) {
        return first.clone();
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("object") | None if schema.get("properties").is_some() => {
            let mut out = serde_json::Map::new();
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, property) in properties {
                    out.insert(name.clone(), sample_of(root, property, depth + 1));
                }
            }
            Value::Object(out)
        }
        Some("object") => Value::Object(serde_json::Map::new()),
        Some("array") => {
            let item = schema
                .get("items")
                .map(|items| sample_of(root, items, depth + 1))
                .unwrap_or(Value::Null);
            Value::Array(vec![item])
        }
        Some("integer") | Some("number") => Value::from(0),
        Some("boolean") => Value::Bool(false),
        Some("string") | _ => Value::String(String::new()),
    }
}

/// Follow a local `$ref`, or hand back what was given.
///
/// **Local only.** A remote `$ref` is an HTTP fetch in the middle of parsing, which would make
/// this function do IO, need a runtime, and fail in ways a document cannot express. The
/// reference is left unresolved instead, which degrades to an empty object rather than an error.
fn resolve(root: &Value, value: &Value) -> Value {
    let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
        return value.clone();
    };
    let Some(pointer) = reference.strip_prefix('#') else {
        return value.clone();
    };

    root.pointer(pointer).cloned().unwrap_or_else(|| value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small but deliberately awkward document: a path-level parameter, a `$ref`d schema, an
    /// author-supplied example, a tag, an optional and a required query parameter, and a body
    /// Zuno cannot author.
    const SPEC: &str = r##"{
      "openapi": "3.0.3",
      "info": { "title": "Billing API" },
      "servers": [{ "url": "https://api.test/v1/" }],
      "components": {
        "schemas": {
          "Invoice": {
            "type": "object",
            "properties": {
              "id": { "type": "string" },
              "amount": { "type": "integer" },
              "paid": { "type": "boolean" },
              "lines": { "type": "array", "items": { "$ref": "#/components/schemas/Line" } }
            }
          },
          "Line": { "type": "object", "properties": { "sku": { "type": "string" } } }
        }
      },
      "paths": {
        "/invoices": {
          "parameters": [
            { "name": "X-Tenant", "in": "header", "required": true, "example": "acme" }
          ],
          "get": {
            "operationId": "listInvoices",
            "tags": ["invoices"],
            "parameters": [
              { "name": "limit", "in": "query", "schema": { "default": 25 } },
              { "name": "status", "in": "query", "required": true, "example": "open" }
            ]
          },
          "post": {
            "summary": "Create an invoice",
            "tags": ["invoices"],
            "requestBody": {
              "content": {
                "application/json": {
                  "schema": { "$ref": "#/components/schemas/Invoice" }
                }
              }
            }
          }
        },
        "/invoices/{id}": {
          "delete": {
            "operationId": "deleteInvoice",
            "parameters": [{ "name": "id", "in": "path", "required": true }]
          }
        },
        "/uploads": {
          "post": {
            "operationId": "upload",
            "requestBody": { "content": { "multipart/form-data": {} } }
          }
        }
      }
    }"##;

    fn find<'a>(import: &'a Import, name: &str) -> &'a Imported {
        import
            .requests
            .iter()
            .find(|request| request.spec.name == name)
            .unwrap_or_else(|| panic!("no {name:?} in {:?}", import.requests.iter().map(|r| &r.spec.name).collect::<Vec<_>>()))
    }

    #[test]
    fn every_operation_becomes_a_request_with_its_url_and_method() {
        let import = parse(SPEC.as_bytes()).expect("parse");
        assert_eq!(import.title.as_deref(), Some("Billing API"));
        assert_eq!(import.requests.len(), 4);

        let list = find(&import, "listInvoices");
        assert_eq!(list.spec.method, Method::Get);
        // The server's trailing slash is trimmed, or every URL would carry a double one.
        assert_eq!(list.spec.url, "https://api.test/v1/invoices");
        assert_eq!(list.folder.as_deref(), Some("invoices"));

        // A path template keeps its braces. Rewriting `{id}` into Zuno's `{{id}}` would make
        // every imported request unsendable until the variable was defined.
        assert_eq!(
            find(&import, "deleteInvoice").spec.url,
            "https://api.test/v1/invoices/{id}"
        );
    }

    #[test]
    fn a_name_falls_back_from_operation_id_to_summary_to_the_route() {
        let import = parse(SPEC.as_bytes()).expect("parse");
        // `post /invoices` has no operationId, so its summary names it.
        find(&import, "Create an invoice");
        // And an operation with neither would be named for its route — asserted separately,
        // since every operation in SPEC has one of the two.
        let bare = parse(
            br#"{"openapi":"3.0.0","paths":{"/ping":{"get":{}}}}"#,
        )
        .expect("parse");
        assert_eq!(bare.requests[0].spec.name, "GET /ping");
    }

    #[test]
    fn only_required_parameters_arrive_enabled() {
        // An optional parameter is worth importing — it documents what the endpoint takes — but
        // sending every filter a spec mentions is not what "import this API" means.
        let import = parse(SPEC.as_bytes()).expect("parse");
        let list = find(&import, "listInvoices");

        let limit = list.spec.query.iter().find(|q| q.name == "limit").expect("limit");
        assert!(!limit.enabled, "an optional parameter must arrive muted");
        assert_eq!(limit.value, "25", "and carry its schema default");

        let status = list.spec.query.iter().find(|q| q.name == "status").expect("status");
        assert!(status.enabled);
        assert_eq!(status.value, "open");
    }

    #[test]
    fn a_path_level_parameter_reaches_every_operation_under_it() {
        // Declared once on `/invoices` and inherited by both `get` and `post`. Read only from
        // the operation and the header is silently missing from every request under that path.
        let import = parse(SPEC.as_bytes()).expect("parse");

        for name in ["listInvoices", "Create an invoice"] {
            let request = find(&import, name);
            let tenant = request
                .spec
                .headers
                .iter()
                .find(|h| h.name == "X-Tenant")
                .unwrap_or_else(|| panic!("{name} lost the path-level header"));
            assert!(tenant.enabled);
            assert_eq!(tenant.value, "acme");
        }
    }

    #[test]
    fn a_referenced_schema_becomes_a_body_with_the_right_shape() {
        let import = parse(SPEC.as_bytes()).expect("parse");
        let create = find(&import, "Create an invoice");

        let Body::Raw { text, kind } = &create.spec.body else {
            panic!("expected a raw body, got {:?}", create.spec.body);
        };
        assert_eq!(*kind, RawKind::Json);

        let value: Value = serde_json::from_str(text).expect("the generated body must be JSON");
        // Shape, not plausible data — and the nested `$ref` inside `items` resolved too.
        assert_eq!(
            value,
            serde_json::json!({
                "id": "",
                "amount": 0,
                "paid": false,
                "lines": [{ "sku": "" }]
            })
        );

        assert!(
            create.spec.headers.iter().any(|h| h.name == "Content-Type"
                && h.value == "application/json"
                && h.enabled),
            "a generated body has to declare what it is"
        );
    }

    #[test]
    fn a_body_zuno_cannot_invent_is_skipped_by_name_rather_than_guessed() {
        // The `curl.rs` rule: never silently drop part of a document.
        let import = parse(SPEC.as_bytes()).expect("parse");
        assert_eq!(find(&import, "upload").spec.body, Body::Empty);
        assert!(
            import.skipped.iter().any(|note| note.contains("multipart/form-data")),
            "the skipped body must be reported: {:?}",
            import.skipped
        );
    }

    #[test]
    fn an_authors_example_beats_a_generated_one() {
        let spec = br##"{
          "openapi": "3.1.0",
          "paths": { "/x": { "post": { "operationId": "x", "requestBody": { "content": {
            "application/json": {
              "schema": { "type": "object", "properties": { "a": { "type": "string" } } },
              "example": { "a": "hello", "extra": true }
            }
          } } } } }
        }"##;

        let import = parse(spec).expect("parse");
        let Body::Raw { text, .. } = &import.requests[0].spec.body else {
            panic!("expected a raw body");
        };
        let value: Value = serde_json::from_str(text).expect("json");
        assert_eq!(value, serde_json::json!({ "a": "hello", "extra": true }));
    }

    #[test]
    fn a_3_1_document_imports_the_same_as_a_3_0_one() {
        // The whole argument for not taking `openapiv3`, which covers 3.0 only. The parts this
        // module reads did not change between the two versions.
        let import = parse(
            br##"{"openapi":"3.1.0","servers":[{"url":"https://a.test"}],
                  "paths":{"/ping":{"get":{"operationId":"ping"}}}}"##,
        )
        .expect("a 3.1 document must import");
        assert_eq!(import.requests[0].spec.url, "https://a.test/ping");
    }

    #[test]
    fn a_reference_cycle_terminates_instead_of_recursing_forever() {
        // Legal and common: a User whose manager is a User. A depth cap handles this and the
        // merely enormous with one rule, where a visited-set would only handle the first.
        let spec = br##"{
          "openapi": "3.0.0",
          "components": { "schemas": { "User": { "type": "object", "properties": {
            "name": { "type": "string" },
            "manager": { "$ref": "#/components/schemas/User" }
          } } } },
          "paths": { "/users": { "post": { "operationId": "createUser", "requestBody": {
            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/User" } } }
          } } } }
        }"##;

        let import = parse(spec).expect("parse");
        let Body::Raw { text, .. } = &import.requests[0].spec.body else {
            panic!("expected a raw body");
        };
        assert!(text.contains("manager"), "the cycle must still produce structure");
        assert!(text.len() < 4096, "and must not have run away: {} bytes", text.len());
    }

    #[test]
    fn swagger_2_and_plain_json_are_refused_with_different_messages() {
        // A 2.0 document is a different shape, not an older version of this one, and saying so
        // beats importing zero requests from a file that plainly describes an API.
        let swagger = parse(br#"{"swagger":"2.0","paths":{}}"#);
        assert!(matches!(swagger, Err(OpenApiError::NotOpenApi)));

        let two_point_oh = parse(br#"{"openapi":"2.0","paths":{}}"#);
        assert!(matches!(two_point_oh, Err(OpenApiError::UnsupportedVersion(_))));

        assert!(matches!(parse(b"not json at all"), Err(OpenApiError::Json(_))));
        assert!(matches!(
            parse(br#"{"openapi":"3.0.0"}"#),
            Err(OpenApiError::NoPaths)
        ));
    }
}
