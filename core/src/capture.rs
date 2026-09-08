//! Capturing a value out of a response into an environment variable.
//!
//! **The rule lives on the request that *produces* the value, not on the one that consumes it.**
//! A client-credentials flow is "POST for a token, then use it", and writing it this way means the
//! consumer side needs nothing at all: `{{token}}` is an ordinary variable, resolved by the
//! `Resolver` that already exists. The alternative — "run X before me" — needs an ordering model,
//! a dependency graph and a story for a failed prerequisite, none of which is chaining. It would
//! also make the token invisible state, where this leaves it in a file you can read and edit.
//!
//! The cost, stated plainly: an expired token means re-sending the producer yourself. Re-running
//! it automatically is a later slice, and one that has somewhere to live precisely because the
//! capture rule is already recorded here.

use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentFile;
use crate::json::{JsonOutline, RowKind, unquote};

/// One "take this out of the response and put it in the environment" rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    pub enabled: bool,
    /// JSONPath into the response body, in the notation `JsonOutline::path_to` emits —
    /// `$.access_token`, `$.data[0].id`, `$["not-an-identifier"]`.
    pub path: String,
    /// The variable it writes: `token` for `{{token}}`.
    pub name: String,
    /// Which half of the environment it is written to. Defaults to the gitignored one, because
    /// the failure directions are not symmetric: a token reaching the committed file is a leak,
    /// and a placeholder reaching the gitignored one is an inconvenience.
    pub secret: bool,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            enabled: true,
            path: String::new(),
            name: String::new(),
            secret: true,
        }
    }
}

/// What one pass of `publish` did.
pub struct Published {
    /// Variable names written, in rule order.
    pub written: Vec<String>,
    /// Paths that matched nothing. Named rather than counted: "1 did not match" makes you go
    /// and find which.
    pub missed: Vec<String>,
    /// Whether anything became secret that was not already — the `.gitignore` trigger.
    pub new_secret: bool,
}

/// Apply a request's captures to an environment, in place.
///
/// **Here rather than at the call site, because the secret rule is invariant 10 and there are
/// two callers now** — a request's own send, and a collection run. Written twice it can be right
/// in one and quietly wrong in the other, which is the failure mode invariant 10 exists for.
///
/// The rule with the sharp edge: a secret name keeps its committed entry only when it was
/// *already* secret. Then the committed value is a separate placeholder and must survive. For a
/// name being marked secret now, the committed value **is** the thing being hidden, so carrying
/// it across would copy the token into the sidecar and leave the original in the file that gets
/// pushed.
pub fn publish(
    file: &mut EnvironmentFile,
    outline: &JsonOutline,
    captures: &[Capture],
) -> Published {
    let mut out = Published {
        written: Vec::new(),
        missed: Vec::new(),
        new_secret: false,
    };

    for capture in captures {
        let path = capture.path.trim();
        let name = capture.name.trim();
        if !capture.enabled || path.is_empty() || name.is_empty() {
            continue;
        }

        let Some(value) = extract(outline, path) else {
            out.missed.push(path.to_string());
            continue;
        };

        if capture.secret {
            let was_secret = file.local.contains_key(name);
            out.new_secret |= !was_secret;
            if !was_secret {
                file.committed.remove(name);
            }
            file.local.insert(name.to_string(), value);
        } else {
            file.local.remove(name);
            file.committed.insert(name.to_string(), value);
        }
        out.written.push(name.to_string());
    }

    out
}

enum Segment {
    Key(String),
    Index(usize),
}

/// Read the value at `path` out of an indexed response body.
///
/// **A descent, not a scan.** The obvious build — walk every row asking `path_to` whether it
/// matches — is O(n²), because `path_to` itself scans from the start of the document to collect
/// ancestors. That is fine for a token response and unusable for a 50MB one, and the difference
/// only shows up on the bodies nobody tests with. This walks the segments down instead, so the
/// cost is the depth of the path times the width of each level.
pub fn extract(outline: &JsonOutline, path: &str) -> Option<String> {
    let segments = segments(path)?;

    let mut current = 0usize;
    for segment in &segments {
        current = child(outline, current, segment)?;
    }

    let span = outline.value_span(current)?;
    let text = outline.text(span);
    // A captured string is wanted as its contents — a header reading `Bearer "abc"` is not what
    // anyone meant. Numbers, booleans and containers come through as written.
    Some(match outline.row(current).map(|row| row.kind) {
        Some(RowKind::Scalar(crate::json::ScalarKind::String)) => unquote(text),
        _ => text.to_string(),
    })
}

