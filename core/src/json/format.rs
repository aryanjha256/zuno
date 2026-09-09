//! Turning an outline back into JSON text.
//!
//! **The bytes are copied, never re-serialized**, which is the whole design. Every token — a key,
//! a number, a string with its escapes — is emitted from its `Span` in the original source, so
//! only the whitespace between tokens is this module's decision. Key order, number formatting and
//! escape sequences survive because nothing here is in a position to change them.
//!
//! **`serde_json::to_string_pretty` was the obvious answer and is wrong**, measured rather than
//! assumed: `serde_json = "1"` carries no `preserve_order`, so `Value`'s objects are a `BTreeMap`
//! and `{"zebra":1,"apple":2}` comes back `{"apple":2,"zebra":1}`. Silently reordering a request
//! body is not formatting it — key order is often deliberate, and a canonicalising signature
//! scheme makes it load-bearing. Enabling `preserve_order` would fix that one symptom and change
//! `Value` behaviour crate-wide, including the order `openapi.rs` walks `paths`.
//!
//! It also reuses the structure this codebase already trusts. `flatten` rejects structural errors,
//! so invalid JSON is *refused* by the parse rather than half-formatted here — and the response
//! viewer's outline is already built off the UI thread, so there is nothing new to arrange.

use super::{JsonOutline, RowKind};

/// Two spaces, matching what the response viewer draws and what every JSON tool emits.
const INDENT: &str = "  ";

/// Format an outline across lines.
pub fn pretty(outline: &JsonOutline) -> String {
    write(outline, true)
}

/// Format an outline onto one line, with no whitespace between tokens.
pub fn minify(outline: &JsonOutline) -> String {
    write(outline, false)
}

