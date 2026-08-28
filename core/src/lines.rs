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

    /// Map ascending byte offsets to the line each one falls in.
    ///
    /// A binary search per offset, unlike `JsonOutline::rows_for_offsets` which has to merge —
    /// the difference is that every line here *has* a recorded span, so the array is directly
    /// searchable. Offsets are capped at `search::MAX_MATCHES`, so `log n` per offset is
    /// nothing next to reconstructing positions.
    ///
    /// An offset inside a line terminator resolves to the line it ends, since `\r\n` is
    /// trimmed out of the span but belongs to the line before it.
    pub fn lines_for_offsets(&self, offsets: &[u32]) -> Vec<u32> {
        offsets
            .iter()
            .map(|offset| {
                let after = self
                    .lines
                    .partition_point(|line| line.start <= *offset);
                // `after` is 0 only for an offset before the first line, which cannot happen
                // — line 0 starts at 0 — but saturating beats an underflow if it ever does.
                after.saturating_sub(1) as u32
            })
            .collect()
    }

    /// Whether `offset` falls within the part of its line that is actually drawn.
    ///
    /// Minified JSON is one line that can be megabytes wide, and `line` cuts it at
    /// `MAX_DISPLAY_LINE`. A match past the cut is real, and scrolling to its row would show
    /// a line with no visible match in it — which reads as a broken search. The caller is
    /// expected to say so instead.
    pub fn offset_is_displayed(&self, offset: u32) -> bool {
        let line = self.lines_for_offsets(&[offset]);
        let Some(span) = line.first().and_then(|ix| self.lines.get(*ix as usize)) else {
            return false;
        };
        (offset.saturating_sub(span.start) as usize) < MAX_DISPLAY_LINE
    }

    /// The index of the widest line *as drawn*, for sizing a horizontal scroll region.
    ///
    /// Measured against `MAX_DISPLAY_LINE`, not the true length: a minified 10MB body is one
    /// line, and sizing the scroll region to 10MB of text would let you scroll ten megabytes
    /// into blank space. What is drawn is what you can scroll to.
    ///
    /// Ties go to the first, so the answer is stable across runs.
    pub fn widest_line(&self) -> usize {
        let mut widest = 0;
        let mut best = 0;

        for (ix, span) in self.lines.iter().enumerate() {
            let drawn = (span.len as usize).min(MAX_DISPLAY_LINE);
            if drawn > widest {
                widest = drawn;
                best = ix;
            }
        }

        best
    }

    /// A whole line, however long it is.
    ///
    /// The counterpart to `line`, and the distinction matters at exactly one call site:
    /// copying. What is *drawn* stops at `MAX_DISPLAY_LINE`, but a copy that silently handed
    /// back 4KB of a minified 10MB body would be a wrong answer wearing a right one — the
    /// same reason `CopyResponse` copies the raw bytes rather than the rendered outline.
    pub fn full_line(&self, ix: usize) -> Option<&str> {
        let span = self.lines.get(ix)?;
        let bytes = self.source.get(span.range())?;
        std::str::from_utf8(bytes).ok()
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
    fn the_widest_line_is_measured_as_drawn_not_as_stored() {
        // A minified body is one enormous line. Sizing a scroll region to its true length
        // would let you scroll megabytes past the last glyph, because the row stops at
        // MAX_DISPLAY_LINE — so a merely-long line must not beat a shorter one once both are
        // past the cut.
        let over = "x".repeat(MAX_DISPLAY_LINE * 3);
        let also_over = "y".repeat(MAX_DISPLAY_LINE + 1);
        let index = LineIndex::build(Bytes::from(format!("{also_over}\n{over}")));

        assert_eq!(
            index.widest_line(),
            0,
            "both are clipped to the same drawn width, so the first wins"
        );
    }

    #[test]
    fn the_widest_line_is_the_longest_one_below_the_cut() {
        let index = LineIndex::build(Bytes::from_static(b"a\nlonger line\nbb"));
        assert_eq!(index.widest_line(), 1);
    }

    #[test]
    fn an_empty_body_has_no_widest_line_to_speak_of() {
        assert_eq!(LineIndex::build(Bytes::from_static(b"")).widest_line(), 0);
    }

    #[test]
    fn a_full_line_is_not_truncated_the_way_the_drawn_one_is() {
        // The pair that matters: `line` is what fits on screen, `full_line` is what a copy
        // has to hand back. Asserting the two separately would let either drift into the
        // other's job without a failure.
        let long = "x".repeat(MAX_DISPLAY_LINE * 3);
        let index = LineIndex::build(Bytes::from(long.clone()));

        let (drawn, truncated) = index.line(0);
        assert!(truncated && drawn.len() <= MAX_DISPLAY_LINE);
        assert_eq!(index.full_line(0), Some(long.as_str()));
    }

    #[test]
    fn there_is_no_full_line_past_the_end() {
        let index = LineIndex::build(Bytes::from_static(b"a\nb"));
        assert_eq!(index.full_line(2), None);
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
    fn offsets_map_to_the_lines_they_fall_in() {
        let lines = index("alpha\nbeta\ngamma");
        let hits = crate::search::find(lines.source(), "beta");
        assert_eq!(lines.lines_for_offsets(&hits.offsets), vec![1]);

        // Every position across the body, including the newlines.
        let all: Vec<u32> = (0..16).collect();
        let mapped = lines.lines_for_offsets(&all);
        assert_eq!(mapped[0], 0, "offset 0 is line 0");
        assert_eq!(mapped[5], 0, "the \\n ending line 0 belongs to line 0");
        assert_eq!(mapped[6], 1, "line 1 starts right after it");
        assert_eq!(mapped[15], 2);
    }

    #[test]
    fn a_match_on_the_last_line_without_a_trailing_newline_maps_to_it() {
        let lines = index("a\nb\nlast");
        let hits = crate::search::find(lines.source(), "last");
        assert_eq!(lines.lines_for_offsets(&hits.offsets), vec![2]);
    }

    #[test]
    fn a_match_past_the_display_cut_is_reported_as_not_displayed() {
        // The trust bug this exists to prevent: minified JSON is one line megabytes wide, so
        // scrolling to a match past the 4KB cut would show a line with no visible match in it.
        let long = format!("{}needle", "x".repeat(MAX_DISPLAY_LINE * 2));
        let lines = LineIndex::build(Bytes::from(long));
        let hits = crate::search::find(lines.source(), "needle");

        assert_eq!(hits.len(), 1);
        assert_eq!(lines.lines_for_offsets(&hits.offsets), vec![0], "still line 0");
        assert!(
            !lines.offset_is_displayed(hits.offsets[0]),
            "a match beyond MAX_DISPLAY_LINE is not on screen and must say so"
        );
    }

    #[test]
    fn a_match_within_the_display_cut_is_reported_as_displayed() {
        let long = format!("needle{}", "x".repeat(MAX_DISPLAY_LINE * 2));
        let lines = LineIndex::build(Bytes::from(long));
        let hits = crate::search::find(lines.source(), "needle");
        assert!(lines.offset_is_displayed(hits.offsets[0]));
    }

    #[test]
    fn displayedness_is_measured_from_the_line_start_not_the_body_start() {
        // The bug a body-relative check would have: a match early on a *later* line sits past
        // MAX_DISPLAY_LINE in absolute terms while being perfectly visible.
        let body = format!("{}\nneedle", "x".repeat(MAX_DISPLAY_LINE * 2));
        let lines = LineIndex::build(Bytes::from(body));
        let hits = crate::search::find(lines.source(), "needle");

        assert_eq!(lines.lines_for_offsets(&hits.offsets), vec![1]);
        assert!(
            lines.offset_is_displayed(hits.offsets[0]),
            "column 0 of line 1 is on screen however long line 0 was"
        );
    }

    #[test]
    fn out_of_range_lines_are_empty_rather_than_a_panic() {
        let lines = index("a");
        assert_eq!(lines.line(99).0, "");
    }
}
