//! Reading a schema rather than writing against it: the schema browser's content.
//!
//! **Plain data out, no rendering in**, so what the browser shows is decided here and tested
//! here — which types are listed and in what order, what a type's page says — and the app only
//! lays it out.

use apollo_compiler::schema::ExtendedType;

use super::complete::SchemaIndex;

/// One entry in the type list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeSummary {
    pub name: String,
    /// `type`, `interface`, `union`, `enum`, `input` or `scalar` — as SDL spells the keyword.
    pub kind: &'static str,
    /// `query`, `mutation` or `subscription` when this is that operation's root type.
    pub root: Option<&'static str>,
}

/// One type's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDetail {
    pub name: String,
    pub kind: &'static str,
    pub description: Option<String>,
    /// The interfaces it implements, a union's members, or an interface's implementers — the
    /// types it points to that are not fields. Each is a link.
    pub related: Vec<String>,
    /// What `related` is: "implements", "members" or "implemented by".
    pub related_label: &'static str,
    /// Fields, input fields or enum values, in schema order.
    pub entries: Vec<Entry>,
}

/// A field, an input field or an enum value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// The type as SDL writes it, `[User!]!`; empty for an enum value.
    pub ty: String,
    /// The named type inside `ty`, when the schema defines it — what clicking the type opens.
    /// `None` for built-in scalars, which have no page worth opening.
    pub target: Option<String>,
    pub arguments: Vec<Argument>,
    pub description: Option<String>,
    /// `Some` when deprecated, holding the reason if one was given.
    pub deprecated: Option<String>,
    /// An input field's or argument's default, as written.
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argument {
    pub name: String,
    pub ty: String,
    pub target: Option<String>,
    pub default: Option<String>,
}

const BUILT_IN_SCALARS: [&str; 5] = ["String", "Int", "Float", "Boolean", "ID"];

