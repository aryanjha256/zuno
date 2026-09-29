//! Checking the Variables JSON against what the operation declares.
//!
//! **The half the query's validation cannot see.** A query can be perfect and still be refused,
//! because `$n: Int!` was sent `"5"`, or never sent at all — and the server's answer to that is an
//! error about *variables*, read off a response, after a round trip. This reads the JSON the way
//! GraphQL's input coercion will, against the operation that will actually run, and places each
//! problem on the text it is about.
//!
//! **The JSON is read twice**, on purpose: once by `serde_json`, for its values and its exact error
//! when the text is not JSON at all, and once by `top_level`, for *where* each top-level key and
//! value sit. `serde_json::Value` carries no positions, and an underline needs them. Nested
//! mistakes — a bad enum three objects deep — are placed on their variable's whole value, with the
//! path in the message, rather than hunted down character by character.

use std::ops::Range;

use apollo_compiler::ast::Type;
use apollo_compiler::schema::ExtendedType;
use serde_json::Value;

use super::complete::{Problem, SchemaIndex};

impl SchemaIndex {
    /// Every problem the variables text has for the operation `document` runs — `operation` by
    /// name, or the document's only one. Ranges are into `variables`.
    ///
    /// Nothing is reported when the operation cannot be found: a half-typed document is the
    /// query's validation to report, not a reason to underline every variable.
    pub fn check_variables(
        &self,
        document: &str,
        operation: Option<&str>,
        variables: &str,
    ) -> Vec<Problem> {
        let schema = apollo_compiler::validation::Valid::assume_valid_ref(self.schema());
        let executable =
            match apollo_compiler::ExecutableDocument::parse(schema, document, "query.graphql") {
                Ok(executable) => executable,
                Err(with_errors) => with_errors.partial,
            };
        let Ok(operation) = executable.operations.get(operation.filter(|name| !name.is_empty()))
        else {
            return Vec::new();
        };

        let trimmed = variables.trim();
        let values: serde_json::Map<String, Value> = if trimmed.is_empty() {
            serde_json::Map::new()
        } else {
            match serde_json::from_str::<Value>(variables) {
                Ok(Value::Object(map)) => map,
                Ok(_) => {
                    return vec![Problem {
                        range: whole(variables),
                        message: "variables must be a JSON object: { \"name\": value }".to_string(),
                    }];
                }
                Err(error) => {
                    let at = offset(variables, error.line(), error.column());
                    return vec![Problem {
                        range: one_char(variables, at),
                        message: format!("not valid JSON: {error}"),
                    }];
                }
            }
        };
        let places = top_level(variables);

        let mut problems = Vec::new();
        for (name, value) in &values {
            let place = places.iter().find(|place| place.key_name == *name);
            let Some(definition) = operation.variables.iter().find(|definition| definition.name == name.as_str())
            else {
                problems.push(Problem {
                    range: place.map_or_else(|| whole(variables), |place| place.key.clone()),
                    message: format!("`${name}` is not declared by the operation"),
                });
                continue;
            };
            if let Err(reason) = self.accepts(&definition.ty, value, &format!("${name}")) {
                problems.push(Problem {
                    range: place.map_or_else(|| whole(variables), |place| place.value.clone()),
                    message: reason,
                });
            }
        }

        // Required, and neither sent nor defaulted — the error with no text of its own to sit on,
        // so it marks the object's opening brace, or nothing when the editor is empty.
        for definition in &operation.variables {
            let required = definition.ty.is_non_null() && definition.default_value.is_none();
            if required && !values.contains_key(definition.name.as_str()) {
                problems.push(Problem {
                    range: opening(variables),
                    message: format!(
                        "`${}` ({}) is required and not set",
                        definition.name, definition.ty
                    ),
                });
            }
        }
        problems
    }

    /// Whether `value` coerces to `ty` as GraphQL input, or why not. `at` names where it is —
    /// `$filter.status` — so a nested mistake says which field it is about.
    fn accepts(&self, ty: &Type, value: &Value, at: &str) -> Result<(), String> {
        if value.is_null() {
            return if ty.is_non_null() {
                Err(format!("`{at}` is {ty} and cannot be null"))
            } else {
                Ok(())
            };
        }
        match ty {
            Type::Named(named) | Type::NonNullNamed(named) => self.accepts_named(named, value, at),
            Type::List(item) | Type::NonNullList(item) => match value {
                Value::Array(items) => items.iter().enumerate().try_for_each(|(ix, item_value)| {
                    self.accepts(item, item_value, &format!("{at}[{ix}]"))
                }),
                // Input coercion wraps a single value in a list, so `"a"` is a valid `[String]`.
                single => self.accepts(item, single, at),
            },
        }
    }

