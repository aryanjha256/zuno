//! Asking a GraphQL server for its schema, and keeping the answer as SDL.
//!
//! **Kept as SDL, not as the introspection JSON.** SDL is what a person reads, diffs in a pull
//! request and hand-edits when a server has introspection off; it is what `apollo-compiler` parses;
//! and it is a fraction of the JSON's size. The reply is converted once, here, and never again.
//!
//! **The classic introspection query, not the newest.** Fields added to the spec later —
//! `isRepeatable`, `specifiedByURL`, deprecated arguments — are errors on servers that predate
//! them, and a schema fetch that fails on a real server because it asked for a detail nobody
//! needs yet is the wrong trade.

use std::fmt::Write as _;

use serde::Deserialize;

/// The query sent to fetch a schema. `operationName` is `IntrospectionQuery`.
pub const QUERY: &str = r#"query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types { ...FullType }
    directives { name description locations args { ...InputValue } }
  }
}
fragment FullType on __Type {
  kind name description
  fields(includeDeprecated: true) {
    name description
    args { ...InputValue }
    type { ...TypeRef }
    isDeprecated deprecationReason
  }
  inputFields { ...InputValue }
  interfaces { ...TypeRef }
  enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason }
  possibleTypes { ...TypeRef }
}
fragment InputValue on __InputValue { name description type { ...TypeRef } defaultValue }
fragment TypeRef on __Type {
  kind name
  ofType { kind name ofType { kind name ofType { kind name ofType { kind name
    ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } } } } } }
}"#;

/// The operation name `QUERY` defines.
pub const OPERATION: &str = "IntrospectionQuery";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntrospectionError {
    #[error("the server's reply is not a GraphQL response: {0}")]
    NotGraphQl(String),
    /// The server answered with errors and no schema — most often because introspection is
    /// turned off, which production servers commonly do. The server's own words are kept.
    #[error("the server refused to describe its schema: {0}")]
    Refused(String),
    /// What came back could not be written as a schema this app can read. A bug here rather than
    /// the server's, so the parser's message is kept for the report.
    #[error("the schema could not be read back: {0}")]
    Unreadable(String),
}

/// Convert an introspection reply's body to SDL, checked by parsing it back.
///
/// Returns the SDL and how many types it defines, for the status line.
pub fn to_sdl(body: &[u8]) -> Result<(String, usize), IntrospectionError> {
    let reply: Reply = serde_json::from_slice(body)
        .map_err(|error| IntrospectionError::NotGraphQl(error.to_string()))?;
    let Some(schema) = reply.data.and_then(|data| data.schema) else {
        let messages: Vec<String> = reply
            .errors
            .unwrap_or_default()
            .into_iter()
            .map(|error| error.message)
            .collect();
        return Err(IntrospectionError::Refused(if messages.is_empty() {
            "it returned no schema and no reason".to_string()
        } else {
            messages.join("; ")
        }));
    };

    let sdl = print(&schema);
    // **Parsed, not validated.** A syntax or build error means this printer is wrong, and the
    // file must not be written. A *validation* error is the server's own schema breaking a rule —
    // an object type with no fields is common — and it is still the schema the server runs.
    let parsed = match apollo_compiler::Schema::parse(sdl.clone(), "schema.graphql") {
        Ok(parsed) => parsed,
        Err(with_errors) => {
            return Err(IntrospectionError::Unreadable(with_errors.errors.to_string()));
        }
    };
    let types = parsed
        .types
        .keys()
        .filter(|name| !name.starts_with("__") && !BUILT_IN_SCALARS.contains(&name.as_str()))
        .count();
    Ok((sdl, types))
}

const BUILT_IN_SCALARS: [&str; 5] = ["String", "Int", "Float", "Boolean", "ID"];

/// Directives every server has, and which SDL must not redefine.
const BUILT_IN_DIRECTIVES: [&str; 5] = ["skip", "include", "deprecated", "specifiedBy", "oneOf"];

#[derive(Deserialize)]
struct Reply {
    data: Option<Data>,
    errors: Option<Vec<ReplyError>>,
}

#[derive(Deserialize)]
struct ReplyError {
    message: String,
}

