//! A JSON tokenizer that emits flat rows with byte spans.
//!
//! **Iterative, not recursive.** A viewer eats arbitrary server output, and deeply
//! nested JSON is a trivially easy way to blow a recursive parser's stack. The explicit
//! `stack` here means nesting depth costs heap, not stack frames.
//!
//! **Permissive about string contents, strict about structure.** `\u` escapes aren't
//! validated beyond "there is a byte after the backslash" — this is an inspector, not a
//! validator, and refusing to display a response over a malformed escape would be
//! actively unhelpful. Structural errors (a missing bracket, a stray comma) *are*
//! rejected, because those mean the flattening would be nonsense.

use super::{Row, RowKind, ScalarKind, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError {
    /// Byte offset where parsing gave up.
    pub offset: usize,
    pub message: &'static str,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for JsonError {}

struct Frame {
    open_ix: usize,
    is_object: bool,
    children: u32,
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    rows: Vec<Row>,
    stack: Vec<Frame>,
    /// The row that just finished, so a following `,` can be attributed to it. For a
    /// container that's its close row, not its open row.
    last_completed: Option<usize>,
}

pub fn flatten(src: &[u8]) -> Result<Vec<Row>, JsonError> {
    let mut parser = Parser {
        src,
        pos: 0,
        // Roughly one row per 24 bytes of JSON, clamped so a huge body doesn't reserve
        // wildly and a tiny one doesn't reallocate.
        rows: Vec::with_capacity((src.len() / 24).clamp(8, 1 << 20)),
        stack: Vec::new(),
        last_completed: None,
    };
    parser.run()?;
    Ok(parser.rows)
}

impl<'a> Parser<'a> {
    fn run(&mut self) -> Result<(), JsonError> {
        self.skip_ws();
        if self.pos == self.src.len() {
            return Err(self.error("the response body is empty"));
        }

        let mut pending_key = Span::NONE;
        let mut expect_value = true;

        loop {
            if expect_value {
                expect_value = self.read_value(&mut pending_key)?;
            } else {
                self.skip_ws();
                if self.stack.is_empty() {
                    break;
                }
                expect_value = self.read_separator(&mut pending_key)?;
            }
        }

        self.skip_ws();
        if self.pos != self.src.len() {
            return Err(self.error("unexpected trailing data after the JSON value"));
        }
        Ok(())
    }

    /// Read one value. Returns whether a value is expected next (true after opening a
    /// non-empty container, since its first member follows immediately).
    fn read_value(&mut self, pending_key: &mut Span) -> Result<bool, JsonError> {
        let depth = self.stack.len() as u16;
        let byte = self.peek().ok_or_else(|| self.error("unexpected end of input"))?;

        match byte {
            b'{' | b'[' => {
                let is_object = byte == b'{';
                self.pos += 1;

                // The container is itself a child of whatever encloses it.
                if let Some(frame) = self.stack.last_mut() {
                    frame.children += 1;
                }

                let kind = if is_object {
                    RowKind::ObjectOpen
                } else {
                    RowKind::ArrayOpen
                };
                let mut row = Row::empty(depth, kind);
                row.key = *pending_key;
                self.rows.push(row);
                *pending_key = Span::NONE;

                let open_ix = self.rows.len() - 1;
                self.stack.push(Frame {
                    open_ix,
                    is_object,
                    children: 0,
                });

                self.skip_ws();
                let closer = if is_object { b'}' } else { b']' };
                if self.peek() == Some(closer) {
                    self.pos += 1;
                    self.close_frame()?;
                    return Ok(false);
                }

                if is_object {
                    *pending_key = self.read_key()?;
                }
                Ok(true)
            }

            _ => {
                let (value, scalar) = self.read_scalar()?;
                let mut row = Row::empty(depth, RowKind::Scalar(scalar));
                row.key = *pending_key;
                row.value = value;
                self.rows.push(row);
                *pending_key = Span::NONE;

                if let Some(frame) = self.stack.last_mut() {
                    frame.children += 1;
                }
                self.last_completed = Some(self.rows.len() - 1);
                Ok(false)
            }
        }
    }

    /// Read a `,` or a closing bracket. Returns whether a value is expected next.
    fn read_separator(&mut self, pending_key: &mut Span) -> Result<bool, JsonError> {
        let is_object = self.stack.last().expect("non-empty stack").is_object;
        let closer = if is_object { b'}' } else { b']' };

        match self.peek() {
            Some(b',') => {
                self.pos += 1;
                if let Some(ix) = self.last_completed {
                    self.rows[ix].trailing_comma = true;
                }
                self.skip_ws();

                // A trailing comma before the closer is invalid JSON; say so rather
                // than emitting a phantom row.
                if self.peek() == Some(closer) {
                    return Err(self.error("trailing comma before the closing bracket"));
                }

                if is_object {
                    *pending_key = self.read_key()?;
                }
                Ok(true)
            }
            Some(byte) if byte == closer => {
                self.pos += 1;
                self.close_frame()?;
                Ok(false)
            }
            Some(_) => Err(self.error("expected ',' or a closing bracket")),
            None => Err(self.error("unexpected end of input inside a container")),
        }
    }

    fn close_frame(&mut self) -> Result<(), JsonError> {
        let frame = self.stack.pop().expect("close_frame with an empty stack");
        let depth = self.stack.len() as u16;

        let kind = if frame.is_object {
            RowKind::ObjectClose
        } else {
            RowKind::ArrayClose
        };
        self.rows.push(Row::empty(depth, kind));
        let close_ix = self.rows.len() - 1;

        let open = &mut self.rows[frame.open_ix];
        open.child_count = frame.children;
        // Rows to skip after the open to land past the close. Empty container => 1.
        open.subtree_len = (close_ix - frame.open_ix) as u32;

        // A comma after a container attaches to its close row, which is where it is
        // rendered.
        self.last_completed = Some(close_ix);
        Ok(())
    }

    fn read_key(&mut self) -> Result<Span, JsonError> {
        self.skip_ws();
        let key = self.read_string()?;
        self.skip_ws();
        match self.peek() {
            Some(b':') => self.pos += 1,
            Some(_) => return Err(self.error("expected ':' after an object key")),
            None => return Err(self.error("unexpected end of input")),
        }
        self.skip_ws();
        Ok(key)
    }

    fn read_scalar(&mut self) -> Result<(Span, ScalarKind), JsonError> {
        match self.peek() {
            Some(b'"') => Ok((self.read_string()?, ScalarKind::String)),
            Some(b't') => Ok((self.read_literal(b"true")?, ScalarKind::Bool)),
            Some(b'f') => Ok((self.read_literal(b"false")?, ScalarKind::Bool)),
            Some(b'n') => Ok((self.read_literal(b"null")?, ScalarKind::Null)),
            Some(b'-' | b'0'..=b'9') => Ok((self.read_number()?, ScalarKind::Number)),
            Some(_) => Err(self.error("expected a JSON value")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    /// Span includes both quotes, so rendering can style a key without re-quoting it.
    fn read_string(&mut self) -> Result<Span, JsonError> {
        let start = self.pos;
        match self.peek() {
            Some(b'"') => self.pos += 1,
            Some(_) => return Err(self.error("expected a string")),
            // Truncated bodies are common enough that this deserves its own message
            // rather than "expected a string" pointing at nothing.
            None => return Err(self.error("unexpected end of input")),
        }

        loop {
            match self.next() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => break,
                // Skip the escaped byte whatever it is; see the module note on being
                // permissive here.
                Some(b'\\') => {
                    if self.next().is_none() {
                        return Err(self.error("unterminated escape sequence"));
                    }
                }
                Some(_) => {}
            }
        }

        Ok(Span::new(start, self.pos - start))
    }

    fn read_number(&mut self) -> Result<Span, JsonError> {
        let start = self.pos;

        if self.peek() == Some(b'-') {
            self.pos += 1;
        }

        let digits_start = self.pos;
        self.skip_digits();
        if self.pos == digits_start {
            return Err(self.error("expected a digit"));
        }

        if self.peek() == Some(b'.') {
            self.pos += 1;
            let frac_start = self.pos;
            self.skip_digits();
            if self.pos == frac_start {
                return Err(self.error("expected a digit after the decimal point"));
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            self.skip_digits();
            if self.pos == exp_start {
                return Err(self.error("expected a digit in the exponent"));
            }
        }

        Ok(Span::new(start, self.pos - start))
    }

    fn read_literal(&mut self, word: &[u8]) -> Result<Span, JsonError> {
        let start = self.pos;
        if self.src[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(Span::new(start, word.len()))
        } else {
            Err(self.error("expected true, false, or null"))
        }
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn error(&self, message: &'static str) -> JsonError {
        JsonError {
            offset: self.pos,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(json: &str) -> Vec<RowKind> {
        flatten(json.as_bytes())
            .expect("valid json")
            .iter()
            .map(|row| row.kind)
            .collect()
    }

    fn err(json: &str) -> JsonError {
        flatten(json.as_bytes()).expect_err("should be rejected")
    }

    #[test]
    fn every_scalar_kind_is_recognised() {
        assert_eq!(kinds("null"), vec![RowKind::Scalar(ScalarKind::Null)]);
        assert_eq!(kinds("true"), vec![RowKind::Scalar(ScalarKind::Bool)]);
        assert_eq!(kinds("false"), vec![RowKind::Scalar(ScalarKind::Bool)]);
        assert_eq!(kinds("-12.5e+3"), vec![RowKind::Scalar(ScalarKind::Number)]);
        assert_eq!(kinds(r#""hi""#), vec![RowKind::Scalar(ScalarKind::String)]);
    }

    #[test]
    fn whitespace_between_every_token_is_tolerated() {
        let pretty = "{\n  \"a\" : [ 1 , 2 ] ,\n  \"b\" : { }\n}\n";
        assert_eq!(
            kinds(pretty),
            vec![
                RowKind::ObjectOpen,
                RowKind::ArrayOpen,
                RowKind::Scalar(ScalarKind::Number),
                RowKind::Scalar(ScalarKind::Number),
                RowKind::ArrayClose,
                RowKind::ObjectOpen,
                RowKind::ObjectClose,
                RowKind::ObjectClose,
            ]
        );
    }

    #[test]
    fn escaped_quotes_do_not_terminate_a_string() {
        let rows = flatten(br#"{"a":"say \"hi\" ok"}"#).expect("valid");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].kind, RowKind::Scalar(ScalarKind::String));
    }

    #[test]
    fn an_escaped_backslash_before_a_quote_still_terminates() {
        // "a\\" — the backslash is escaped, so the following quote closes the string.
        let rows = flatten(br#"["a\\"]"#).expect("valid");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        // A recursive parser dies here; the iterative one just allocates.
        let depth = 50_000;
        let mut json = String::with_capacity(depth * 2);
        json.push_str(&"[".repeat(depth));
        json.push_str(&"]".repeat(depth));

        let rows = flatten(json.as_bytes()).expect("deep but valid");
        assert_eq!(rows.len(), depth * 2);
        assert_eq!(rows[depth - 1].depth, (depth - 1) as u16);
    }

    #[test]
    fn structural_errors_are_rejected_with_an_offset() {
        assert_eq!(err("{").message, "unexpected end of input");
        assert_eq!(err("[1,").message, "unexpected end of input");
        assert_eq!(err("{\"a\" 1}").message, "expected ':' after an object key");
        assert_eq!(err("{a:1}").message, "expected a string");
        assert_eq!(err("[1 2]").message, "expected ',' or a closing bracket");
        assert_eq!(
            err("[1,]").message,
            "trailing comma before the closing bracket"
        );
        assert_eq!(err("").message, "the response body is empty");
    }

    #[test]
    fn trailing_data_is_rejected() {
        // Two documents concatenated is not one document.
        assert_eq!(
            err("{} {}").message,
            "unexpected trailing data after the JSON value"
        );
    }

    #[test]
    fn malformed_numbers_are_rejected() {
        assert_eq!(err("[1.]").message, "expected a digit after the decimal point");
        assert_eq!(err("[1e]").message, "expected a digit in the exponent");
        assert_eq!(err("[-]").message, "expected a digit");
    }

    #[test]
    fn error_offsets_point_into_the_input() {
        let error = err("{\"a\":1 \"b\":2}");
        assert!(
            error.offset > 5 && error.offset < 13,
            "offset {} should be at the missing comma",
            error.offset
        );
    }

    #[test]
    fn a_bare_literal_typo_is_rejected() {
        assert_eq!(err("[tru]").message, "expected true, false, or null");
    }
}
