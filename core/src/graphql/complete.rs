//! Completion in a GraphQL document: what goes where the caret is.
//!
//! **Two halves that never meet until the end.** `context` reads the document up to the caret and
//! says *where* it is — inside which selection set, reached by which fields, or among which field's
//! arguments — without knowing any schema. `SchemaIndex::suggest` then says *what* goes there. The
//! split is what makes the hard half testable: a document is half-typed on nearly every keystroke,
//! and the scanner's whole job is to stay useful through that, so it is pinned by cases that need
//! no schema at all.
//!
//! **A tolerant scanner rather than a parser**, for `graphql.rs`'s reason and one more: a parser
//! answers "is this valid", and the question here is only "which `{` am I inside". Unbalanced
//! braces, a missing `}` at the end and a half-written argument are the normal state of the text
//! it reads, and each is simply wherever the caret stopped.

use std::ops::Range;

/// Which operation a selection set belongs to — or, in a fragment, which type it starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    Query,
    Mutation,
    Subscription,
    Type(String),
}

/// One step down from the root: into a field's type, or into an `... on Type` fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Field(String),
    On(String),
}

/// Where the caret is, in terms of what may be typed there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spot {
    /// Inside a selection set, where a field name goes.
    Field { root: Root, path: Vec<Step> },
    /// Inside a field's parentheses, where an argument name goes. `used` are the ones already
    /// written, which are not offered again.
    Argument {
        root: Root,
        path: Vec<Step>,
        field: String,
        used: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub spot: Spot,
    /// The part of a name typed before the caret — what the list is filtered by.
    pub prefix: String,
    /// The whole name around the caret, which is what accepting replaces — so accepting in the
    /// middle of a word replaces the word rather than splicing into it.
    pub replace: Range<usize>,
}

/// Where the caret at byte `cursor` is, or `None` where nothing is offered: in a string or a
/// comment, among variables or directives, inside an argument's value, or outside any operation.
pub fn context(document: &str, cursor: usize) -> Option<Context> {
    let cursor = cursor.min(document.len());
    if !document.is_char_boundary(cursor) {
        return None;
    }
    let bytes = document.as_bytes();

    let mut start = cursor;
    while start > 0 && is_name_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = cursor;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    // A name cannot start with a digit, so this is a number being typed.
    if bytes.get(start).is_some_and(u8::is_ascii_digit) && start < cursor {
        return None;
    }
    // `$id` and `@include` are variables and directives, which this does not complete.
    if start > 0 && matches!(bytes[start - 1], b'$' | b'@') {
        return None;
    }

    let tokens = lex(&document[..start])?;
    let spot = walk(&tokens)?;
    Some(Context {
        spot,
        prefix: document[start..cursor].to_string(),
        replace: start..end,
    })
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Punct(u8),
    Spread,
    /// A string or a number — a value, and nothing else matters about it here.
    Value,
}

/// The tokens before the caret, or `None` when the caret sits inside a string or a comment,
/// where nothing should be offered.
fn lex(text: &str) -> Option<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        match byte {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => i += 1,
            b'#' => match bytes[i..].iter().position(|&b| b == b'\n') {
                Some(offset) => i += offset + 1,
                None => return None,
            },
            b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                let rest = &text[i + 3..];
                let mut search = 0;
                loop {
                    let found = rest[search..].find("\"\"\"")?;
                    let at = search + found;
                    // `\"""` is an escaped triple quote inside a block string.
                    if at > 0 && rest.as_bytes()[at - 1] == b'\\' {
                        search = at + 3;
                        continue;
                    }
                    i += 3 + at + 3;
                    break;
                }
                tokens.push(Token::Value);
            }
            b'"' => {
                let mut j = i + 1;
                loop {
                    match bytes.get(j) {
                        None | Some(b'\n') => return None,
                        Some(b'\\') => j += 2,
                        Some(b'"') => break,
                        Some(_) => j += 1,
                    }
                }
                i = j + 1;
                tokens.push(Token::Value);
            }
            b'.' if bytes[i..].starts_with(b"...") => {
                tokens.push(Token::Spread);
                i += 3;
            }
            b'-' | b'0'..=b'9' => {
                let mut j = i + 1;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || matches!(bytes[j], b'.' | b'+' | b'-'))
                {
                    j += 1;
                }
                tokens.push(Token::Value);
                i = j;
            }
            _ if is_name_byte(byte) => {
                let mut j = i + 1;
                while j < bytes.len() && is_name_byte(bytes[j]) {
                    j += 1;
                }
                tokens.push(Token::Name(text[i..j].to_string()));
                i = j;
            }
            _ => {
                tokens.push(Token::Punct(byte));
                i += 1;
            }
        }
    }
    Some(tokens)
}

