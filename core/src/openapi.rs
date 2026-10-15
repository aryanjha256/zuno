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
//! YAML arrives here already converted to the same `Value` — see `import::read`.
//!
//! Deliberately absent: remote `$ref`s, and OpenAPI 2.0 / Swagger, which is a different document
//! shape rather than an older version of this one.

use serde_json::Value;

use crate::import::{Import, Imported, Variable};
use crate::{
    Auth, Body, FormField, Header, HttpRequest, Method, MultipartField, MultipartValue, QueryParam,
    RawKind, RequestId, RequestKind, RequestSpec,
};

/// How deep to follow a schema when inventing an example body.
///
/// A bound rather than a cycle check: `$ref` cycles are legal and common in real schemas (a
/// `User` with a `manager: User`), and a depth cap handles those *and* the merely enormous with
/// one rule. Five levels is past where a generated example stops helping anyone.
const MAX_SCHEMA_DEPTH: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum OpenApiError {
    #[error("not an OpenAPI document — no \"openapi\" version field")]
    NotOpenApi,
    #[error(
        "OpenAPI {0} is not supported — Zuno reads 3.x, and 2.0/Swagger is a different format"
    )]
    UnsupportedVersion(String),
    #[error("the document has no paths, so there is nothing to import")]
    NoPaths,
}

/// Read a spec.
///
/// Takes the parsed document rather than bytes: `import::parse` has already read the JSON in
/// order to decide which parser to call, and a megabyte spec should not be parsed twice.
pub fn parse(root: &Value) -> Result<Import, OpenApiError> {
    // A number as well as a string: YAML reads an unquoted `openapi: 3.0` as a float.
    let version = match root.get("openapi") {
        Some(Value::String(version)) => version.clone(),
        Some(Value::Number(version)) => version.to_string(),
        _ => return Err(OpenApiError::NotOpenApi),
    };
    if !version.starts_with("3.") {
        return Err(OpenApiError::UnsupportedVersion(version.to_string()));
    }

    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or(OpenApiError::NoPaths)?;

    let base = base_url(root);
    let title = root
        .get("info")
        .and_then(|info| info.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut import = Import {
        title,
        ..Import::default()
    };
    let mut auth = AuthPlan::new(root);
    // Operations a split spec keeps in other files. Counted, not listed: DigitalOcean's has 845,
    // and they all share one cause.
    let mut external: Vec<String> = Vec::new();
    let mut external_paths: Vec<String> = Vec::new();

    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            import.skipped.push(format!("{path}: not an object"));
            continue;
        };
        if let Some(reference) = external_ref(item) {
            external_paths.push(format!("{path} ({reference})"));
            continue;
        }

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
            if let Some(reference) = external_ref(operation) {
                external.push(reference.to_string());
            }

            let mut imported =
                operation_to_request(root, &base, path, method, operation, &shared, &mut import.skipped);
            auth.apply(operation, &mut imported.spec);
            import.requests.push(imported);
        }
    }

    if let Some(example) = external.first() {
        import.skipped.push(format!(
            "{} operations are in other files the spec points to ({example}, …) — Zuno reads one \
             file, so they have their method and URL only. Import the bundled spec for the rest",
            external.len()
        ));
    }
    if let Some(example) = external_paths.first() {
        import.skipped.push(format!(
            "{} paths are defined entirely in other files, e.g. {example} — not imported",
            external_paths.len()
        ));
    }
    auth.finish(&mut import);

    Ok(import)
}

/// A `$ref` to another file, standing in for a path item or an operation.
///
/// **Reported, never followed.** The importer is handed a document, not a location, so a sibling
/// file is as unreachable as a remote one — and a split spec quietly importing as bare URLs, with
/// "0 skipped", was the failure found on DigitalOcean's.
fn external_ref(object: &serde_json::Map<String, Value>) -> Option<&str> {
    object
        .get("$ref")
        .and_then(Value::as_str)
        .filter(|reference| !reference.starts_with('#'))
}

