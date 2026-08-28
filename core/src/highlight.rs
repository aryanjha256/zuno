//! Token-level colouring for JSON, one line at a time.
//!
//! **A lexer, not a parser, and the distinction is the whole design.** `json::flatten` rejects
//! structural errors, which is right for a viewer and useless here: text being edited is invalid
//! on most keystrokes — you are part-way through a string, a brace is unclosed, a comma is
//! missing. Colour has to survive all of that, and it can, because knowing a token is a string
//! never requires knowing whether it nests correctly.
//!
//! **Stateless per line, which is what makes this cheap.** JSON strings cannot contain a raw
//! newline and there are no block comments, so no token spans a line break. Each line is lexed
//! independently, only the visible ones are lexed at all, and there is no cache to invalidate —
//! the "highlight cache" that made this look expensive is a requirement of XML and HTML, not of
//! JSON.
//!
//! Ranges are byte offsets into the line, never copies, matching `json::Span`.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A string used as an object key — a string with a `:` after it.
    Key,
    String,
    Number,
    /// `true`, `false`, `null`.
    Literal,
    /// `{}`, `[]`, `:`, `,`.
    Punct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub range: Range<usize>,
    pub kind: TokenKind,
}

/// Lex one line of JSON into coloured spans.
///
/// Anything unrecognised produces no token at all rather than an error, and renders in the
/// default text colour. That is the tolerance the editor needs: a half-typed line is mostly
/// recognisable and the recognisable part should still be coloured.
pub fn lex_json(line: &str) -> Vec<Token> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut ix = 0;

    while ix < bytes.len() {
        let byte = bytes[ix];

        if byte.is_ascii_whitespace() {
            ix += 1;
            continue;
        }

        match byte {
            b'"' => {
                let end = string_end(bytes, ix);
                // A key is a string with a `:` after it. That lookahead is the one piece of
                // context this needs, and it stays within the line — a pretty-printer never puts
                // the colon on the next line, and if one did the only cost is a colour.
                let kind = if next_is_colon(bytes, end) {
                    TokenKind::Key
                } else {
                    TokenKind::String
                };
                tokens.push(Token {
                    range: ix..end,
                    kind,
                });
                ix = end;
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                tokens.push(Token {
                    range: ix..ix + 1,
                    kind: TokenKind::Punct,
                });
                ix += 1;
            }
            b'-' | b'0'..=b'9' => {
                let end = number_end(bytes, ix);
                tokens.push(Token {
                    range: ix..end,
                    kind: TokenKind::Number,
                });
                ix = end;
            }
            b't' | b'f' | b'n' => match literal_end(bytes, ix) {
                Some(end) => {
                    tokens.push(Token {
                        range: ix..end,
                        kind: TokenKind::Literal,
                    });
                    ix = end;
                }
                None => ix += 1,
            },
            // Not JSON. Left uncoloured rather than guessed at.
            _ => ix += 1,
        }
    }

    tokens
}

/// Where the string starting at `open` ends, one past its closing quote.
///
/// An unterminated string runs to the end of the line and is still a string — which is exactly
/// the state of every string the moment you type its opening quote.
fn string_end(bytes: &[u8], open: usize) -> usize {
    let mut ix = open + 1;

    while ix < bytes.len() {
        match bytes[ix] {
            // Skip whatever follows, so `\"` does not close the string. Permissive about *what*
            // is escaped, matching `flatten` — this is an inspector, not a validator.
            b'\\' => ix += 2,
            b'"' => return ix + 1,
            _ => ix += 1,
        }
    }

    bytes.len()
}

/// Whether the next non-space byte after `from` is a `:`.
fn next_is_colon(bytes: &[u8], from: usize) -> bool {
    bytes[from..]
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b':')
}

/// Where the number starting at `start` ends.
///
/// Deliberately loose — it accepts `1.2.3` and `-` alone. A strict grammar would have to decide
/// what to do with the half-typed `-` that exists between two keystrokes, and colouring it as a
/// number is a better answer than colouring it as nothing.
fn number_end(bytes: &[u8], start: usize) -> usize {
    let mut ix = start + 1;

    while ix < bytes.len() {
        match bytes[ix] {
            b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-' => ix += 1,
            _ => break,
        }
    }

    ix
}