    fn accepts_named(&self, named: &str, value: &Value, at: &str) -> Result<(), String> {
        let wrong = |expected: &str| {
            Err(format!("`{at}` expects {expected}, got {}", describe(value)))
        };
        match named {
            "Int" => match value.as_i64() {
                Some(int) if i32::try_from(int).is_ok() => Ok(()),
                Some(_) => Err(format!("`{at}` is outside Int's 32-bit range")),
                None => wrong("Int"),
            },
            "Float" if value.is_number() => Ok(()),
            "Float" => wrong("Float"),
            "String" if value.is_string() => Ok(()),
            "String" => wrong("String"),
            "Boolean" if value.is_boolean() => Ok(()),
            "Boolean" => wrong("Boolean"),
            "ID" if value.is_string() || value.is_i64() || value.is_u64() => Ok(()),
            "ID" => wrong("ID"),
            _ => match self.schema().types.get(named) {
                Some(ExtendedType::Enum(enumeration)) => match value.as_str() {
                    Some(name) if enumeration.values.contains_key(name) => Ok(()),
                    Some(name) => Err(format!("`{at}`: `{name}` is not a value of {named}")),
                    None => wrong(named),
                },
                Some(ExtendedType::InputObject(input)) => {
                    let Value::Object(fields) = value else {
                        return wrong(&format!("an object ({named})"));
                    };
                    for key in fields.keys() {
                        if !input.fields.contains_key(key.as_str()) {
                            return Err(format!("`{at}.{key}` is not a field of {named}"));
                        }
                    }
                    for (field_name, field) in &input.fields {
                        match fields.get(field_name.as_str()) {
                            Some(field_value) => {
                                self.accepts(&field.ty, field_value, &format!("{at}.{field_name}"))?
                            }
                            None if field.ty.is_non_null() && field.default_value.is_none() => {
                                return Err(format!(
                                    "`{at}.{field_name}` ({}) is required and not set",
                                    field.ty
                                ));
                            }
                            None => {}
                        }
                    }
                    Ok(())
                }
                // A custom scalar's rules are the server's own, and unknowable here.
                _ => Ok(()),
            },
        }
    }
}

/// "a string", "the number 5" — what was actually sent, for a message.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => format!("the boolean {value}"),
        Value::Number(value) => format!("the number {value}"),
        Value::String(value) => format!("the string {:?}", value),
        Value::Array(_) => "a list".to_string(),
        Value::Object(_) => "an object".to_string(),
    }
}

/// One top-level `"key": value` pair's position.
struct Place {
    key_name: String,
    key: Range<usize>,
    value: Range<usize>,
}

/// Where each top-level key and value is, for text `serde_json` has already accepted as an object.
fn top_level(text: &str) -> Vec<Place> {
    let bytes = text.as_bytes();
    let mut places = Vec::new();
    let Some(open) = bytes.iter().position(|&b| b == b'{') else {
        return places;
    };
    let mut i = open + 1;
    loop {
        i = skip_blank(bytes, i);
        if bytes.get(i) != Some(&b'"') {
            break;
        }
        let key_start = i;
        i = string_end(bytes, i);
        let key = key_start..i;
        let key_name = serde_json::from_str::<String>(&text[key.clone()]).unwrap_or_default();
        i = skip_blank(bytes, i);
        if bytes.get(i) != Some(&b':') {
            break;
        }
        i = skip_blank(bytes, i + 1);
        let value_start = i;
        i = value_end(bytes, i);
        places.push(Place {
            key_name,
            key,
            value: value_start..i,
        });
        i = skip_blank(bytes, i);
        if bytes.get(i) == Some(&b',') {
            i += 1;
        } else {
            break;
        }
    }
    places
}