/// How a spec's `securitySchemes` land on its requests.
///
/// **Stamped per request, with the secret left as a variable.** Zuno has no folder-level auth, so
/// an API whose every operation needs a token imported as hundreds of requests to fix by hand —
/// every spec tried declared its auth, and none of it arrived. Each request gets the scheme in
/// the field Zuno already sends, and the variable it names goes into the environment the import
/// creates and selects, so a key is pasted once.
struct AuthPlan<'a> {
    root: &'a Value,
    schemes: Option<&'a serde_json::Map<String, Value>>,
    /// The spec-wide requirement, which an operation's own `security` replaces.
    default: Option<&'a Value>,
    /// Only one `apiKey` scheme can be called `{{apiKey}}`.
    api_keys: usize,
    variables: Vec<String>,
    unsupported: std::collections::BTreeSet<String>,
}

impl<'a> AuthPlan<'a> {
    fn new(root: &'a Value) -> Self {
        let schemes = root
            .get("components")
            .and_then(|components| components.get("securitySchemes"))
            .and_then(Value::as_object);
        let api_keys = schemes
            .map(|schemes| {
                schemes
                    .values()
                    .filter(|scheme| {
                        resolve(root, scheme).get("type").and_then(Value::as_str) == Some("apiKey")
                    })
                    .count()
            })
            .unwrap_or(0);
        Self {
            root,
            schemes,
            default: root.get("security"),
            api_keys,
            variables: Vec::new(),
            unsupported: Default::default(),
        }
    }

    /// The first requirement that can be met in full. `{}` is an *optional* requirement — the
    /// operation also works anonymously — so it is passed over rather than read as "no auth",
    /// and `security: []` on an operation is an explicitly public endpoint, which gets nothing.
    fn apply(&mut self, operation: &serde_json::Map<String, Value>, spec: &mut RequestSpec) {
        let Some(requirements) = operation
            .get("security")
            .or(self.default)
            .and_then(Value::as_array)
        else {
            return;
        };

        for requirement in requirements {
            let Some(names) = requirement.as_object().filter(|names| !names.is_empty()) else {
                continue;
            };
            let effects: Option<Vec<SchemeEffect>> =
                names.keys().map(|name| self.effect(name)).collect();
            let Some(effects) = effects else { continue };

            for effect in effects {
                match effect {
                    SchemeEffect::Auth(auth) => spec.auth = auth,
                    SchemeEffect::Header(header) => {
                        if !spec.headers.iter().any(|h| h.name.eq_ignore_ascii_case(&header.name)) {
                            spec.headers.push(header);
                        }
                    }
                    SchemeEffect::Query(param) => {
                        if let Some(http) = spec.http_mut()
                            && !http.query.iter().any(|q| q.name == param.name)
                        {
                            http.query.push(param);
                        }
                    }
                }
            }
            return;
        }
    }

    fn effect(&mut self, name: &str) -> Option<SchemeEffect> {
        let scheme = resolve(self.root, self.schemes?.get(name)?);
        let kind = scheme.get("type").and_then(Value::as_str).unwrap_or_default();

        let effect = match kind {
            "http" => match scheme
                .get("scheme")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("bearer") => SchemeEffect::Auth(Auth::Bearer {
                    token: self.variable("bearerToken"),
                }),
                Some("basic") => SchemeEffect::Auth(Auth::Basic {
                    username: self.variable("username"),
                    password: self.variable("password"),
                }),
                _ => return self.unsupported(name, kind),
            },
            // The token comes from a flow Zuno does not run; what goes on the wire is a bearer
            // token, so the request is ready the moment one is pasted.
            "oauth2" | "openIdConnect" => SchemeEffect::Auth(Auth::Bearer {
                token: self.variable("bearerToken"),
            }),
            "apiKey" => {
                let key = scheme.get("name").and_then(Value::as_str)?.to_string();
                let variable = if self.api_keys > 1 {
                    self.variable(&variable_name(name))
                } else {
                    self.variable("apiKey")
                };
                match scheme.get("in").and_then(Value::as_str) {
                    Some("header") => SchemeEffect::Header(Header {
                        enabled: true,
                        name: key,
                        value: variable,
                    }),
                    Some("query") => SchemeEffect::Query(QueryParam {
                        enabled: true,
                        name: key,
                        value: variable,
                    }),
                    Some("cookie") => SchemeEffect::Header(Header {
                        enabled: true,
                        name: "Cookie".to_string(),
                        value: format!("{key}={variable}"),
                    }),
                    _ => return self.unsupported(name, kind),
                }
            }
            _ => return self.unsupported(name, kind),
        };
        Some(effect)
    }

    fn variable(&mut self, name: &str) -> String {
        if !self.variables.iter().any(|known| known == name) {
            self.variables.push(name.to_string());
        }
        format!("{{{{{name}}}}}")
    }

    fn unsupported(&mut self, name: &str, kind: &str) -> Option<SchemeEffect> {
        self.unsupported.insert(format!("{name} ({kind})"));
        None
    }

    /// Empty and secret: the value is the user's, and it belongs in the gitignored half.
    fn finish(self, import: &mut Import) {
        import
            .variables
            .extend(self.variables.into_iter().map(|name| Variable {
                name,
                value: String::new(),
                secret: true,
            }));
        for scheme in self.unsupported {
            import
                .skipped
                .push(format!("auth scheme {scheme} — not imported, add it by hand"));
        }
    }
}