#[derive(Deserialize)]
struct Data {
    #[serde(rename = "__schema")]
    schema: Option<IntroSchema>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntroSchema {
    query_type: Option<Named>,
    mutation_type: Option<Named>,
    subscription_type: Option<Named>,
    types: Vec<FullType>,
    #[serde(default)]
    directives: Vec<Directive>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FullType {
    kind: String,
    name: String,
    description: Option<String>,
    fields: Option<Vec<Field>>,
    input_fields: Option<Vec<InputValue>>,
    interfaces: Option<Vec<TypeRef>>,
    enum_values: Option<Vec<EnumValue>>,
    possible_types: Option<Vec<TypeRef>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Field {
    name: String,
    description: Option<String>,
    #[serde(default)]
    args: Vec<InputValue>,
    #[serde(rename = "type")]
    ty: TypeRef,
    #[serde(default)]
    is_deprecated: bool,
    deprecation_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputValue {
    name: String,
    description: Option<String>,
    #[serde(rename = "type")]
    ty: TypeRef,
    default_value: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnumValue {
    name: String,
    description: Option<String>,
    #[serde(default)]
    is_deprecated: bool,
    deprecation_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TypeRef {
    kind: String,
    name: Option<String>,
    of_type: Option<Box<TypeRef>>,
}

#[derive(Deserialize)]
struct Directive {
    name: String,
    description: Option<String>,
    #[serde(default)]
    locations: Vec<String>,
    #[serde(default)]
    args: Vec<InputValue>,
}

fn print(schema: &IntroSchema) -> String {
    let mut out = String::new();

    // Always written out, even with the conventional names: it costs three lines and means a
    // server whose root is called `RootQuery` needs no special case.
    let roots = [
        ("query", &schema.query_type),
        ("mutation", &schema.mutation_type),
        ("subscription", &schema.subscription_type),
    ];
    out.push_str("schema {\n");
    for (operation, root) in roots {
        if let Some(root) = root {
            let _ = writeln!(out, "  {operation}: {}", root.name);
        }
    }
    out.push_str("}\n");

    for directive in &schema.directives {
        if BUILT_IN_DIRECTIVES.contains(&directive.name.as_str()) {
            continue;
        }
        out.push('\n');
        description(&mut out, directive.description.as_deref(), "");
        let _ = write!(out, "directive @{}", directive.name);
        arguments(&mut out, &directive.args);
        let _ = writeln!(out, " on {}", directive.locations.join(" | "));
    }

    for ty in &schema.types {
        if ty.name.starts_with("__") || BUILT_IN_SCALARS.contains(&ty.name.as_str()) {
            continue;
        }
        out.push('\n');
        description(&mut out, ty.description.as_deref(), "");
        match ty.kind.as_str() {
            "SCALAR" => {
                let _ = writeln!(out, "scalar {}", ty.name);
            }
            "OBJECT" | "INTERFACE" => {
                let keyword = if ty.kind == "OBJECT" { "type" } else { "interface" };
                let _ = write!(out, "{keyword} {}", ty.name);
                let implements: Vec<&str> = ty
                    .interfaces
                    .iter()
                    .flatten()
                    .filter_map(|interface| interface.name.as_deref())
                    .collect();
                if !implements.is_empty() {
                    let _ = write!(out, " implements {}", implements.join(" & "));
                }
                let fields = ty.fields.as_deref().unwrap_or_default();
                if fields.is_empty() {
                    out.push('\n');
                    continue;
                }
                out.push_str(" {\n");
                for field in fields {
                    description(&mut out, field.description.as_deref(), "  ");
                    let _ = write!(out, "  {}", field.name);
                    arguments(&mut out, &field.args);
                    let _ = write!(out, ": {}", type_ref(&field.ty));
                    deprecated(&mut out, field.is_deprecated, field.deprecation_reason.as_deref());
                    out.push('\n');
                }
                out.push_str("}\n");
            }
            "UNION" => {
                let members: Vec<&str> = ty
                    .possible_types
                    .iter()
                    .flatten()
                    .filter_map(|member| member.name.as_deref())
                    .collect();
                if members.is_empty() {
                    let _ = writeln!(out, "union {}", ty.name);
                } else {
                    let _ = writeln!(out, "union {} = {}", ty.name, members.join(" | "));
                }
            }
            "ENUM" => {
                let _ = write!(out, "enum {}", ty.name);
                let values = ty.enum_values.as_deref().unwrap_or_default();
                if values.is_empty() {
                    out.push('\n');
                    continue;
                }
                out.push_str(" {\n");
                for value in values {
                    description(&mut out, value.description.as_deref(), "  ");
                    let _ = write!(out, "  {}", value.name);
                    deprecated(&mut out, value.is_deprecated, value.deprecation_reason.as_deref());
                    out.push('\n');
                }
                out.push_str("}\n");
            }
            "INPUT_OBJECT" => {
                let _ = write!(out, "input {}", ty.name);
                let fields = ty.input_fields.as_deref().unwrap_or_default();
                if fields.is_empty() {
                    out.push('\n');
                    continue;
                }
                out.push_str(" {\n");
                for field in fields {
                    description(&mut out, field.description.as_deref(), "  ");
                    let _ = write!(out, "  {}: {}", field.name, type_ref(&field.ty));
                    if let Some(default) = &field.default_value {
                        let _ = write!(out, " = {default}");
                    }
                    out.push('\n');
                }
                out.push_str("}\n");
            }
            // A kind this printer does not know is skipped rather than guessed at; if anything
            // referred to it, the parse check refuses the file with the name in its message.
            _ => {}
        }
    }
    out
}

/// A description on its own line, or nothing.
fn description(out: &mut String, text: Option<&str>, indent: &str) {
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return;
    };
    let _ = writeln!(out, "{indent}{}", string_literal(text));
}

/// A description as SDL spells it: a `"""block"""`, which keeps a multi-line description readable
/// in a diff, with `"""` inside it escaped — the only sequence a block may not contain.
///
/// **Except when the text ends in `"`**, which no escape can put before a closing `"""`: the four
/// quotes would close the block one early and leave a stray `"`. Those fall back to an ordinary
/// string, quoted by `serde_json`, whose escapes are a subset of GraphQL's.
fn string_literal(text: &str) -> String {
    if text.ends_with('"') {
        return serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    }
    format!("\"\"\"{}\"\"\"", text.replace("\"\"\"", "\\\"\"\""))
}

fn arguments(out: &mut String, args: &[InputValue]) {
    if args.is_empty() {
        return;
    }
    let rendered: Vec<String> = args
        .iter()
        .map(|arg| {
            let mut text = String::new();
            if let Some(description) = arg.description.as_deref().map(str::trim)
                && !description.is_empty()
            {
                let _ = write!(text, "{} ", string_literal(description));
            }
            let _ = write!(text, "{}: {}", arg.name, type_ref(&arg.ty));
            if let Some(default) = &arg.default_value {
                let _ = write!(text, " = {default}");
            }
            text
        })
        .collect();
    let _ = write!(out, "({})", rendered.join(", "));
}

/// ` @deprecated` with its reason. JSON's string escapes are a subset of GraphQL's, so
/// `serde_json` quotes the reason safely.
fn deprecated(out: &mut String, is_deprecated: bool, reason: Option<&str>) {
    if !is_deprecated {
        return;
    }
    match reason {
        Some(reason) => {
            let quoted = serde_json::to_string(reason).unwrap_or_else(|_| "\"\"".to_string());
            let _ = write!(out, " @deprecated(reason: {quoted})");
        }
        None => out.push_str(" @deprecated"),
    }
}

/// `[User!]!` from its nested introspection form.
fn type_ref(ty: &TypeRef) -> String {
    match (ty.kind.as_str(), &ty.of_type) {
        ("NON_NULL", Some(inner)) => format!("{}!", type_ref(inner)),
        ("LIST", Some(inner)) => format!("[{}]", type_ref(inner)),
        _ => ty.name.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply shaped like a real server's, trimmed to one of everything the printer handles.
    const REPLY: &str = r#"{"data":{"__schema":{
      "queryType":{"name":"Query"},"mutationType":null,"subscriptionType":null,
      "directives":[
        {"name":"include","description":null,"locations":["FIELD"],"args":[]},
        {"name":"cached","description":"Cache it.","locations":["FIELD_DEFINITION","OBJECT"],
         "args":[{"name":"ttl","description":null,"defaultValue":"60",
                  "type":{"kind":"SCALAR","name":"Int","ofType":null}}]}
      ],
      "types":[
        {"kind":"OBJECT","name":"Query","description":null,"interfaces":[],
         "fields":[
           {"name":"user","description":"One user, by \"\"\"id\"\"\".","isDeprecated":false,
            "deprecationReason":null,
            "args":[{"name":"id","description":null,"defaultValue":null,
                     "type":{"kind":"NON_NULL","name":null,"ofType":{"kind":"SCALAR","name":"ID","ofType":null}}}],
            "type":{"kind":"OBJECT","name":"User","ofType":null}},
           {"name":"users","description":null,"isDeprecated":true,"deprecationReason":"Use \"search\".",
            "args":[],
            "type":{"kind":"NON_NULL","name":null,"ofType":{"kind":"LIST","name":null,
                    "ofType":{"kind":"NON_NULL","name":null,"ofType":{"kind":"OBJECT","name":"User","ofType":null}}}}}
         ],
         "inputFields":null,"enumValues":null,"possibleTypes":null},
        {"kind":"INTERFACE","name":"Node","description":null,"interfaces":[],
         "fields":[{"name":"id","description":"Opaque, like \"abc\"","isDeprecated":false,"deprecationReason":null,"args":[],
                    "type":{"kind":"NON_NULL","name":null,"ofType":{"kind":"SCALAR","name":"ID","ofType":null}}}],
         "inputFields":null,"enumValues":null,"possibleTypes":[{"kind":"OBJECT","name":"User","ofType":null}]},
        {"kind":"OBJECT","name":"User","description":null,
         "interfaces":[{"kind":"INTERFACE","name":"Node","ofType":null}],
         "fields":[
           {"name":"id","description":null,"isDeprecated":false,"deprecationReason":null,"args":[],
            "type":{"kind":"NON_NULL","name":null,"ofType":{"kind":"SCALAR","name":"ID","ofType":null}}},
           {"name":"role","description":null,"isDeprecated":false,"deprecationReason":null,"args":[],
            "type":{"kind":"ENUM","name":"Role","ofType":null}}
         ],
         "inputFields":null,"enumValues":null,"possibleTypes":null},
        {"kind":"ENUM","name":"Role","description":null,"fields":null,"inputFields":null,"interfaces":null,
         "enumValues":[{"name":"ADMIN","description":null,"isDeprecated":false,"deprecationReason":null},
                       {"name":"GUEST","description":null,"isDeprecated":true,"deprecationReason":null}],
         "possibleTypes":null},
        {"kind":"UNION","name":"Result","description":null,"fields":null,"inputFields":null,"interfaces":null,
         "enumValues":null,"possibleTypes":[{"kind":"OBJECT","name":"User","ofType":null}]},
        {"kind":"INPUT_OBJECT","name":"Filter","description":null,"fields":null,"interfaces":null,
         "enumValues":null,"possibleTypes":null,
         "inputFields":[{"name":"limit","description":null,"defaultValue":"10",
                         "type":{"kind":"SCALAR","name":"Int","ofType":null}}]},
        {"kind":"SCALAR","name":"DateTime","description":null,"fields":null,"inputFields":null,
         "interfaces":null,"enumValues":null,"possibleTypes":null},
        {"kind":"SCALAR","name":"String","description":null,"fields":null,"inputFields":null,
         "interfaces":null,"enumValues":null,"possibleTypes":null},
        {"kind":"OBJECT","name":"__Schema","description":null,"fields":[],"inputFields":null,
         "interfaces":[],"enumValues":null,"possibleTypes":null}
      ]}}}"#;

    #[test]
    fn a_reply_becomes_sdl_that_parses() {
        let (sdl, types) = to_sdl(REPLY.as_bytes()).expect("converts");
        assert_eq!(
            sdl,
            r#"schema {
  query: Query
}

"""Cache it."""
directive @cached(ttl: Int = 60) on FIELD_DEFINITION | OBJECT

type Query {
  """One user, by \"""id\"""."""
  user(id: ID!): User
  users: [User!]! @deprecated(reason: "Use \"search\".")
}

interface Node {
  "Opaque, like \"abc\""
  id: ID!
}

type User implements Node {
  id: ID!
  role: Role
}

enum Role {
  ADMIN
  GUEST @deprecated
}

union Result = User

input Filter {
  limit: Int = 10
}

scalar DateTime
"#
        );
        // Query, Node, User, Role, Result, Filter, DateTime — not the built-in scalars and not
        // the introspection types, which every server has and nobody wrote.
        assert_eq!(types, 7);
    }

    /// **The common failure, said in the server's own words**: production servers often turn
    /// introspection off, and "could not parse" would send someone looking for a bug.
    #[test]
    fn a_refusal_carries_the_servers_reason() {
        let reply = br#"{"data":null,"errors":[{"message":"GraphQL introspection is not allowed"}]}"#;
        assert_eq!(
            to_sdl(reply),
            Err(IntrospectionError::Refused(
                "GraphQL introspection is not allowed".to_string()
            ))
        );
        assert!(matches!(to_sdl(b"<html>502</html>"), Err(IntrospectionError::NotGraphQl(_))));
    }
}
