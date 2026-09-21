//! Reading a GraphQL document well enough to route it.
//!
//! **Not a parser.** The only question asked here is which operation will run and what kind it
//! is, because a subscription needs a transport that stays open and a query does not. Validation
//! is the server's job and always was; a full grammar would be a large dependency answering a
//! question nobody asked.
//!
//! It is deliberately permissive in one direction: anything it cannot make sense of returns
//! `None`, and `None` means "send it the way we always did". A sniff that guesses wrong must
//! never be the reason a working request stops working.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Query,
    Mutation,
    Subscription,
}

/// Which kind of operation this document will run, if it can be told.
///
/// `operation` is the request's `operationName`, which decides between several definitions in
/// one document — the same field the server uses for the same purpose.
pub fn operation_kind(document: &str, operation: Option<&str>) -> Option<OperationKind> {
    let found = operations(document);

    match operation.map(str::trim).filter(|name| !name.is_empty()) {
        // Named: only that one matters, and a name the document does not define is a request
        // the server will reject anyway — answering `None` sends it and lets it say so.
        Some(name) => found
            .into_iter()
            .find(|(_, defined)| defined.as_deref() == Some(name))
            .map(|(kind, _)| kind),
        // Unnamed: unambiguous only when the document holds exactly one operation. Several with
        // no name chosen is a document the server refuses too.
        None => match found.as_slice() {
            [(kind, _)] => Some(*kind),
            _ => None,
        },
    }
}

/// Every operation definition in the document, in order.
fn operations(document: &str) -> Vec<(OperationKind, Option<String>)> {
    let bytes = document.as_bytes();
    let mut found = Vec::new();

    let mut at = 0usize;
    let mut depth = 0usize;
    // Variable definitions live in parentheses and may carry a default object value —
    // `($where: Filter = {id: 1})` — whose braces are not a selection set. Counted so they
    // cannot open one.
    let mut parens = 0usize;
    let mut kind: Option<OperationKind> = None;
    let mut name: Option<String> = None;
    let mut fragment = false;

    while at < bytes.len() {
        match bytes[at] {
            // A comment runs to the end of the line. Strings are skipped below, so a `#` here
            // is always a comment.
            b'#' => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'"' => at = skip_string(bytes, at),
            b'(' => {
                parens += 1;
                at += 1;
            }
            b')' => {
                parens = parens.saturating_sub(1);
                at += 1;
            }
            b'{' => {
                if depth == 0 && parens == 0 {
                    if fragment {
                        fragment = false;
                    } else {
                        // A bare `{ … }` with no keyword is shorthand for a query.
                        found.push((kind.take().unwrap_or(OperationKind::Query), name.take()));
                    }
                }
                if parens == 0 {
                    depth += 1;
                }
                at += 1;
            }
            b'}' => {
                if parens == 0 {
                    depth = depth.saturating_sub(1);
                }
                at += 1;
            }
            c if depth == 0 && parens == 0 && (c.is_ascii_alphabetic() || c == b'_') => {
                let start = at;
                while at < bytes.len()
                    && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_')
                {
                    at += 1;
                }
                match &document[start..at] {
                    "query" => (kind, name) = (Some(OperationKind::Query), None),
                    "mutation" => (kind, name) = (Some(OperationKind::Mutation), None),
                    "subscription" => (kind, name) = (Some(OperationKind::Subscription), None),
                    // A fragment is a definition too, and its body must not be mistaken for an
                    // operation's — leaving this out makes every document with a fragment
                    // report one operation too many.
                    "fragment" => {
                        fragment = true;
                        kind = None;
                        name = None;
                    }
                    word => {
                        if kind.is_some() && name.is_none() {
                            name = Some(word.to_string());
                        }
                    }
                }
            }
            _ => at += 1,
        }
    }

    found
}

/// Past a string, including a `"""block"""` one, whose contents may hold anything at all.
fn skip_string(bytes: &[u8], from: usize) -> usize {
    if bytes[from..].starts_with(b"\"\"\"") {
        let mut at = from + 3;
        while at + 2 < bytes.len() {
            if &bytes[at..at + 3] == b"\"\"\"" {
                return at + 3;
            }
            at += 1;
        }
        return bytes.len();
    }

    let mut at = from + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(document: &str) -> Option<OperationKind> {
        operation_kind(document, None)
    }

    #[test]
    fn the_leading_keyword_decides() {
        assert_eq!(kind("subscription { greetings }"), Some(OperationKind::Subscription));
        assert_eq!(kind("mutation { add(n: 1) { id } }"), Some(OperationKind::Mutation));
        assert_eq!(kind("query Me { me { id } }"), Some(OperationKind::Query));
    }

    /// Shorthand: a bare selection set is a query, and it is what most examples use.
    #[test]
    fn a_bare_selection_set_is_a_query() {
        assert_eq!(kind("{ me { id } }"), Some(OperationKind::Query));
        assert_eq!(kind("  \n  { a }"), Some(OperationKind::Query));
    }

    /// **A fragment is a definition, not an operation.** Counting it makes a one-operation
    /// document look ambiguous, which would silently route a subscription over HTTP.
    #[test]
    fn fragments_are_not_operations() {
        let document = "fragment Bits on Thing { id name }\nsubscription { thing { ...Bits } }";
        assert_eq!(kind(document), Some(OperationKind::Subscription));
    }

    /// A default value in the variable definitions has braces that are not a selection set.
    #[test]
    fn a_default_object_value_does_not_open_an_operation() {
        let document = "subscription S($where: Filter = {id: 1}) { events { id } }";
        assert_eq!(kind(document), Some(OperationKind::Subscription));
    }

    #[test]
    fn comments_and_strings_are_skipped() {
        assert_eq!(
            kind("# query Nope { a }\nsubscription { b }"),
            Some(OperationKind::Subscription)
        );
        // A brace inside a string must not open a selection set.
        assert_eq!(
            kind("subscription S($s: String = \"{\") { b }"),
            Some(OperationKind::Subscription)
        );
        assert_eq!(
            kind("subscription S($s: String = \"\"\"a { b\"\"\") { c }"),
            Some(OperationKind::Subscription)
        );
    }

    /// **Several operations need the name that picks one**, which is exactly what
    /// `operationName` is for. Without it the server refuses too, so `None` is the honest
    /// answer rather than a guess at the first.
    #[test]
    fn several_operations_are_told_apart_by_name() {
        let document = "query A { a }\nsubscription B { b }";
        assert_eq!(operation_kind(document, None), None);
        assert_eq!(operation_kind(document, Some("A")), Some(OperationKind::Query));
        assert_eq!(
            operation_kind(document, Some("B")),
            Some(OperationKind::Subscription)
        );
        assert_eq!(operation_kind(document, Some("Missing")), None);
    }

    /// Nothing to read is not a subscription, and must not become one.
    #[test]
    fn an_empty_or_unreadable_document_answers_nothing() {
        assert_eq!(kind(""), None);
        assert_eq!(kind("   \n # just a comment"), None);
    }
}