fn skip_blank(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// Just past the string starting at `i`, which is a `"`.
fn string_end(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Just past the value starting at `i`: a string, a nested object or list, or a bare word.
fn value_end(bytes: &[u8], mut i: usize) -> usize {
    match bytes.get(i) {
        Some(b'"') => string_end(bytes, i),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            while let Some(&byte) = bytes.get(i) {
                match byte {
                    b'"' => {
                        i = string_end(bytes, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            i
        }
        _ => {
            while bytes
                .get(i)
                .is_some_and(|byte| !matches!(byte, b',' | b'}' | b']') && !byte.is_ascii_whitespace())
            {
                i += 1;
            }
            i
        }
    }
}

/// `serde_json`'s 1-based line and column as a byte offset.
fn offset(text: &str, line: usize, column: usize) -> usize {
    let line_start: usize = text
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum();
    (line_start + column.saturating_sub(1)).min(text.len())
}

fn one_char(text: &str, at: usize) -> Range<usize> {
    let start = at.min(text.len());
    let end = text[start..]
        .chars()
        .next()
        .map_or(start, |ch| start + ch.len_utf8());
    if start == end && start > 0 {
        let prev = text[..start].chars().next_back().map_or(0, char::len_utf8);
        return start - prev..start;
    }
    start..end
}

fn whole(text: &str) -> Range<usize> {
    let start = text.len() - text.trim_start().len();
    start..text.trim_end().len().max(start)
}

/// The `{`, for a problem that belongs to the object as a whole; empty when there is no text.
fn opening(text: &str) -> Range<usize> {
    match text.find('{') {
        Some(at) => at..at + 1,
        None => 0..0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDL: &str = r#"
        schema { query: Query }
        type Query { users(first: Int, status: Status, filter: Filter): [String] }
        enum Status { ACTIVE DRAFT }
        input Filter { status: Status!, tags: [String!] }
    "#;
    const QUERY: &str =
        "query Q($first: Int!, $status: Status, $filter: Filter, $page: Int = 1) { users(first: $first) }";

    fn check(variables: &str) -> Vec<(String, String)> {
        let index = SchemaIndex::parse(SDL).expect("parses");
        index
            .check_variables(QUERY, None, variables)
            .into_iter()
            .map(|problem| (variables[problem.range].to_string(), problem.message))
            .collect()
    }

    #[test]
    fn values_that_coerce_have_no_problems() {
        assert_eq!(
            check(r#"{ "first": 5, "status": "ACTIVE", "filter": { "status": "DRAFT", "tags": "a" } }"#),
            []
        );
        // `$page` has a default, so leaving it out is fine; `$status` is nullable.
        assert_eq!(check(r#"{ "first": 1, "status": null }"#), []);
    }

    /// Each problem sits on the text it is about, which is what an underline needs.
    #[test]
    fn each_problem_is_placed_on_its_own_text() {
        let problems = check(r#"{ "first": "5", "stauts": "ACTIVE" }"#);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(problems[0].0, "\"5\"");
        assert!(problems[0].1.contains("expects Int"), "{problems:?}");
        assert_eq!(problems[1].0, "\"stauts\"");
        assert!(problems[1].1.contains("not declared"), "{problems:?}");

        // A nested mistake marks the variable's whole value and names the path.
        let problems = check(r#"{ "first": 1, "filter": { "status": "OPEN" } }"#);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].0, r#"{ "status": "OPEN" }"#);
        assert!(problems[0].1.contains("$filter.status"), "{problems:?}");

        let problems = check(r#"{ "first": 1, "filter": {} }"#);
        assert!(problems[0].1.contains("$filter.status") && problems[0].1.contains("required"));
    }

    #[test]
    fn a_required_variable_left_out_marks_the_object() {
        let problems = check("{ }");
        assert_eq!(problems, [("{".to_string(), "`$first` (Int!) is required and not set".to_string())]);
        // An empty editor still reports it, with nothing to underline.
        let index = SchemaIndex::parse(SDL).expect("parses");
        let problems = index.check_variables(QUERY, None, "");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].range.is_empty());
    }

    #[test]
    fn text_that_is_not_json_is_placed_where_it_breaks() {
        let problems = check("{ \"first\": 1,\n  \"status\": ACTIVE }");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].1.starts_with("not valid JSON"), "{problems:?}");
        assert_eq!(problems[0].0, "A");
    }
}
