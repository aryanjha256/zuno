//! What an import yields, and which parser to hand a document to.
//!
//! **One result shape, two parsers, one writer.** `openapi` and `postman` read documents with
//! almost nothing in common and both answer with an `Import`, so the half that creates
//! directories, allocates free filenames, writes an environment and reports what was dropped is
//! written once. A third format is a parser and a sniff arm, not another writer.
//!
//! **The format is sniffed, never chosen.** A separate "Import from Postman" verb would make
//! someone classify their own export before they could use it, which is the friction this
//! feature exists to remove — paste a path and it works. The cost is paid here instead: every
//! shape we can *recognise but not read* gets its own refusal, because "this is a Postman
//! environment export" is an answer and "unrecognised document" is a dead end.

use serde_json::Value;

use crate::RequestSpec;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    OpenApi(#[from] crate::openapi::OpenApiError),
    #[error(transparent)]
    Postman(#[from] crate::postman::PostmanError),
    #[error("Swagger 2.0 is a different format from OpenAPI 3.x, which is what Zuno reads")]
    Swagger,
    #[error("this is a Postman v1 collection — re-export it from Postman as v2.1")]
    PostmanV1,
    #[error("not a document Zuno can read — expected an OpenAPI 3.x spec or a Postman collection")]
    Unrecognised,
}

/// What a document turned out to be.
///
/// Two variants rather than one `Import` with an empty `requests`, because they are different
/// *outcomes* and not a difference of degree: a collection is written into the tree, an
/// environment into `environments/`, and a caller that forgot the distinction would report "no
/// requests to import" about a perfectly good environment export.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Collection(Import),
    Environment(EnvironmentImport),
}

/// A Postman environment or globals export: variables and nothing else.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EnvironmentImport {
    /// The label to write it under. **`None` for a globals export**, which has no name of its
    /// own — it is Postman's always-on base layer and maps exactly onto Zuno's, which is not a
    /// name anyone picks.
    pub name: Option<String>,
    pub variables: Vec<Variable>,
    pub skipped: Vec<String>,
}

/// One request an import yielded, and where it belongs.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    /// Nested folders, outermost first, relative to the import's own folder. Empty means the
    /// request sits directly in it.
    ///
    /// A `Vec` rather than one optional name because Postman folders nest arbitrarily and an
    /// API arriving flattened is an API someone has to re-file by hand. OpenAPI fills it with
    /// at most one entry — the operation's first tag.
    pub folders: Vec<String>,
    pub spec: RequestSpec,
}

/// A variable an import brought with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    /// Written to the gitignored half. Postman marks these itself (`type: "secret"`), which is
    /// the same distinction invariant 10 draws with a file split — so the marking survives the
    /// crossing rather than being guessed at from the name.
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Import {
    pub requests: Vec<Imported>,
    /// What to name the folder everything lands in: `info.title` or `info.name`.
    pub title: Option<String>,
    /// Collection-level variables, for the environment written alongside the requests. An
    /// import whose every URL starts `{{baseUrl}}` and that carries no `baseUrl` is an import
    /// you cannot send, which is not an import.
    pub variables: Vec<Variable>,
    /// How many `expect_status`, capture and assertion rules were recovered from Postman test
    /// scripts. A count rather than a list, because the rules themselves are on the requests and
    /// visible in the Assert tab — this exists so the report can say the scripts were read at
    /// all, rather than leaving "12 skipped" to imply nothing came across.
    pub recovered: usize,
    /// What was dropped, in words. Surfaced rather than logged: an import that silently loses
    /// part of a document is worse than one that says so.
    pub skipped: Vec<String>,
}