/// The direct child of the container at `parent` named by one segment.
fn child(outline: &JsonOutline, parent: usize, segment: &Segment) -> Option<usize> {
    let row = outline.row(parent)?;
    if !row.kind.is_open() {
        return None;
    }

    let close = parent + row.subtree_len as usize;
    let mut ix = parent + 1;
    let mut position = 0usize;

    while ix < close {
        let candidate = outline.row(ix)?;
        // Gated on the *container's* kind, not just on whether something sits at that position:
        // without this `$.data[0]` resolves against an object by matching its first key, so a
        // path with the wrong bracket captures a real value instead of reporting a miss.
        let matched = match segment {
            Segment::Key(name) => {
                row.kind == RowKind::ObjectOpen
                    && !candidate.key.is_none()
                    && unquote(outline.text(candidate.key)) == *name
            }
            Segment::Index(wanted) => row.kind == RowKind::ArrayOpen && *wanted == position,
        };
        if matched {
            return Some(ix);
        }

        position += 1;
        // An open row's own close is at `ix + subtree_len`, so its next sibling is one past it.
        ix += if candidate.kind.is_open() {
            candidate.subtree_len as usize + 1
        } else {
            1
        };
    }

    None
}

/// `$.data[0]["odd key"]` into segments, or `None` if it isn't a path this can follow.
///
/// Deliberately accepts only what `path_to` emits rather than the whole JSONPath grammar: every
/// path a user has is one Zuno wrote for them, and a wildcard or a filter would be a query
/// language whose results are a *set*, which a single variable has nowhere to put.
fn segments(path: &str) -> Option<Vec<Segment>> {
    let mut rest = path.strip_prefix('$')?;
    let mut out = Vec::new();

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let end = after
                .find(['.', '['])
                .unwrap_or(after.len());
            if end == 0 {
                return None;
            }
            out.push(Segment::Key(after[..end].to_string()));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']')?;
            let inner = &after[..end];
            out.push(if inner.starts_with('"') {
                Segment::Key(unquote(inner))
            } else {
                Segment::Index(inner.parse().ok()?)
            });
            rest = &after[end + 1..];
        } else {
            return None;
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn outline(json: &str) -> JsonOutline {
        JsonOutline::parse(Bytes::from(json.to_string())).expect("valid json")
    }

    #[test]
    fn a_captured_string_comes_out_without_its_quotes() {
        let doc = outline(r#"{"access_token":"abc123","expires_in":3600}"#);
        // The motivating case: this goes into `Authorization: Bearer {{token}}`, and a header
        // reading `Bearer "abc123"` is not what anyone meant.
        assert_eq!(extract(&doc, "$.access_token").as_deref(), Some("abc123"));
        // Everything else is taken as written — a number is not a string missing its quotes.
        assert_eq!(extract(&doc, "$.expires_in").as_deref(), Some("3600"));
    }

    #[test]
    fn a_path_can_descend_through_arrays_and_odd_keys() {
        let doc = outline(r#"{"data":[{"id":"first"},{"id":"second"}],"not-an-id":"x"}"#);
        assert_eq!(extract(&doc, "$.data[1].id").as_deref(), Some("second"));
        // `path_to` brackets a key that isn't an identifier, so `extract` has to read that back.
        assert_eq!(extract(&doc, r#"$["not-an-id"]"#).as_deref(), Some("x"));
    }

    #[test]
    fn a_path_that_does_not_match_returns_nothing_rather_than_empty() {
        let doc = outline(r#"{"data":{"id":"x"}}"#);
        // The distinction the caller needs: "the token wasn't there" must be reportable, and an
        // empty string written into `{{token}}` is a chain that fails on the *next* request.
        assert_eq!(extract(&doc, "$.token"), None);
        assert_eq!(extract(&doc, "$.data.missing"), None);
        assert_eq!(extract(&doc, "$.data[0]"), None, "an object has no index 0");
        assert_eq!(extract(&doc, "$.data.id.deeper"), None, "a scalar has no children");
    }

    #[test]
    fn a_malformed_path_is_refused_rather_than_guessed() {
        let doc = outline(r#"{"a":"1"}"#);
        assert_eq!(extract(&doc, "a"), None, "no root");
        assert_eq!(extract(&doc, "$."), None);
        assert_eq!(extract(&doc, "$[0"), None, "unclosed bracket");
        assert_eq!(extract(&doc, "$.a.").is_none(), true);
    }

    #[test]
    fn every_path_the_ui_can_copy_is_a_path_extract_can_follow() {
        // **The load-bearing one.** `path_to` is what `Alt+C` copies and what the capture editor
        // is filled from, so a path the writer emits and the reader cannot follow is a chain that
        // silently captures nothing. Pinning them to each other is the only way that stays true
        // as either changes.
        let doc = outline(
            r#"{"a":1,"b":{"c":[true,null,{"d-e":"deep"}]},"f":[[{"g":"nested"}]],"":"empty key"}"#,
        );

        let mut checked = 0;
        for ix in 0..doc.len() {
            let Some(path) = doc.path_to(ix) else { continue };
            let Some(span) = doc.value_span(ix) else { continue };
            // Containers are addressable but not capturable as a value; the scalars are the point.
            if !matches!(doc.row(ix).map(|row| row.kind), Some(RowKind::Scalar(_))) {
                continue;
            }

            let expected = match doc.row(ix).map(|row| row.kind) {
                Some(RowKind::Scalar(crate::json::ScalarKind::String)) => unquote(doc.text(span)),
                _ => doc.text(span).to_string(),
            };
            assert_eq!(
                extract(&doc, &path).as_deref(),
                Some(expected.as_str()),
                "{path} round-trips"
            );
            checked += 1;
        }

        // Without this the loop is vacuous if `path_to` ever starts returning `None` everywhere.
        assert_eq!(checked, 6, "every scalar in the fixture was checked");
    }

    fn file(committed: &[(&str, &str)], local: &[(&str, &str)]) -> EnvironmentFile {
        let map = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        EnvironmentFile {
            name: "dev".into(),
            committed: map(committed),
            local: map(local),
        }
    }

    fn rule(path: &str, name: &str, secret: bool) -> Capture {
        Capture {
            path: path.into(),
            name: name.into(),
            secret,
            enabled: true,
        }
    }

    #[test]
    fn publishing_an_already_secret_name_leaves_its_committed_placeholder_alone() {
        // **The distinction the app path got wrong.** It removed the committed entry for every
        // secret name, which deletes the placeholder half of the pattern `EnvironmentFile`
        // exists to keep — a value committed for whoever clones the repo, the real one local.
        let doc = outline(r#"{"access_token":"rotated"}"#);
        let mut env = file(&[("token", "ask-alice")], &[("token", "old")]);

        publish(&mut env, &doc, &[rule("$.access_token", "token", true)]);

        assert_eq!(env.local.get("token").map(String::as_str), Some("rotated"));
        assert_eq!(
            env.committed.get("token").map(String::as_str),
            Some("ask-alice"),
            "the placeholder is not the secret and must survive"
        );
    }

    #[test]
    fn publishing_a_newly_secret_name_takes_its_value_out_of_the_committed_file() {
        // The other direction, and the one invariant 10 is actually about: the committed value
        // *is* the thing being hidden, so leaving it there commits the token.
        let doc = outline(r#"{"access_token":"s3cret"}"#);
        let mut env = file(&[("token", "was-plain")], &[]);

        let published = publish(&mut env, &doc, &[rule("$.access_token", "token", true)]);

        assert!(!env.committed.contains_key("token"), "{:?}", env.committed);
        assert_eq!(env.local.get("token").map(String::as_str), Some("s3cret"));
        assert!(published.new_secret, "and it arms the gitignore offer");
    }

    #[test]
    fn publishing_reports_what_landed_and_what_missed() {
        let doc = outline(r#"{"id":"7"}"#);
        let mut env = file(&[], &[]);

        let published = publish(
            &mut env,
            &doc,
            &[
                rule("$.id", "id", false),
                rule("$.nope", "token", true),
                // Disabled and half-typed rows are skipped here, not by each caller.
                Capture { enabled: false, ..rule("$.id", "other", false) },
                rule("  ", "blank", false),
            ],
        );

        assert_eq!(published.written, vec!["id".to_string()]);
        assert_eq!(published.missed, vec!["$.nope".to_string()]);
        assert!(!published.new_secret, "nothing secret actually landed");
        assert_eq!(env.committed.get("id").map(String::as_str), Some("7"));
    }

    #[test]
    fn a_new_capture_is_secret_until_told_otherwise() {
        // The failure directions are not symmetric: a token in the committed file is a leak, a
        // placeholder in the gitignored one is an inconvenience.
        assert!(Capture::default().secret);
    }
}