/// Where the keyword starting at `start` ends, if one starts there.
fn literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    for word in ["true", "false", "null"] {
        let end = start + word.len();
        if bytes.len() >= end && &bytes[start..end] == word.as_bytes() {
            // `nullable` is an identifier, not a keyword with a suffix.
            let bounded = bytes
                .get(end)
                .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_');
            if bounded {
                return Some(end);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<(TokenKind, &str)> {
        lex_json(line)
            .into_iter()
            .map(|token| (token.kind, &line[token.range]))
            .collect()
    }

    #[test]
    fn a_member_splits_into_key_punctuation_and_value() {
        assert_eq!(
            kinds(r#"  "name": "zuno","#),
            vec![
                (TokenKind::Key, r#""name""#),
                (TokenKind::Punct, ":"),
                (TokenKind::String, r#""zuno""#),
                (TokenKind::Punct, ","),
            ]
        );
    }

    #[test]
    fn a_key_is_told_from_a_string_by_the_colon_after_it() {
        // The one piece of context the lexer uses. Both are strings to a tokenizer; only the
        // following `:` makes one a key, and the response viewer gets this for free from the
        // outline while the editor has to look.
        assert_eq!(kinds(r#""a": "a""#)[0].0, TokenKind::Key);
        assert_eq!(kinds(r#""a": "a""#)[2].0, TokenKind::String);
        // Whitespace before the colon still counts.
        assert_eq!(kinds(r#""a"   : 1"#)[0].0, TokenKind::Key);
        // An array element is never a key.
        assert_eq!(kinds(r#"["a", "b"]"#)[1].0, TokenKind::String);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        assert_eq!(
            kinds(r#""say \"hi\"" "#),
            vec![(TokenKind::String, r#""say \"hi\"""#)]
        );
    }

    #[test]
    fn an_unterminated_string_runs_to_the_end_of_the_line() {
        // The state of every string between typing its opening quote and its closing one. A
        // parser would refuse the line; colour has to survive it.
        assert_eq!(
            kinds(r#"{"half": "typing"#),
            vec![
                (TokenKind::Punct, "{"),
                (TokenKind::Key, r#""half""#),
                (TokenKind::Punct, ":"),
                (TokenKind::String, r#""typing"#),
            ]
        );
    }

    #[test]
    fn a_trailing_backslash_does_not_run_past_the_end() {
        // `\` skips *two* bytes, so a backslash in the last position steps `ix` past the line.
        // The loop condition is what catches that, not a clamp on the return — one was written
        // and deleted, because breaking it on purpose changed nothing.
        assert_eq!(kinds(r#""oops\"#), vec![(TokenKind::String, r#""oops\"#)]);
    }

    #[test]
    fn numbers_and_keywords_are_recognised() {
        assert_eq!(
            kinds("[-1.5e10, true, false, null]"),
            vec![
                (TokenKind::Punct, "["),
                (TokenKind::Number, "-1.5e10"),
                (TokenKind::Punct, ","),
                (TokenKind::Literal, "true"),
                (TokenKind::Punct, ","),
                (TokenKind::Literal, "false"),
                (TokenKind::Punct, ","),
                (TokenKind::Literal, "null"),
                (TokenKind::Punct, "]"),
            ]
        );
    }

    #[test]
    fn a_keyword_needs_a_boundary_after_it() {
        // `nullable` is a word that happens to start with `null`. Colouring its first four
        // characters differently from the rest looks like a rendering fault.
        assert!(kinds(r#""nullable""#)[0].0 == TokenKind::String);
        assert_eq!(kinds("nullable"), vec![]);
        assert_eq!(kinds("null,"), vec![
            (TokenKind::Literal, "null"),
            (TokenKind::Punct, ","),
        ]);
    }

    #[test]
    fn unrecognised_text_produces_no_tokens_rather_than_an_error() {
        // Not JSON at all. The editor holds whatever you paste, and refusing to colour is a
        // better answer than guessing or panicking.
        // The quoted attribute is still a string; the tags around it are simply left alone.
        assert_eq!(
            kinds("<html lang=\"en\">"),
            vec![(TokenKind::String, "\"en\"")]
        );
        assert_eq!(kinds(""), vec![]);
        assert_eq!(kinds("      "), vec![]);
    }

    #[test]
    fn ranges_land_on_character_boundaries_in_a_utf8_line() {
        // Every split is at an ASCII structural byte, and UTF-8 continuation bytes are all
        // >= 0x80, so they can never be mistaken for one. Slicing the line by these ranges must
        // not panic.
        let line = r#"{"café": "naïve ☕", "n": 1}"#;
        for token in lex_json(line) {
            assert!(line.get(token.range.clone()).is_some(), "{token:?}");
        }
        assert_eq!(kinds(line)[1].1, r#""café""#);
    }
}