enum SchemeEffect {
    Auth(Auth),
    Header(Header),
    Query(QueryParam),
}

/// A scheme's name as a `{{variable}}`: the characters a variable name may hold, the rest `_`.
fn variable_name(scheme: &str) -> String {
    scheme
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
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
        // A tag is one level deep by construction; Postman is what needed the `Vec`.
        folders: folder.into_iter().collect(),
        spec: RequestSpec {
            // Collection files always store 0; a live handle is assigned when a buffer opens.
            id: RequestId(0),
            name,
            url: format!("{base}{path}"),
            headers,
            settings: Default::default(),
            kind: RequestKind::Http(HttpRequest {
                method,
                query,
                body,
            }),
            ..RequestSpec::default()
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

/// The request body, if the operation declares one, and the content type to send it as — `None`
/// for a form or multipart body, whose content type the engine sets from the body itself.
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

    // JSON first; then the two form encodings, whose fields Zuno edits as rows; then `*/*` with a
    // schema, which is how Kubernetes declares what is in practice a JSON body. XML and the rest
    // stay empty: an invented XML document is less use than an empty editor plus the knowledge
    // that a body is expected.
    let json = content
        .iter()
        .find(|(name, _)| name.starts_with("application/json") || name.ends_with("+json"))
        .map(|(name, media)| (name.clone(), media))
        .or_else(|| {
            content
                .get("*/*")
                .filter(|media| media.get("schema").is_some())
                .map(|media| ("application/json".to_string(), media))
        });
    let Some((content_type, media)) = json else {
        let fields = |prefix: &str| {
            content
                .iter()
                .find(|(name, _)| name.starts_with(prefix))
                .and_then(|(_, media)| form_fields(root, media))
        };
        if let Some(fields) = fields("application/x-www-form-urlencoded") {
            if fields.is_empty() {
                return (Body::Empty, None);
            }
            let fields = fields
                .into_iter()
                .map(|field| FormField {
                    enabled: field.required,
                    name: field.name,
                    value: field.value,
                })
                .collect();
            return (Body::Form(fields), None);
        }
        if let Some(fields) = fields("multipart/form-data") {
            if fields.is_empty() {
                return (Body::Empty, None);
            }
            let fields = fields
                .into_iter()
                .map(|field| MultipartField {
                    enabled: field.required,
                    name: field.name,
                    // An empty path rather than a guess: the file is the user's to choose.
                    value: if field.binary {
                        MultipartValue::File(Default::default())
                    } else {
                        MultipartValue::Text(field.value)
                    },
                })
                .collect();
            return (Body::Multipart(fields), None);
        }

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

struct SchemaField {
    name: String,
    value: String,
    required: bool,
    binary: bool,
}

/// A form body's fields, from its schema's properties — required first, then the rest.
///
/// **Values follow the parameter rule, not the JSON-body one:** an author's example or default,
/// otherwise empty. A form is rows, and a row pre-filled with `0` reads as a decision someone made
/// — see `parameter_example`. Optional fields arrive switched off, for the same reason optional
/// query parameters do; Stripe's create-a-charge has dozens.
///
/// `None` when there are no properties to read, so a schemaless form is still reported as
/// skipped rather than imported as an empty body that looks deliberate. An *empty* `properties`
/// is the opposite case — a declared "no fields", which Stripe writes on all 298 of its
/// body-less operations — and is `Some(vec![])`, imported empty without a note.
fn form_fields(root: &Value, media: &Value) -> Option<Vec<SchemaField>> {
    let schema = resolve(root, media.get("schema")?);
    let example = media.get("example").and_then(Value::as_object);

    // `allOf` composes the fields, so its branches merge here — the opposite of `sample_of`,
    // which only picks one because it cannot tell `allOf` from `oneOf` safely in a JSON value.
    let mut branches = vec![schema.clone()];
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        branches.extend(all.iter().map(|branch| resolve(root, branch)));
    }

    let mut fields: Vec<SchemaField> = Vec::new();
    let mut declared = false;
    for branch in &branches {
        let required: Vec<&str> = branch
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(properties) = branch.get("properties").and_then(Value::as_object) else {
            continue;
        };
        declared = true;
        for (name, property) in properties {
            if fields.iter().any(|field| &field.name == name) {
                continue;
            }
            let property = resolve(root, property);
            let value = example
                .and_then(|example| example.get(name))
                .or_else(|| property.get("example"))
                .or_else(|| property.get("default"));
            fields.push(SchemaField {
                name: name.clone(),
                value: match value {
                    Some(Value::String(text)) => text.clone(),
                    Some(scalar @ (Value::Number(_) | Value::Bool(_))) => scalar.to_string(),
                    _ => String::new(),
                },
                required: required.contains(&name.as_str()),
                binary: property.get("format").and_then(Value::as_str) == Some("binary"),
            });
        }
    }

    if !declared {
        return None;
    }
    fields.sort_by_key(|field| !field.required);
    Some(fields)
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

    /// `parse` takes an already-read document now, so every test reads its own.
    fn read(document: &str) -> Result<Import, OpenApiError> {
        parse(&serde_json::from_str(document).expect("test document is valid JSON"))
    }

    fn find<'a>(import: &'a Import, name: &str) -> &'a Imported {
        import
            .requests
            .iter()
            .find(|request| request.spec.name == name)
            .unwrap_or_else(|| panic!("no {name:?} in {:?}", import.requests.iter().map(|r| &r.spec.name).collect::<Vec<_>>()))
    }

    #[test]
    fn every_operation_becomes_a_request_with_its_url_and_method() {
        let import = read(SPEC).expect("parse");
        assert_eq!(import.title.as_deref(), Some("Billing API"));
        assert_eq!(import.requests.len(), 4);

        let list = find(&import, "listInvoices");
        assert_eq!(list.spec.http().unwrap().method, Method::Get);
        // The server's trailing slash is trimmed, or every URL would carry a double one.
        assert_eq!(list.spec.url, "https://api.test/v1/invoices");
        assert_eq!(list.folders, vec!["invoices".to_string()]);

        // A path template keeps its braces. Rewriting `{id}` into Zuno's `{{id}}` would make
        // every imported request unsendable until the variable was defined.
        assert_eq!(
            find(&import, "deleteInvoice").spec.url,
            "https://api.test/v1/invoices/{id}"
        );
    }

    #[test]
    fn a_name_falls_back_from_operation_id_to_summary_to_the_route() {
        let import = read(SPEC).expect("parse");
        // `post /invoices` has no operationId, so its summary names it.
        find(&import, "Create an invoice");
        // And an operation with neither would be named for its route — asserted separately,
        // since every operation in SPEC has one of the two.
        let bare = read(
            r#"{"openapi":"3.0.0","paths":{"/ping":{"get":{}}}}"#,
        )
        .expect("parse");
        assert_eq!(bare.requests[0].spec.name, "GET /ping");
    }

    #[test]
    fn only_required_parameters_arrive_enabled() {
        // An optional parameter is worth importing — it documents what the endpoint takes — but
        // sending every filter a spec mentions is not what "import this API" means.
        let import = read(SPEC).expect("parse");
        let list = find(&import, "listInvoices");

        let limit = list.spec.http().unwrap().query.iter().find(|q| q.name == "limit").expect("limit");
        assert!(!limit.enabled, "an optional parameter must arrive muted");
        assert_eq!(limit.value, "25", "and carry its schema default");

        let status = list.spec.http().unwrap().query.iter().find(|q| q.name == "status").expect("status");
        assert!(status.enabled);
        assert_eq!(status.value, "open");
    }

    #[test]
    fn a_path_level_parameter_reaches_every_operation_under_it() {
        // Declared once on `/invoices` and inherited by both `get` and `post`. Read only from
        // the operation and the header is silently missing from every request under that path.
        let import = read(SPEC).expect("parse");

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
        let import = read(SPEC).expect("parse");
        let create = find(&import, "Create an invoice");

        let Body::Raw { text, kind } = &create.spec.http().unwrap().body else {
            panic!("expected a raw body, got {:?}", create.spec.http().unwrap().body);
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
        let import = read(SPEC).expect("parse");
        assert_eq!(find(&import, "upload").spec.http().unwrap().body, Body::Empty);
        assert!(
            import.skipped.iter().any(|note| note.contains("multipart/form-data")),
            "the skipped body must be reported: {:?}",
            import.skipped
        );
    }

    /// **The spec's auth arrives on every request, as a variable to fill once.** Every real spec
    /// tried declared its auth and none of it was imported, so a first send was always a 401.
    #[test]
    fn security_schemes_reach_each_request_with_their_secret_as_a_variable() {
        let import = read(
            r##"{
              "openapi": "3.0.3",
              "servers": [{ "url": "https://api.test" }],
              "security": [{}, { "bearer": [] }],
              "components": { "securitySchemes": {
                "bearer": { "type": "http", "scheme": "Bearer" },
                "key": { "type": "apiKey", "in": "query", "name": "api_key" },
                "tls": { "type": "mutualTLS" }
              } },
              "paths": {
                "/me": { "get": { "operationId": "me" } },
                "/health": { "get": { "operationId": "health", "security": [] } },
                "/search": { "get": { "operationId": "search", "security": [{ "tls": [] }, { "key": [] }] } }
              }
            }"##,
        )
        .expect("parse");

        // `{}` means "also works anonymously", so the real requirement after it is used.
        assert_eq!(
            find(&import, "me").spec.auth,
            Auth::Bearer { token: "{{bearerToken}}".into() }
        );
        // `security: []` is a public endpoint, deliberately.
        assert_eq!(find(&import, "health").spec.auth, Auth::None);
        // A requirement Zuno cannot meet falls through to the next, and is named.
        let search = find(&import, "search").spec.http().unwrap();
        assert!(search.query.iter().any(|q| q.name == "api_key" && q.value == "{{apiKey}}" && q.enabled));
        assert!(import.skipped.iter().any(|note| note.contains("tls (mutualTLS)")), "{:?}", import.skipped);

        let names: Vec<(&str, bool)> = import
            .variables
            .iter()
            .map(|variable| (variable.name.as_str(), variable.secret))
            .collect();
        assert_eq!(names, vec![("bearerToken", true), ("apiKey", true)]);
    }

    /// **Form and multipart bodies arrive as rows.** Stripe declares 611 of its 612 operations
    /// form-encoded, and every one of them imported with an empty body.
    #[test]
    fn form_bodies_become_fields_with_only_the_required_ones_on() {
        let import = read(
            r##"{
              "openapi": "3.0.3",
              "paths": {
                "/charges": { "post": { "operationId": "charge", "requestBody": { "content": {
                  "application/x-www-form-urlencoded": { "schema": {
                    "type": "object", "required": ["currency", "amount"],
                    "properties": {
                      "amount": { "type": "integer" },
                      "currency": { "type": "string", "example": "usd" },
                      "description": { "type": "string" }
                    } } } } } } },
                "/files": { "post": { "operationId": "upload", "requestBody": { "content": {
                  "multipart/form-data": { "schema": { "type": "object", "required": ["file"],
                    "properties": { "file": { "type": "string", "format": "binary" }, "purpose": { "type": "string" } } } } } } } },
                "/pods": { "post": { "operationId": "pod", "requestBody": { "content": {
                  "*/*": { "schema": { "type": "object", "properties": { "kind": { "type": "string" } } } } } } } },
                "/split": { "get": { "$ref": "paths/split.yaml" } }
              }
            }"##,
        )
        .expect("parse");

        let Body::Form(fields) = &find(&import, "charge").spec.http().unwrap().body else {
            panic!("a form body must import as a form");
        };
        let rows: Vec<(&str, &str, bool)> = fields
            .iter()
            .map(|field| (field.name.as_str(), field.value.as_str(), field.enabled))
            .collect();
        assert_eq!(
            rows,
            vec![("amount", "", true), ("currency", "usd", true), ("description", "", false)]
        );

        let Body::Multipart(parts) = &find(&import, "upload").spec.http().unwrap().body else {
            panic!("a multipart body must import as multipart");
        };
        assert_eq!(parts[0].value, MultipartValue::File(Default::default()));
        assert!(parts[0].enabled && !parts[1].enabled);

        assert!(matches!(find(&import, "pod").spec.http().unwrap().body, Body::Raw { kind: RawKind::Json, .. }));

        assert!(
            import.skipped.iter().any(|note| note.contains("paths/split.yaml")),
            "a split spec must say so: {:?}",
            import.skipped
        );
    }

    #[test]
    fn an_authors_example_beats_a_generated_one() {
        let spec = r##"{
          "openapi": "3.1.0",
          "paths": { "/x": { "post": { "operationId": "x", "requestBody": { "content": {
            "application/json": {
              "schema": { "type": "object", "properties": { "a": { "type": "string" } } },
              "example": { "a": "hello", "extra": true }
            }
          } } } } }
        }"##;

        let import = read(spec).expect("parse");
        let Body::Raw { text, .. } = &import.requests[0].spec.http().unwrap().body else {
            panic!("expected a raw body");
        };
        let value: Value = serde_json::from_str(text).expect("json");
        assert_eq!(value, serde_json::json!({ "a": "hello", "extra": true }));
    }

    #[test]
    fn a_3_1_document_imports_the_same_as_a_3_0_one() {
        // The whole argument for not taking `openapiv3`, which covers 3.0 only. The parts this
        // module reads did not change between the two versions.
        let import = read(
            r##"{"openapi":"3.1.0","servers":[{"url":"https://a.test"}],
                  "paths":{"/ping":{"get":{"operationId":"ping"}}}}"##,
        )
        .expect("a 3.1 document must import");
        assert_eq!(import.requests[0].spec.url, "https://a.test/ping");
    }

    #[test]
    fn a_reference_cycle_terminates_instead_of_recursing_forever() {
        // Legal and common: a User whose manager is a User. A depth cap handles this and the
        // merely enormous with one rule, where a visited-set would only handle the first.
        let spec = r##"{
          "openapi": "3.0.0",
          "components": { "schemas": { "User": { "type": "object", "properties": {
            "name": { "type": "string" },
            "manager": { "$ref": "#/components/schemas/User" }
          } } } },
          "paths": { "/users": { "post": { "operationId": "createUser", "requestBody": {
            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/User" } } }
          } } } }
        }"##;

        let import = read(spec).expect("parse");
        let Body::Raw { text, .. } = &import.requests[0].spec.http().unwrap().body else {
            panic!("expected a raw body");
        };
        assert!(text.contains("manager"), "the cycle must still produce structure");
        assert!(text.len() < 4096, "and must not have run away: {} bytes", text.len());
    }

    #[test]
    fn a_document_this_parser_cannot_read_is_refused_by_reason() {
        // Which format a file *is* is `import::parse`'s question, and the messages for the
        // shapes we recognise but cannot read are asserted there. These are this parser's own
        // refusals, reachable by calling it directly.
        assert!(matches!(
            read(r#"{"swagger":"2.0","paths":{}}"#),
            Err(OpenApiError::NotOpenApi)
        ));
        assert!(matches!(
            read(r#"{"openapi":"2.0","paths":{}}"#),
            Err(OpenApiError::UnsupportedVersion(_))
        ));
        assert!(matches!(
            read(r#"{"openapi":"3.0.0"}"#),
            Err(OpenApiError::NoPaths)
        ));
    }
}