/// Read a document, whatever it is.
///
/// The JSON is parsed **once** and the `Value` handed to whichever parser claims it, rather than
/// sniffing on bytes and letting the parser start over. A Postman export of a real workspace
/// runs to megabytes, and this runs on the UI thread's caller.
pub fn parse(bytes: &[u8]) -> Result<Parsed, ImportError> {
    let root: Value = serde_json::from_slice(bytes)?;

    // **The Postman API wraps what the Postman app exports.** A share link
    // (`api.postman.com/collections/<uid>?access_key=…`) answers `{"collection": {…}}`, while a
    // file exported from the app is the bare object. Unwrapped here rather than in `postman.rs`
    // because it is an envelope of the *transport*, not a version of the collection format —
    // and the link is the more useful of the two paths, since it needs no export step at all.
    // Gated on the inner `item` so an unrelated `collection` key cannot claim a document.
    let root = root
        .get("collection")
        .filter(|collection| collection.get("item").is_some())
        .unwrap_or(&root);

    // Ordered most specific first. Every arm below `openapi` is a shape we can name but not
    // read, and naming it is the whole point — see the module note.
    if root.get("openapi").is_some() {
        return Ok(Parsed::Collection(crate::openapi::parse(root)?));
    }
    if root.get("swagger").is_some() {
        return Err(ImportError::Swagger);
    }
    if root.get("item").is_some() {
        return Ok(Parsed::Collection(crate::postman::parse(root)?));
    }
    // A Postman environment export is `{ "name", "values": [...] }`. The second key is not
    // distinctive enough on its own — plenty of JSON has a top-level `values` — so this also
    // wants one of the two things every real export carries.
    if root.get("values").and_then(Value::as_array).is_some()
        && (root.get("_postman_variable_scope").is_some()
            || root.get("name").and_then(Value::as_str).is_some())
    {
        return Ok(Parsed::Environment(crate::postman::parse_environment(root)?));
    }
    // v1 is a different document rather than an older version of v2: a top-level `requests`
    // array with no `item` anywhere.
    if root.get("requests").and_then(Value::as_array).is_some() {
        return Err(ImportError::PostmanV1);
    }

    Err(ImportError::Unrecognised)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of these is a document someone will actually drop on the field, and the message is
    /// the entire feature: a refusal that says which format it saw is a next step, and
    /// "unrecognised" is a shrug.
    #[test]
    fn every_shape_we_can_name_is_refused_by_name() {
        let cases = [
            (r#"{"swagger":"2.0","paths":{}}"#, "Swagger 2.0"),
            (r#"{"id":"x","name":"Old","requests":[]}"#, "Postman v1"),
            (r#"{"totally":"unrelated"}"#, "not a document Zuno can read"),
        ];
        for (document, expected) in cases {
            let error = parse(document.as_bytes()).expect_err(document);
            assert!(
                error.to_string().contains(expected),
                "{document} said {error:?}, wanted {expected:?}"
            );
        }

        assert!(matches!(parse(b"not json"), Err(ImportError::Json(_))));
    }

    fn collection(document: &[u8]) -> Import {
        match parse(document).expect("parse") {
            Parsed::Collection(import) => import,
            other => panic!("wanted a collection, got {other:?}"),
        }
    }

    fn environment(document: &[u8]) -> EnvironmentImport {
        match parse(document).expect("parse") {
            Parsed::Environment(import) => import,
            other => panic!("wanted an environment, got {other:?}"),
        }
    }

    #[test]
    fn a_document_is_routed_to_the_parser_that_claims_it() {
        let openapi =
            collection(br#"{"openapi":"3.0.0","info":{"title":"A"},"paths":{"/p":{"get":{}}}}"#);
        assert_eq!(openapi.title.as_deref(), Some("A"));

        let postman = collection(
            br#"{"info":{"name":"B"},"item":[{"name":"r","request":{"method":"GET","url":"https://a.test"}}]}"#,
        );
        assert_eq!(postman.title.as_deref(), Some("B"));

        // An environment export is a different *outcome*, not a collection with no requests.
        let staging = environment(
            br#"{"name":"Staging","_postman_variable_scope":"environment",
                 "values":[{"key":"baseUrl","value":"https://s.test","enabled":true}]}"#,
        );
        assert_eq!(staging.name.as_deref(), Some("Staging"));
        assert_eq!(staging.variables.len(), 1);

        // And a globals export drops its name, because the layer it maps to is not one you pick.
        let globals = environment(
            br#"{"name":"My Workspace Globals","_postman_variable_scope":"globals",
                 "values":[{"key":"v","value":"1","enabled":true}]}"#,
        );
        assert_eq!(globals.name, None);
    }

    #[test]
    fn a_collection_fetched_from_the_postman_api_reads_the_same_as_an_exported_file() {
        // The API wraps it in `{"collection": …}` and the app's export does not. This shipped
        // reading only the export, so pasting a share link — the path that needs no export step
        // at all, and so the one someone reaches for first — said "not a document Zuno can
        // read" about a perfectly good collection.
        let wrapped = collection(
            br#"{"collection":{"info":{"name":"C"},"item":[
                 {"name":"r","request":{"method":"GET","url":"https://a.test"}}]}}"#,
        );
        assert_eq!(wrapped.title.as_deref(), Some("C"));
        assert_eq!(wrapped.requests.len(), 1);

        // And a `collection` key that is not one must not hijack the document.
        assert!(matches!(
            parse(br#"{"collection":{"name":"not a collection"}}"#),
            Err(ImportError::Unrecognised)
        ));
    }
}
