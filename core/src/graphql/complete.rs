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
    /// After `argument:`, where its value goes — an enum's values, or `true`/`false`.
    Value {
        root: Root,
        path: Vec<Step>,
        field: String,
        argument: String,
    },
    /// After `argument: $`, where a variable goes. `declared` are the operation's own, as
    /// `(name, type)`, since a variable can only be one the operation declares.
    Variable {
        root: Root,
        path: Vec<Step>,
        field: String,
        argument: String,
        declared: Vec<(String, String)>,
    },
    /// After `on`, where a type goes. `within` is the selection set an inline fragment sits in,
    /// which limits it to the types that field can be; `None` for a `fragment X on` definition,
    /// which may name any composite type.
    TypeCondition { within: Option<(Root, Vec<Step>)> },
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
/// comment, after `@`, inside an input object or list, or outside any operation.
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
    // `@include` is a directive, which this does not complete.
    if start > 0 && bytes[start - 1] == b'@' {
        return None;
    }
    // `$id` is a variable — offered only where a value goes, since elsewhere a `$` is
    // declaring one, and a new name is not something a list can know.
    let dollar = start > 0 && bytes[start - 1] == b'$';

    let tokens = lex(&document[..if dollar { start - 1 } else { start }])?;
    let (spot, declared) = walk(&tokens)?;
    let spot = match (dollar, spot) {
        (
            true,
            Spot::Value {
                root,
                path,
                field,
                argument,
            },
        ) => Spot::Variable {
            root,
            path,
            field,
            argument,
            declared,
        },
        (true, _) => return None,
        (false, spot) => spot,
    };
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

/// Where the tokens leave the caret, and the variables the operation around it declares.
fn walk(tokens: &[Token]) -> Option<(Spot, Vec<(String, String)>)> {
    // The operation header's `($id: ID!, …)`, collected until its `)` and then read by
    // `definitions` — declared on the header, in force inside the operation's `{`.
    let mut defining: Option<(usize, Vec<Token>)> = None;
    let mut pending_declared: Vec<(String, String)> = Vec::new();
    let mut declared: Vec<(String, String)> = Vec::new();
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

        if let Some((depth, collected)) = defining.as_mut() {
            match token {
                Token::Punct(b'(') => *depth += 1,
                Token::Punct(b')') if *depth == 1 => {
                    pending_declared = definitions(collected);
                    defining = None;
                    continue;
                }
                Token::Punct(b')') => *depth -= 1,
                _ => {}
            }
            collected.push(token.clone());
            continue;
        }

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
                // Variable definitions: read, so a `$` later can offer them.
                Token::Punct(b'(') => defining = Some((1, Vec::new())),
                Token::Punct(b'{') => {
                    root = Some(pending_root.take().unwrap_or(Root::Query));
                    fragment = 0;
                    frames.push(None);
                    last_field = None;
                    declared = std::mem::take(&mut pending_declared);
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
                    declared.clear();
                }
            }
            _ => {}
        }
    }

    if skip_parens > 0 || defining.is_some() || directive || directive_named {
        return None;
    }
    if frames.is_empty() {
        // `fragment Name on |` — any composite type may follow.
        return (fragment == 3).then_some((Spot::TypeCondition { within: None }, Vec::new()));
    }
    let root = root?;
    let path: Vec<Step> = frames.into_iter().flatten().collect();

    let spot = if let Some(args) = arguments {
        if args.depth != 0 {
            // Inside an input object or a list: a later slice's question.
            return None;
        }
        match args.expect {
            Expect::Name => Spot::Argument {
                root,
                path,
                field: args.field,
                used: args.used,
            },
            Expect::Value => Spot::Value {
                root,
                path,
                field: args.field,
                // The name the `:` followed is the last one written.
                argument: args.used.last()?.clone(),
            },
            Expect::Colon | Expect::Variable => return None,
        }
    } else if spread == 2 {
        Spot::TypeCondition {
            within: Some((root, path)),
        }
    } else if spread != 0 || pending_on.is_some() {
        // After `...` a fragment's name is typed, and after `on Type` a `{`: neither is a field.
        return None;
    } else {
        Spot::Field { root, path }
    };
    Some((spot, declared))
}