#[derive(PartialEq)]
enum Expect {
    Name,
    Colon,
    Value,
    Variable,
}

struct Arguments {
    field: String,
    used: Vec<String>,
    expect: Expect,
    /// Nesting inside a value — an input object or a list.
    depth: usize,
}

fn walk(tokens: &[Token]) -> Option<Spot> {
    let mut root: Option<Root> = None;
    let mut pending_root: Option<Root> = None;
    // The step each open selection set was entered by; `None` for the operation's own and for
    // an inline fragment with no type condition, which stays on the type around it.
    let mut frames: Vec<Option<Step>> = Vec::new();
    let mut last_field: Option<String> = None;
    // `...` seen (1), then `on` (2); a type condition's name closes it.
    let mut spread = 0u8;
    let mut pending_on: Option<String> = None;
    // `fragment` (1), its name (2), `on` (3).
    let mut fragment = 0u8;
    let mut directive = false;
    let mut directive_named = false;
    let mut skip_parens = 0usize;
    let mut arguments: Option<Arguments> = None;

    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        i += 1;

        if skip_parens > 0 {
            match token {
                Token::Punct(b'(') => skip_parens += 1,
                Token::Punct(b')') => skip_parens -= 1,
                _ => {}
            }
            continue;
        }

        if let Some(args) = arguments.as_mut() {
            if args.depth > 0 {
                match token {
                    Token::Punct(b'{' | b'[') => args.depth += 1,
                    Token::Punct(b'}' | b']') => {
                        args.depth -= 1;
                        if args.depth == 0 {
                            args.expect = Expect::Name;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match (token, &args.expect) {
                (Token::Punct(b')'), _) => arguments = None,
                (Token::Name(name), Expect::Name) => {
                    args.used.push(name.clone());
                    args.expect = Expect::Colon;
                }
                (Token::Punct(b':'), Expect::Colon) => args.expect = Expect::Value,
                (Token::Punct(b'{' | b'['), Expect::Value) => args.depth = 1,
                (Token::Punct(b'$'), Expect::Value) => args.expect = Expect::Variable,
                (Token::Name(_) | Token::Value, Expect::Value | Expect::Variable) => {
                    args.expect = Expect::Name;
                }
                _ => {}
            }
            continue;
        }

        let after_directive_name = std::mem::take(&mut directive_named);
        match token {
            Token::Punct(b'@') => directive = true,
            Token::Name(_) if directive => {
                directive = false;
                directive_named = true;
            }
            Token::Punct(b'(') if after_directive_name => skip_parens = 1,

            // Outside any selection set: the operation or fragment header.
            _ if frames.is_empty() => match token {
                Token::Name(name) if fragment == 0 => match name.as_str() {
                    "query" => pending_root = Some(Root::Query),
                    "mutation" => pending_root = Some(Root::Mutation),
                    "subscription" => pending_root = Some(Root::Subscription),
                    "fragment" => fragment = 1,
                    _ => {}
                },
                Token::Name(name) => {
                    fragment = match (fragment, name.as_str()) {
                        (1, _) => 2,
                        (2, "on") => 3,
                        (3, _) => {
                            pending_root = Some(Root::Type(name.clone()));
                            0
                        }
                        _ => 0,
                    };
                }
                // Variable definitions, whose `$id: ID!` is nothing to complete.
                Token::Punct(b'(') => skip_parens = 1,
                Token::Punct(b'{') => {
                    root = Some(pending_root.take().unwrap_or(Root::Query));
                    fragment = 0;
                    frames.push(None);
                    last_field = None;
                }
                _ => {}
            },

            Token::Spread => {
                spread = 1;
                last_field = None;
            }
            Token::Name(name) if spread == 1 && name == "on" => spread = 2,
            Token::Name(name) if spread == 2 => {
                pending_on = Some(name.clone());
                spread = 0;
            }
            // A named fragment spread: nothing opens after it.
            Token::Name(_) if spread == 1 => spread = 0,
            Token::Name(name) => {
                if matches!(tokens.get(i), Some(Token::Punct(b':'))) {
                    // `alias: field` — the field is the name after the colon.
                    last_field = None;
                    i += 1;
                } else {
                    last_field = Some(name.clone());
                }
            }
            Token::Punct(b'(') => match last_field.clone() {
                Some(field) => {
                    arguments = Some(Arguments {
                        field,
                        used: Vec::new(),
                        expect: Expect::Name,
                        depth: 0,
                    });
                }
                None => skip_parens = 1,
            },
            Token::Punct(b'{') => {
                let step = pending_on
                    .take()
                    .map(Step::On)
                    .or_else(|| last_field.take().map(Step::Field));
                frames.push(step);
                spread = 0;
            }
            Token::Punct(b'}') => {
                frames.pop();
                last_field = None;
                spread = 0;
                if frames.is_empty() {
                    root = None;
                }
            }
            _ => {}
        }
    }

    if skip_parens > 0 || directive || directive_named {
        return None;
    }
    let root = root?;
    if frames.is_empty() {
        return None;
    }
    let path: Vec<Step> = frames.into_iter().flatten().collect();

    if let Some(args) = arguments {
        return (args.expect == Expect::Name && args.depth == 0).then_some(Spot::Argument {
            root,
            path,
            field: args.field,
            used: args.used,
        });
    }
    // After `...` a fragment's name or `on` is typed, and after `on` a type: neither is a field.
    if spread != 0 || pending_on.is_some() {
        return None;
    }
    Some(Spot::Field { root, path })
}

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The field or argument name.
    pub label: String,
    /// Its type, as SDL writes it: `[User!]!`.
    pub detail: String,
    pub deprecated: bool,
    /// What accepting writes in place of `Context::replace`: the name, and for an argument the
    /// `: ` after it, since a value is the only thing that can follow.
    pub insert: String,
}

/// A parsed schema, ready to be asked what goes where.
#[derive(Debug, Clone)]
pub struct SchemaIndex {
    schema: apollo_compiler::Schema,
}

impl SchemaIndex {
    /// Parse SDL — a file `introspection` wrote, or one written by hand. A schema that fails
    /// *validation* still loads, for the reason `introspection::to_sdl` keeps one: it is what the
    /// server runs, whatever rule it breaks.
    pub fn parse(sdl: &str) -> Result<Self, String> {
        match apollo_compiler::Schema::parse(sdl.to_string(), "schema.graphql") {
            Ok(schema) => Ok(Self { schema }),
            Err(with_errors) => Err(with_errors.errors.to_string()),
        }
    }

    /// What may be typed at `spot`, filtered by `prefix`.
    ///
    /// **Prefix matches first, then names that merely contain it, and never fuzzy** — the
    /// header-name list's rule (§6l), for its reason: a list that reorders unpredictably as you
    /// type is one you stop reading. Case-insensitive, since `user` is how people start `userId`
    /// and `UserId` alike.
    pub fn suggest(&self, spot: &Spot, prefix: &str) -> Vec<Suggestion> {
        let candidates = match spot {
            Spot::Field { root, path } => self.fields(root, path),
            Spot::Argument {
                root,
                path,
                field,
                used,
            } => self.arguments(root, path, field, used),
        };

        let needle = prefix.to_ascii_lowercase();
        let (mut starts, mut contains): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
        for suggestion in candidates {
            let label = suggestion.label.to_ascii_lowercase();
            if label.starts_with(&needle) {
                starts.push(suggestion);
            } else if label.contains(&needle) {
                contains.push(suggestion);
            }
        }
        starts.append(&mut contains);
        starts
    }

    fn fields(&self, root: &Root, path: &[Step]) -> Vec<Suggestion> {
        let Some(ty) = self.resolve(root, path) else {
            return Vec::new();
        };
        let mut out: Vec<Suggestion> = match self.schema.types.get(ty.as_str()) {
            Some(apollo_compiler::schema::ExtendedType::Object(object)) => {
                object.fields.values().map(|field| field_row(field)).collect()
            }
            Some(apollo_compiler::schema::ExtendedType::Interface(interface)) => {
                interface.fields.values().map(|field| field_row(field)).collect()
            }
            // A union has no fields of its own; everything is reached through `... on`.
            Some(apollo_compiler::schema::ExtendedType::Union(_)) => Vec::new(),
            _ => return Vec::new(),
        };
        // Every composite type has it, and it is what distinguishes a union's members.
        out.push(Suggestion {
            label: "__typename".to_string(),
            detail: "String!".to_string(),
            deprecated: false,
            insert: "__typename".to_string(),
        });
        out
    }

    fn arguments(&self, root: &Root, path: &[Step], field: &str, used: &[String]) -> Vec<Suggestion> {
        let Some(ty) = self.resolve(root, path) else {
            return Vec::new();
        };
        let Some(definition) = self.field(&ty, field) else {
            return Vec::new();
        };
        definition
            .arguments
            .iter()
            .filter(|arg| !used.iter().any(|used| used == arg.name.as_str()))
            .map(|arg| Suggestion {
                label: arg.name.to_string(),
                detail: arg.ty.to_string(),
                deprecated: arg.directives.get("deprecated").is_some(),
                insert: format!("{}: ", arg.name),
            })
            .collect()
    }

    /// The type a selection set at `path` selects from.
    fn resolve(&self, root: &Root, path: &[Step]) -> Option<String> {
        let definition = &self.schema.schema_definition;
        let mut current = match root {
            Root::Query => definition.query.as_ref()?.name.to_string(),
            Root::Mutation => definition.mutation.as_ref()?.name.to_string(),
            Root::Subscription => definition.subscription.as_ref()?.name.to_string(),
            Root::Type(name) => name.clone(),
        };
        for step in path {
            current = match step {
                Step::Field(name) => self.field(&current, name)?.ty.inner_named_type().to_string(),
                Step::On(name) => name.clone(),
            };
        }
        Some(current)
    }

    fn field(&self, ty: &str, name: &str) -> Option<&apollo_compiler::schema::FieldDefinition> {
        match self.schema.types.get(ty)? {
            apollo_compiler::schema::ExtendedType::Object(object) => {
                object.fields.get(name).map(|field| &***field)
            }
            apollo_compiler::schema::ExtendedType::Interface(interface) => {
                interface.fields.get(name).map(|field| &***field)
            }
            _ => None,
        }
    }
}

fn field_row(field: &apollo_compiler::schema::FieldDefinition) -> Suggestion {
    Suggestion {
        label: field.name.to_string(),
        detail: field.ty.to_string(),
        deprecated: field.directives.get("deprecated").is_some(),
        insert: field.name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The context at the `|` in `marked`.
    fn at(marked: &str) -> Option<Context> {
        let cursor = marked.find('|').expect("a caret marker");
        let document = marked.replacen('|', "", 1);
        context(&document, cursor)
    }

    fn field(root: Root, path: &[Step]) -> Spot {
        Spot::Field { root, path: path.to_vec() }
    }

    fn step(name: &str) -> Step {
        Step::Field(name.to_string())
    }

    #[test]
    fn a_selection_set_offers_fields_from_where_it_is_nested() {
        let found = at("query { us|").expect("a field goes here");
        assert_eq!(found.spot, field(Root::Query, &[]));
        assert_eq!(found.prefix, "us");

        let found = at("{ user(id: 1) { na| } }").expect("nested");
        assert_eq!(found.spot, field(Root::Query, &[step("user")]));
        assert_eq!(found.replace, 16..18);

        assert_eq!(at("mutation M { cre|").map(|c| c.spot), Some(field(Root::Mutation, &[])));
        assert_eq!(
            at("subscription { |").map(|c| c.spot),
            Some(field(Root::Subscription, &[]))
        );
        // Back out of a closed selection set, into its parent.
        assert_eq!(at("{ me { name } |").map(|c| c.spot), Some(field(Root::Query, &[])));
    }

    /// Every construct between a field and its selection set that the scanner has to walk past
    /// without losing its place.
    #[test]
    fn aliases_variables_directives_and_fragments_keep_the_path() {
        assert_eq!(
            at("query Q($id: ID!, $n: Int = 5) { u: user(id: $id) @include(if: true) { na|")
                .map(|c| c.spot),
            Some(field(Root::Query, &[step("user")]))
        );
        assert_eq!(
            at("{ node(id: \"1\") { ... on User { na|").map(|c| c.spot),
            Some(field(Root::Query, &[step("node"), Step::On("User".into())]))
        );
        assert_eq!(
            at("fragment F on User { friends { na|").map(|c| c.spot),
            Some(field(Root::Type("User".into()), &[step("friends")]))
        );
        // An alias is typed first; the field after its colon is still a field.
        assert_eq!(at("{ a: us|").map(|c| c.spot), Some(field(Root::Query, &[])));
        // Arguments holding braces must not be read as selection sets.
        assert_eq!(
            at("{ search(filter: { tags: [\"a\", \"b\"] }) { na|").map(|c| c.spot),
            Some(field(Root::Query, &[step("search")]))
        );
    }

    #[test]
    fn inside_parentheses_offers_the_fields_arguments_not_yet_written() {
        let found = at("{ user(|").expect("an argument goes here");
        assert_eq!(
            found.spot,
            Spot::Argument {
                root: Root::Query,
                path: vec![],
                field: "user".into(),
                used: vec![],
            }
        );
        assert_eq!(
            at("{ me { posts(first: 10, af|").map(|c| c.spot),
            Some(Spot::Argument {
                root: Root::Query,
                path: vec![step("me")],
                field: "posts".into(),
                // The half-typed `af` is the prefix, not an argument already written.
                used: vec!["first".into()],
            })
        );
        assert_eq!(
            at("{ user(where: { id: 1 } |").map(|c| c.spot),
            Some(Spot::Argument {
                root: Root::Query,
                path: vec![],
                field: "user".into(),
                used: vec!["where".into()],
            })
        );
    }

    /// Where a name is not a field or an argument, nothing is offered rather than a wrong list.
    #[test]
    fn nothing_is_offered_where_no_field_or_argument_goes() {
        for marked in [
            "|",
            "query Q|",
            "{ user(id: |",
            "{ user(id: us|",
            "{ user(filter: { na|",
            "{ user(id: $i|",
            "{ user @inc|",
            "{ ...Fra|",
            "{ ... on Us|",
            "{ user(name: \"na|",
            "# a comment { |",
            "{ user } |",
            "query Q($|",
        ] {
            assert_eq!(at(marked), None, "{marked}");
        }
    }

    const SDL: &str = r#"
        schema { query: Query }
        type Query {
          user(id: ID!, verbose: Boolean): User
          users(first: Int, after: String): [User!]!
          node(id: ID!): Node
          result: Result
        }
        interface Node { id: ID! }
        type User implements Node {
          id: ID!
          name: String
          username: String @deprecated(reason: "Use name.")
          friends: [User!]!
        }
        union Result = User
    "#;

    #[test]
    fn a_schema_answers_with_fields_prefix_first() {
        let index = SchemaIndex::parse(SDL).expect("parses");
        let root = |path: &[Step]| field(Root::Query, path);

        let labels = |found: Vec<Suggestion>| -> Vec<String> {
            found.into_iter().map(|s| s.label).collect()
        };
        assert_eq!(labels(index.suggest(&root(&[]), "us")), ["user", "users"]);
        // Prefix before substring: `name`, then the two that merely contain it.
        let found = index.suggest(&root(&[step("user")]), "name");
        assert_eq!(labels(found.clone()), ["name", "username", "__typename"]);
        assert!(found[1].deprecated, "username is deprecated");
        assert_eq!(found[0].detail, "String");

        let friends = index.suggest(&root(&[step("users"), step("friends")]), "");
        assert_eq!(labels(friends), ["id", "name", "username", "friends", "__typename"]);
        // A union offers only `__typename`; its members are reached through `... on`.
        assert_eq!(labels(index.suggest(&root(&[step("result")]), "")), ["__typename"]);
        // An interface offers its own fields, and `... on User` offers User's.
        assert_eq!(labels(index.suggest(&root(&[step("node")]), "")), ["id", "__typename"]);
        assert_eq!(
            labels(index.suggest(&root(&[step("node"), Step::On("User".into())]), "fri")),
            ["friends"]
        );
        // A path through a field that does not exist offers nothing rather than guessing.
        assert!(index.suggest(&root(&[step("nope")]), "").is_empty());
    }

    #[test]
    fn a_schema_answers_with_the_arguments_left() {
        let index = SchemaIndex::parse(SDL).expect("parses");
        let spot = Spot::Argument {
            root: Root::Query,
            path: vec![],
            field: "user".into(),
            used: vec!["id".into()],
        };
        let found = index.suggest(&spot, "");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "verbose");
        assert_eq!(found[0].detail, "Boolean");
        assert_eq!(found[0].insert, "verbose: ", "a value is the only thing that can follow");
    }
}