impl SchemaIndex {
    /// Every type a person would look up: **the operation roots first**, in query, mutation,
    /// subscription order — where every exploration starts — then everything else by name.
    /// Built-in scalars and the introspection types are left out; nobody browses to `String`.
    pub fn types(&self) -> Vec<TypeSummary> {
        let schema = self.schema();
        let definition = &schema.schema_definition;
        let roots = [
            ("query", definition.query.as_ref()),
            ("mutation", definition.mutation.as_ref()),
            ("subscription", definition.subscription.as_ref()),
        ];
        let root_of = |name: &str| {
            roots
                .iter()
                .find(|(_, root)| root.is_some_and(|root| root.name == name))
                .map(|(operation, _)| *operation)
        };

        let mut out: Vec<TypeSummary> = schema
            .types
            .iter()
            .filter(|(name, _)| !name.starts_with("__") && !BUILT_IN_SCALARS.contains(&name.as_str()))
            .map(|(name, ty)| TypeSummary {
                name: name.to_string(),
                kind: kind(ty),
                root: root_of(name),
            })
            .collect();
        let rank = |summary: &TypeSummary| match summary.root {
            Some("query") => 0,
            Some("mutation") => 1,
            Some("subscription") => 2,
            _ => 3,
        };
        out.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// One type's page, or `None` for a name the schema does not define.
    pub fn describe(&self, name: &str) -> Option<TypeDetail> {
        let schema = self.schema();
        let ty = schema.types.get(name)?;
        let target = |named: &str| {
            (schema.types.contains_key(named) && !BUILT_IN_SCALARS.contains(&named))
                .then(|| named.to_string())
        };
        let deprecation = |directives: &apollo_compiler::ast::DirectiveList| {
            directives.get("deprecated").map(|directive| {
                directive
                    .specified_argument_by_name("reason")
                    .and_then(|reason| reason.as_str().map(str::to_string))
                    .unwrap_or_default()
            })
        };
        let field_entry = |field: &apollo_compiler::schema::FieldDefinition| Entry {
            name: field.name.to_string(),
            ty: field.ty.to_string(),
            target: target(field.ty.inner_named_type()),
            arguments: field
                .arguments
                .iter()
                .map(|arg| Argument {
                    name: arg.name.to_string(),
                    ty: arg.ty.to_string(),
                    target: target(arg.ty.inner_named_type()),
                    default: arg.default_value.as_ref().map(|value| value.to_string()),
                })
                .collect(),
            description: field.description.as_ref().map(|text| text.to_string()),
            deprecated: deprecation(&field.directives),
            default: None,
        };

        let (related_label, related, entries) = match ty {
            ExtendedType::Object(object) => (
                "implements",
                object.implements_interfaces.iter().map(|name| name.to_string()).collect(),
                object.fields.values().map(|field| field_entry(field)).collect(),
            ),
            ExtendedType::Interface(interface) => (
                "implemented by",
                schema
                    .implementers_map()
                    .get(name)
                    .map(|found| {
                        found
                            .objects
                            .iter()
                            .chain(found.interfaces.iter())
                            .map(|name| name.to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                interface.fields.values().map(|field| field_entry(field)).collect(),
            ),
            ExtendedType::Union(union) => (
                "members",
                union.members.iter().map(|member| member.to_string()).collect(),
                Vec::new(),
            ),
            ExtendedType::Enum(enumeration) => (
                "",
                Vec::new(),
                enumeration
                    .values
                    .values()
                    .map(|value| Entry {
                        name: value.value.to_string(),
                        ty: String::new(),
                        target: None,
                        arguments: Vec::new(),
                        description: value.description.as_ref().map(|text| text.to_string()),
                        deprecated: deprecation(&value.directives),
                        default: None,
                    })
                    .collect(),
            ),
            ExtendedType::InputObject(input) => (
                "",
                Vec::new(),
                input
                    .fields
                    .values()
                    .map(|field| Entry {
                        name: field.name.to_string(),
                        ty: field.ty.to_string(),
                        target: target(field.ty.inner_named_type()),
                        arguments: Vec::new(),
                        description: field.description.as_ref().map(|text| text.to_string()),
                        deprecated: deprecation(&field.directives),
                        default: field.default_value.as_ref().map(|value| value.to_string()),
                    })
                    .collect(),
            ),
            ExtendedType::Scalar(_) => ("", Vec::new(), Vec::new()),
        };

        Some(TypeDetail {
            name: name.to_string(),
            kind: kind(ty),
            description: ty.description().map(|text| text.to_string()),
            related,
            related_label,
            entries,
        })
    }
}

fn kind(ty: &ExtendedType) -> &'static str {
    match ty {
        ExtendedType::Object(_) => "type",
        ExtendedType::Interface(_) => "interface",
        ExtendedType::Union(_) => "union",
        ExtendedType::Enum(_) => "enum",
        ExtendedType::InputObject(_) => "input",
        ExtendedType::Scalar(_) => "scalar",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDL: &str = r#"
        schema { query: Query mutation: Mutation }
        """The entry point."""
        type Query {
          "One user."
          user(id: ID!, verbose: Boolean = false): User
          search(filter: Filter): [Result!]!
        }
        type Mutation { rename(id: ID!, name: String!): User }
        interface Node { id: ID! }
        type User implements Node {
          id: ID!
          name: String @deprecated(reason: "Use displayName.")
          role: Role
        }
        type Post implements Node { id: ID! }
        union Result = User | Post
        enum Role { ADMIN GUEST @deprecated }
        input Filter { term: String!, limit: Int = 10 }
        scalar DateTime
    "#;

    #[test]
    fn the_list_starts_where_exploring_does() {
        let index = SchemaIndex::parse(SDL).expect("parses");
        let listed: Vec<(String, &str, Option<&str>)> = index
            .types()
            .into_iter()
            .map(|summary| (summary.name, summary.kind, summary.root))
            .collect();
        assert_eq!(
            listed,
            [
                ("Query".to_string(), "type", Some("query")),
                ("Mutation".to_string(), "type", Some("mutation")),
                ("DateTime".to_string(), "scalar", None),
                ("Filter".to_string(), "input", None),
                ("Node".to_string(), "interface", None),
                ("Post".to_string(), "type", None),
                ("Result".to_string(), "union", None),
                ("Role".to_string(), "enum", None),
                ("User".to_string(), "type", None),
            ]
        );
    }

    #[test]
    fn a_type_page_carries_what_the_browser_shows() {
        let index = SchemaIndex::parse(SDL).expect("parses");

        let query = index.describe("Query").expect("Query");
        assert_eq!(query.description.as_deref(), Some("The entry point."));
        let user = &query.entries[0];
        assert_eq!((user.name.as_str(), user.ty.as_str()), ("user", "User"));
        assert_eq!(user.target.as_deref(), Some("User"), "a link to User's page");
        assert_eq!(user.description.as_deref(), Some("One user."));
        assert_eq!(user.arguments[0].target, None, "ID is built in: nothing to open");
        assert_eq!(user.arguments[1].default.as_deref(), Some("false"));
        assert_eq!(query.entries[1].ty, "[Result!]!");
        assert_eq!(query.entries[1].target.as_deref(), Some("Result"));

        let person = index.describe("User").expect("User");
        assert_eq!((person.related_label, person.related.clone()), ("implements", vec!["Node".to_string()]));
        assert_eq!(person.entries[1].deprecated.as_deref(), Some("Use displayName."));

        let node = index.describe("Node").expect("Node");
        assert_eq!(node.related_label, "implemented by");
        assert_eq!(node.related, ["User", "Post"]);

        assert_eq!(index.describe("Result").expect("Result").related, ["User", "Post"]);
        let role = index.describe("Role").expect("Role");
        assert_eq!(role.entries[1].deprecated.as_deref(), Some(""), "deprecated, no reason");
        let filter = index.describe("Filter").expect("Filter");
        assert_eq!(filter.entries[1].default.as_deref(), Some("10"));

        assert!(index.describe("Nope").is_none());
    }
}