fn write(outline: &JsonOutline, indent: bool) -> String {
    let rows = outline.rows();
    // Sized from the source: the output is the same tokens plus or minus whitespace, so this is
    // the right order of magnitude and saves regrowing a multi-megabyte string.
    let mut out = String::with_capacity(outline.source().len() + rows.len());

    let mut ix = 0;
    while ix < rows.len() {
        let row = &rows[ix];

        if indent {
            for _ in 0..row.depth {
                out.push_str(INDENT);
            }
        }

        // A close row carries its *open* row's depth, so indentation is `depth` either way.
        if !row.key.is_none() {
            out.push_str(outline.text(row.key));
            out.push(':');
            if indent {
                out.push(' ');
            }
        }

        match row.kind {
            RowKind::Scalar(_) => out.push_str(outline.text(row.value)),
            RowKind::ObjectClose => out.push('}'),
            RowKind::ArrayClose => out.push(']'),
            RowKind::ObjectOpen | RowKind::ArrayOpen => {
                let (open, close) = match row.kind {
                    RowKind::ObjectOpen => ('{', '}'),
                    _ => ('[', ']'),
                };
                out.push(open);

                // An empty container goes on one line. `{\n}` is what a naive walk emits and it
                // reads as a mistake rather than as an empty object.
                if row.subtree_len == 1 {
                    out.push(close);
                    // The close row is consumed here, so its comma has to come from it — the
                    // open row of a container never carries one.
                    if rows[ix + 1].trailing_comma {
                        out.push(',');
                    }
                    if indent {
                        out.push('\n');
                    }
                    ix += 2;
                    continue;
                }
            }
        }

        if row.trailing_comma {
            out.push(',');
        }
        if indent {
            out.push('\n');
        }
        ix += 1;
    }

    // No trailing newline: this lands in an editor buffer, where a blank last line is something
    // the person then has to delete.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn outline(json: &str) -> JsonOutline {
        JsonOutline::parse(Bytes::from(json.to_string())).expect("valid json")
    }

    #[test]
    fn key_order_and_number_text_survive_formatting() {
        // The whole reason this is not `serde_json::to_string_pretty`, which reorders keys
        // alphabetically and would rewrite `1.0` through an f64. Measured, not assumed.
        let source = r#"{"zebra":1,"apple":2,"big":12345678901234567890,"exact":1.0}"#;
        let formatted = pretty(&outline(source));

        assert_eq!(
            formatted,
            "{\n  \"zebra\": 1,\n  \"apple\": 2,\n  \"big\": 12345678901234567890,\n  \"exact\": 1.0\n}"
        );
    }

    #[test]
    fn an_escape_is_copied_rather_than_re_encoded() {
        // Re-encoding is where a formatter turns `é` into `é` or the reverse, silently
        // changing bytes someone may have written that way on purpose.
        let source = r#"{"a":"café","b":"line\nbreak","c":"quote\"inside"}"#;
        let formatted = pretty(&outline(source));
        assert!(formatted.contains(r#""café""#), "{formatted}");
        assert!(formatted.contains(r#""line\nbreak""#), "{formatted}");
        assert!(formatted.contains(r#""quote\"inside""#), "{formatted}");
    }

    #[test]
    fn nesting_indents_and_an_empty_container_stays_on_one_line() {
        let source = r#"{"a":[1,{"b":null}],"empty":{},"none":[]}"#;
        assert_eq!(
            pretty(&outline(source)),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": null\n    }\n  ],\n  \"empty\": {},\n  \"none\": []\n}"
        );
    }

    #[test]
    fn a_scalar_at_the_root_formats_as_itself() {
        // `flatten` produces a single row for these, so the walk has to handle a document that
        // is not a container at all.
        assert_eq!(pretty(&outline(r#""hello""#)), r#""hello""#);
        assert_eq!(pretty(&outline("42")), "42");
        assert_eq!(pretty(&outline("null")), "null");
        assert_eq!(pretty(&outline("{}")), "{}");
        assert_eq!(pretty(&outline("[]")), "[]");
    }

    #[test]
    fn there_is_no_trailing_newline() {
        // This lands in an editor buffer, where a blank last line is something the person then
        // has to notice and delete.
        let formatted = pretty(&outline(r#"{"a":1}"#));
        assert!(!formatted.ends_with('\n'), "{formatted:?}");
    }

    #[test]
    fn minifying_removes_every_byte_of_whitespace_between_tokens() {
        let source = "{\n  \"a\": [ 1, 2 ],\n  \"b\": { }\n}";
        assert_eq!(minify(&outline(source)), r#"{"a":[1,2],"b":{}}"#);
        // Whitespace *inside* a string is content, not formatting.
        assert_eq!(minify(&outline(r#"{"a":"two  spaces"}"#)), r#"{"a":"two  spaces"}"#);
    }

    #[test]
    fn formatting_is_idempotent_and_round_trips_through_a_re_parse() {
        // The property that matters more than any single expected string: formatting twice must
        // not drift, and pretty-then-minify must land back on the minified original. Between
        // them these catch a comma emitted in the wrong place, which is the one thing in this
        // walk that is easy to get subtly wrong.
        let sources = [
            r#"{"a":1,"b":[1,2,{"c":3}],"d":{},"e":[[]],"f":null}"#,
            r#"[{"x":[1]},{},[],"s",7,true]"#,
            r#"{"nested":{"deep":{"deeper":{"deepest":[1,{"z":0}]}}}}"#,
        ];
        for source in sources {
            let once = pretty(&outline(source));
            let twice = pretty(&outline(&once));
            assert_eq!(once, twice, "not idempotent: {source}");

            assert_eq!(minify(&outline(&once)), source, "round trip: {source}");
        }
    }

    #[test]
    fn invalid_json_never_reaches_the_formatter() {
        // The refusal is the parse, which is the point of building on `flatten`: there is no
        // path here that half-formats a broken document.
        for broken in [r#"{"a":1,}"#, r#"{"a""#, "{", "[1,2", "not json"] {
            assert!(
                JsonOutline::parse(Bytes::from(broken.to_string())).is_err(),
                "{broken} must not parse"
            );
        }
    }
}