/// `$id: ID!, $n: [Int!] = [1]` as `(name, type)` pairs, the type written as SDL writes it.
fn definitions(tokens: &[Token]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if let (Token::Punct(b'$'), Some(Token::Name(name)), Some(Token::Punct(b':'))) =
            (&tokens[i], tokens.get(i + 1), tokens.get(i + 2))
        {
            i += 3;
            let mut ty = String::new();
            while let Some(token) = tokens.get(i) {
                match token {
                    Token::Name(part) => ty.push_str(part),
                    Token::Punct(byte @ (b'[' | b']' | b'!')) => ty.push(*byte as char),
                    _ => break,
                }
                i += 1;
            }
            out.push((name.clone(), ty));
            continue;
        }
        i += 1;
    }
    out
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

    /// The parsed schema, for `variables`' checks.
    pub(super) fn schema(&self) -> &apollo_compiler::Schema {
        &self.schema
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
            Spot::Value {
                root,
                path,
                field,
                argument,
            } => self.values(root, path, field, argument),
            Spot::Variable {
                root,
                path,
                field,
                argument,
                declared,
            } => self.variables(root, path, field, argument, declared),
            Spot::TypeCondition { within } => self.type_conditions(within.as_ref()),
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

    /// The type an argument takes, as written — `Status!`, `[ID!]`.
    fn argument_type(
        &self,
        root: &Root,
        path: &[Step],
        field: &str,
        argument: &str,
    ) -> Option<&apollo_compiler::ast::Type> {
        let ty = self.resolve(root, path)?;
        let definition = self.field(&ty, field)?;
        definition
            .arguments
            .iter()
            .find(|arg| arg.name == argument)
            .map(|arg| &*arg.ty)
    }

    /// An enum argument's values, or `true`/`false` for a Boolean. Anything else is a value no
    /// list can know — a string, a number, an ID — and is offered nothing rather than a guess.
    fn values(&self, root: &Root, path: &[Step], field: &str, argument: &str) -> Vec<Suggestion> {
        let Some(ty) = self.argument_type(root, path, field, argument) else {
            return Vec::new();
        };
        let named = ty.inner_named_type();
        match self.schema.types.get(named) {
            Some(apollo_compiler::schema::ExtendedType::Enum(enumeration)) => enumeration
                .values
                .values()
                .map(|value| Suggestion {
                    label: value.value.to_string(),
                    detail: named.to_string(),
                    deprecated: value.directives.get("deprecated").is_some(),
                    insert: value.value.to_string(),
                })
                .collect(),
            _ if named == "Boolean" => ["true", "false"]
                .into_iter()
                .map(|value| Suggestion {
                    label: value.to_string(),
                    detail: "Boolean".to_string(),
                    deprecated: false,
                    insert: value.to_string(),
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The operation's declared variables, **those whose type fits the argument first** — a
    /// `$id: ID!` where the argument takes `ID!`. The rest still follow: GraphQL allows a
    /// non-null variable where a nullable argument is taken, and the validator is what says
    /// whether a particular pairing works, not this list.
    fn variables(
        &self,
        root: &Root,
        path: &[Step],
        field: &str,
        argument: &str,
        declared: &[(String, String)],
    ) -> Vec<Suggestion> {
        let wanted = self
            .argument_type(root, path, field, argument)
            .map(|ty| ty.to_string());
        let row = |(name, ty): &(String, String)| Suggestion {
            label: name.clone(),
            detail: ty.clone(),
            deprecated: false,
            insert: name.clone(),
        };
        let (fits, rest): (Vec<_>, Vec<_>) = declared
            .iter()
            .partition(|(_, ty)| wanted.as_deref() == Some(ty.as_str()));
        fits.into_iter().chain(rest).map(row).collect()
    }

    /// The types an `on` may name. Inside a selection set, the ones the field there can actually
    /// be — a union's members, an interface and its implementers, an object itself — since any
    /// other would never match. In a `fragment … on`, every object, interface and union.
    fn type_conditions(&self, within: Option<&(Root, Vec<Step>)>) -> Vec<Suggestion> {
        use apollo_compiler::schema::ExtendedType;

        let kind = |name: &str| match self.schema.types.get(name) {
            Some(ExtendedType::Object(_)) => "type",
            Some(ExtendedType::Interface(_)) => "interface",
            Some(ExtendedType::Union(_)) => "union",
            _ => "",
        };
        let row = |name: &str| Suggestion {
            label: name.to_string(),
            detail: kind(name).to_string(),
            deprecated: false,
            insert: name.to_string(),
        };

        let Some((root, path)) = within else {
            return self
                .schema
                .types
                .iter()
                .filter(|(name, ty)| {
                    !name.starts_with("__")
                        && matches!(
                            ty,
                            ExtendedType::Object(_)
                                | ExtendedType::Interface(_)
                                | ExtendedType::Union(_)
                        )
                })
                .map(|(name, _)| row(name))
                .collect();
        };
        let Some(current) = self.resolve(root, path) else {
            return Vec::new();
        };
        match self.schema.types.get(current.as_str()) {
            Some(ExtendedType::Union(union)) => {
                union.members.iter().map(|member| row(member)).collect()
            }
            Some(ExtendedType::Interface(_)) => {
                let implementers = self.schema.implementers_map();
                let mut out = vec![row(&current)];
                if let Some(found) = implementers.get(current.as_str()) {
                    out.extend(found.objects.iter().map(|name| row(name)));
                    out.extend(found.interfaces.iter().map(|name| row(name)));
                }
                out
            }
            Some(ExtendedType::Object(_)) => vec![row(&current)],
            _ => Vec::new(),
        }
    }

    /// Every problem `document` has against this schema, as byte ranges into it.
    ///
    /// **GraphQL's own validation, all of it**, through `apollo-compiler` — syntax, unknown
    /// fields, arguments and types, wrong argument types, missing required arguments, undefined
    /// variables and fragments. The schema is taken as valid without checking, for `parse`'s
    /// reason: it is what the server runs, and a rule it breaks is not the query's problem.
    ///
    /// An empty document has no problems. Nothing has been written yet, and "expected a
    /// definition" under an empty editor is noise.
    pub fn validate(&self, document: &str) -> Vec<Problem> {
        use apollo_compiler::diagnostic::ToCliReport as _;

        if document.trim().is_empty() {
            return Vec::new();
        }
        const PATH: &str = "query.graphql";
        let schema = apollo_compiler::validation::Valid::assume_valid_ref(&self.schema);
        let Err(with_errors) =
            apollo_compiler::ExecutableDocument::parse_and_validate(schema, document, PATH)
        else {
            return Vec::new();
        };

        let problems: Vec<Problem> = with_errors
            .errors
            .iter()
            .map(|diagnostic| {
                // Only locations inside the query itself: a diagnostic can also point into the
                // schema, and those offsets mean nothing in the editor.
                let span = diagnostic.error.location().filter(|span| {
                    diagnostic
                        .sources
                        .get(&span.file_id())
                        .is_some_and(|file| file.path() == std::path::Path::new(PATH))
                });
                let range = match span {
                    Some(span) => span.offset()..span.end_offset(),
                    None => 0..0,
                };
                Problem {
                    range: visible(range, document),
                    message: diagnostic.error.to_string(),
                }
            })
            .collect();

        // **The specific problem, not its echo.** One typo inside `user { nmae }` is reported
        // twice — once on `nmae`, and again on the whole of `user { … }`, which is left with no
        // valid selection. Underlining the whole field hides where the mistake is, so a problem
        // whose range contains another's is dropped in favour of the one inside it.
        problems
            .iter()
            .filter(|outer| {
                !problems.iter().any(|inner| {
                    inner.range != outer.range
                        && outer.range.start <= inner.range.start
                        && inner.range.end <= outer.range.end
                })
            })
            .cloned()
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

/// One problem in a document: where, and what the validator said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub range: Range<usize>,
    pub message: String,
}

/// A range at least one character wide, inside `document`, on character boundaries — so an error
/// reported *at* a point (a missing `}` at the end) still has something to underline.
fn visible(range: Range<usize>, document: &str) -> Range<usize> {
    let len = document.len();
    let mut start = range.start.min(len);
    let mut end = range.end.min(len);
    if start == end {
        if end < len {
            end += 1;
        } else if start > 0 {
            start -= 1;
        }
    }
    while start > 0 && !document.is_char_boundary(start) {
        start -= 1;
    }
    while end < len && !document.is_char_boundary(end) {
        end += 1;
    }
    start..end
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

    /// After `argument:` a value goes, after `argument: $` a declared variable, and after `on` a
    /// type — each its own spot, carrying what its answer needs.
    #[test]
    fn values_variables_and_type_conditions_have_spots_of_their_own() {
        let value = |argument: &str| Spot::Value {
            root: Root::Query,
            path: vec![],
            field: "users".into(),
            argument: argument.into(),
        };
        assert_eq!(at("{ users(status: |").map(|c| c.spot), Some(value("status")));
        let found = at("{ users(first: 2, status: AC|").expect("a value being typed");
        assert_eq!(found.spot, value("status"));
        assert_eq!(found.prefix, "AC");

        // The operation's declarations travel with a `$`, types and all.
        let found =
            at("query Q($s: Status!, $ids: [ID!] = [\"1\"]) { users(status: $|").expect("a variable");
        assert_eq!(
            found.spot,
            Spot::Variable {
                root: Root::Query,
                path: vec![],
                field: "users".into(),
                argument: "status".into(),
                declared: vec![("s".into(), "Status!".into()), ("ids".into(), "[ID!]".into())],
            }
        );
        // A `$` in the header is declaring a name, which no list can know.
        assert_eq!(at("query Q($|"), None);

        assert_eq!(
            at("{ node(id: 1) { ... on |").map(|c| c.spot),
            Some(Spot::TypeCondition {
                within: Some((Root::Query, vec![step("node")])),
            })
        );
        assert_eq!(
            at("fragment F on Us|").map(|c| c.spot),
            Some(Spot::TypeCondition { within: None })
        );
    }

    /// Where a name is not a field or an argument, nothing is offered rather than a wrong list.
    #[test]
    fn nothing_is_offered_where_no_field_or_argument_goes() {
        for marked in [
            "|",
            "query Q|",
            "{ user(filter: { na|",
            "{ user @inc|",
            "{ ...Fra|",
            "{ ... on User |",
            "{ user(name: \"na|",
            "{ user($|",
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
          users(first: Int, after: String, status: Status): [User!]!
          node(id: ID!): Node
          result: Result
        }
        enum Status { ACTIVE DRAFT ARCHIVED @deprecated }
        interface Node { id: ID! }
        type User implements Node {
          id: ID!
          name: String
          username: String @deprecated(reason: "Use name.")
          friends: [User!]!
        }
        type Post implements Node { id: ID! }
        union Result = User | Post
    "#;

    #[test]
    fn values_variables_and_type_conditions_are_answered_from_the_schema() {
        let index = SchemaIndex::parse(SDL).expect("parses");
        let labels = |found: Vec<Suggestion>| -> Vec<String> {
            found.into_iter().map(|s| s.label).collect()
        };
        let value = |field: &str, argument: &str| Spot::Value {
            root: Root::Query,
            path: vec![],
            field: field.into(),
            argument: argument.into(),
        };

        let status = index.suggest(&value("users", "status"), "");
        assert_eq!(labels(status.clone()), ["ACTIVE", "DRAFT", "ARCHIVED"]);
        assert!(status[2].deprecated && status[0].detail == "Status");
        assert_eq!(labels(index.suggest(&value("user", "verbose"), "")), ["true", "false"]);
        // A value no list can know — an ID — gets nothing rather than a guess.
        assert!(index.suggest(&value("user", "id"), "").is_empty());

        // The variable whose type fits comes first; the others still follow.
        let variables = index.suggest(
            &Spot::Variable {
                root: Root::Query,
                path: vec![],
                field: "user".into(),
                argument: "id".into(),
                declared: vec![("n".into(), "Int".into()), ("id".into(), "ID!".into())],
            },
            "",
        );
        assert_eq!(labels(variables), ["id", "n"]);

        let within = |field: &str| Spot::TypeCondition {
            within: Some((Root::Query, vec![step(field)])),
        };
        assert_eq!(labels(index.suggest(&within("result"), "")), ["User", "Post"]);
        assert_eq!(labels(index.suggest(&within("node"), "")), ["Node", "User", "Post"]);
        let anywhere = labels(index.suggest(&Spot::TypeCondition { within: None }, ""));
        for name in ["Query", "Node", "User", "Post", "Result"] {
            assert!(anywhere.iter().any(|label| label == name), "{name} in {anywhere:?}");
        }
        assert!(!anywhere.iter().any(|label| label == "Status"), "an enum is not a fragment's type");
    }

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

    /// **Problems are located where they are in the query**, which is what an underline needs,
    /// and an empty editor has none.
    #[test]
    fn validation_places_each_problem_in_the_query() {
        let index = SchemaIndex::parse(SDL).expect("parses");
        assert!(index.validate("").is_empty());
        assert!(index.validate("{ user(id: 1) { name } }").is_empty(), "a valid query");

        let query = "{ user(id: 1) { nmae } }";
        let problems = index.validate(query);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(&query[problems[0].range.clone()], "nmae", "{problems:?}");
        assert!(problems[0].message.contains("nmae"), "{problems:?}");

        // A required argument left out is reported on the field that needs it.
        let problems = index.validate("{ user { name } }");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].message.contains("id"), "{problems:?}");

        // A syntax error at the very end still gets a character to underline.
        let query = "{ user(id: 1) { name }";
        let problems = index.validate(query);
        assert!(!problems.is_empty());
        assert!(problems.iter().all(|problem| !problem.range.is_empty()), "{problems:?}");
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
