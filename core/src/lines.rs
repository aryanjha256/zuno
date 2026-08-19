//! A line index for the raw-text fallback view.
//!
//! Needed for the same reason `JsonOutline` is: `uniform_list` wants an indexable list
//! of rows, and splitting 10MB into a `Vec<String>` on the UI thread would block for as
//! long as it takes to allocate a million strings. Here each line is a byte span into
//! the original buffer.
//!
//! This is what a non-JSON body, an over-the-cap body, and a body that failed to parse
//! all fall back to — so it has to stay cheap on inputs that are hostile to line-based
//! display, notably minified JSON that is one 10MB line.

use bytes::Bytes;

use crate::json::Span;

/// Longest line handed to the renderer.
///
/// Minified JSON is a single line that can be megabytes wide. Shaping that as one text
/// run stalls the frame no matter how good the virtualisation is, so long lines are
/// truncated for *display* — the full bytes are still in `source`, and the caller is
/// expected to say so in the UI rather than pretend the line ended.
pub const MAX_DISPLAY_LINE: usize = 4096;

#[derive(Debug)]
pub struct LineIndex {
    source: Bytes,
    lines: Vec<Span>,
}

impl LineIndex {
    /// Scan for line breaks. O(n) with no allocation per line.
    pub fn build(source: Bytes) -> Self {
        // Cap at u32 so spans stay narrow; anything larger is not going to be displayed
        // line-by-line anyway.
        let limit = source.len().min(u32::MAX as usize);

        let mut lines = Vec::new();
        let mut start = 0usize;

        for ix in 0..limit {
            if source[ix] == b'\n' {
                // Strip a preceding \r so CRLF bodies don't render a stray glyph.
                let mut end = ix;
                if end > start && source[end - 1] == b'\r' {
                    end -= 1;
                }
                lines.push(Span::new(start, end - start));
                start = ix + 1;
            }
        }

        // Trailing content with no final newline is still a line. A body ending in \n
        // does *not* get a phantom empty line after it.
        if start < limit {
            lines.push(Span::new(start, limit - start));
        }

        Self { source, lines }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn source(&self) -> &Bytes {
        &self.source
    }

    /// The text of a line, truncated to `MAX_DISPLAY_LINE` bytes on a UTF-8 boundary.
    ///
    /// Returns the text and whether it was cut short, so the caller can mark it instead
    /// of silently showing a partial line.
    pub fn line(&self, ix: usize) -> (&str, bool) {
        let Some(span) = self.lines.get(ix) else {
            return ("", false);
        };

        let bytes = match self.source.get(span.range()) {
            Some(bytes) => bytes,
            None => return ("", false),
        };

        if bytes.len() <= MAX_DISPLAY_LINE {
            return (std::str::from_utf8(bytes).unwrap_or(""), false);
        }

        // Back off to a char boundary so from_utf8 succeeds.
        let mut end = MAX_DISPLAY_LINE;
        while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
        (std::str::from_utf8(&bytes[..end]).unwrap_or(""), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(text: &'static str) -> LineIndex {
        LineIndex::build(Bytes::from_static(text.as_bytes()))
    }

    #[test]
    fn splits_on_newlines() {
        let lines = index("a\nb\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.line(0).0, "a");
        assert_eq!(lines.line(2).0, "c");
    }

    #[test]
    fn a_trailing_newline_does_not_add_a_phantom_line() {
        assert_eq!(index("a\nb\n").len(), 2);
    }

    #[test]
    fn crlf_endings_do_not_leave_a_stray_carriage_return() {
        let lines = index("a\r\nb\r\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.line(0).0, "a");
        assert_eq!(lines.line(1).0, "b");
    }

    #[test]
    fn blank_lines_are_preserved() {
        let lines = index("a\n\nb");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.line(1).0, "");
    }

    #[test]
    fn an_empty_body_has_no_lines() {
        assert!(index("").is_empty());
    }

    #[test]
    fn a_single_line_body_is_one_line() {
        // The minified-JSON shape.
        let lines = index("{\"a\":1}");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn over_long_lines_are_truncated_and_say_so() {
        let long = "x".repeat(MAX_DISPLAY_LINE * 3);
        let lines = LineIndex::build(Bytes::from(long));
        let (text, truncated) = lines.line(0);

        assert!(truncated, "an over-long line should report truncation");
        assert!(text.len() <= MAX_DISPLAY_LINE);
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        // Multi-byte chars straddling the cut must not produce invalid UTF-8.
        let long = "é".repeat(MAX_DISPLAY_LINE);
        let lines = LineIndex::build(Bytes::from(long));
        let (text, truncated) = lines.line(0);

        assert!(truncated);
        assert!(!text.is_empty(), "backing off must not empty the line");
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn out_of_range_lines_are_empty_rather_than_a_panic() {
        let lines = index("a");
        assert_eq!(lines.line(99).0, "");
    }
}
