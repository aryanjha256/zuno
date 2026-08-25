//! Flattening JSON into an indexable list of rows.
//!
//! GPUI's `uniform_list` renders only the visible slice, but it demands one thing: an
//! **O(1)-indexable flat list of fixed-height rows** (architecture.md §6). A JSON tree
//! is not that, so the work here is turning a tree into a flat index, once, off the UI
//! thread.
//!
//! **Rows hold byte spans into the original `Bytes`, never copied strings.** For a 50MB
//! response that's the difference between ~50MB resident and several hundred, and it
//! eliminates millions of small allocations during the flatten pass.
//!
//! ### Correction to architecture.md §6
//!
//! The plan said "start with `serde_json::Value` for correctness, replace the parser
//! when it hurts". That was never actually viable: `Value` discards byte offsets, and
//! every `Span` here depends on them. A position-tracking parser wasn't a later
//! optimisation, it was the only way to build this at all — so `flatten.rs` is a small
//! hand-written tokenizer instead.

mod flatten;

use bytes::Bytes;

pub use flatten::JsonError;

/// A byte range into the source buffer.
///
/// `u32` because offsets are capped at 4GB — far past the point where any of this is a
/// good idea — and halving the size of `Row` matters when there are a million of them.
/// `len == 0` is the "absent" sentinel, which is safe because every real token includes
/// at least one byte (strings include their quotes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub len: u32,
}

impl Span {
    pub const NONE: Span = Span { start: 0, len: 0 };

    pub fn new(start: usize, len: usize) -> Self {
        Self {
            start: start as u32,
            len: len as u32,
        }
    }

    pub fn is_none(&self) -> bool {
        self.len == 0
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.start as usize..(self.start as usize + self.len as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Null,
    Bool,
    Number,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A leaf — `"key": 42`, or just `42` inside an array.
    Scalar(ScalarKind),
    ObjectOpen,
    ObjectClose,
    ArrayOpen,
    ArrayClose,
}

impl RowKind {
    pub fn is_open(&self) -> bool {
        matches!(self, RowKind::ObjectOpen | RowKind::ArrayOpen)
    }

    pub fn is_close(&self) -> bool {
        matches!(self, RowKind::ObjectClose | RowKind::ArrayClose)
    }
}

/// One rendered line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub depth: u16,
    pub kind: RowKind,
    /// The object key, quotes included. `NONE` for array elements and close rows.
    pub key: Span,
    /// The scalar token. `NONE` for open and close rows.
    pub value: Span,
    /// Open rows only: how many rows to skip to land past the matching close.
    /// An empty container has `subtree_len == 1` (just the close row).
    pub subtree_len: u32,
    /// Open rows only: number of direct children, for the folded summary.
    pub child_count: u32,
    /// Whether a `,` follows this row in the source.
    pub trailing_comma: bool,
}

impl Row {
    fn empty(depth: u16, kind: RowKind) -> Self {
        Self {
            depth,
            kind,
            key: Span::NONE,
            value: Span::NONE,
            subtree_len: 0,
            child_count: 0,
            trailing_comma: false,
        }
    }
}

/// A parsed, immutable view of a JSON document.
///
/// Fold state deliberately lives elsewhere: keeping this immutable means it can sit
/// behind an `Arc` and be shared into a render closure without locking.
#[derive(Debug)]
pub struct JsonOutline {
    source: Bytes,
    rows: Vec<Row>,
}

impl JsonOutline {
    /// Parse and flatten. Call this on a background executor — it is O(n) over the
    /// whole body and has no business on the UI thread.
    pub fn parse(source: Bytes) -> Result<Self, JsonError> {
        if source.len() > u32::MAX as usize {
            return Err(JsonError {
                offset: 0,
                message: "response is too large to index",
            });
        }

        // Validate UTF-8 once here so `text()` can be infallible and allocation-free on
        // the render path. SIMD-accelerated, so a few ms even on 10MB.
        if std::str::from_utf8(&source).is_err() {
            return Err(JsonError {
                offset: 0,
                message: "response is not valid UTF-8",
            });
        }

        let rows = flatten::flatten(&source)?;
        Ok(Self { source, rows })
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row(&self, ix: usize) -> Option<&Row> {
        self.rows.get(ix)
    }

    pub fn source(&self) -> &Bytes {
        &self.source
    }

    /// The text for a span. Never allocates — this runs once per visible row per frame.
    pub fn text(&self, span: Span) -> &str {
        if span.is_none() {
            return "";
        }
        self.source
            .get(span.range())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }

    /// Map ascending byte offsets to the row each one falls in.
    ///
    /// A **merge** rather than a binary search per offset, and not by choice: a row's source
    /// position isn't stored. `Row` holds spans for the key and the scalar value, but an open
    /// row inside an array and *every* close row have neither — the `{`, `[`, `}` and `]`
    /// positions are consumed by the tokenizer and never recorded. Adding a `start` field
    /// would grow the struct by 4 bytes, which is 5MB on the 1.31M rows a 10MB body produces,
    /// to serve one caller. So the start is reconstructed here in a single forward walk, where
    /// a spanless row inherits the position of the row before it. Both sequences are sorted,
    /// so this is O(rows + offsets) with no allocation beyond the result.
    ///
    /// A spanless row inherits where the previous row *ended*, not where it began — which is
    /// the whole correctness of this. Inheriting the start instead makes every trailing close
    /// row share the last scalar's position, so a match in that scalar accepts all of them and
    /// resolves to the outermost `}`. That was the first version, and it was wrong by exactly
    /// the nesting depth.
    ///
    /// With ends, the result is precise even for braces: a `}` resolves to its own close row,
    /// and a `{` opening an array element resolves to that open row. The only imprecision left
    /// is structural whitespace, which lands on the row whose content most recently ended.
    pub fn rows_for_offsets(&self, offsets: &[u32]) -> Vec<u32> {
        let mut rows = Vec::with_capacity(offsets.len());
        if self.rows.is_empty() {
            return rows;
        }

        // The last row known to begin at or before the offset in hand.
        let mut best = 0u32;
        let mut scan = 0usize;
        // End of the most recently *computed* row. The inheritance chain follows row order, so
        // this advances even for a row that turns out to be too far along for this offset.
        let mut computed_end = 0u32;
        let mut pending: Option<(usize, u32)> = None;

        for &offset in offsets {
            loop {
                let (ix, start) = match pending.take() {
                    Some(peeked) => peeked,
                    None => {
                        if scan >= self.rows.len() {
                            break;
                        }
                        let (start, end) = row_bounds(&self.rows[scan], computed_end);
                        computed_end = end;
                        let peeked = (scan, start);
                        scan += 1;
                        peeked
                    }
                };

                if start <= offset {
                    best = ix as u32;
                } else {
                    // Too far. Hold it for the next, larger offset rather than recomputing.
                    pending = Some((ix, start));
                    break;
                }
            }
            rows.push(best);
        }

        rows
    }

    /// Every folded row that has to open for `row_ix` to be visible.
    ///
    /// Walks forward rather than up a parent chain, because `Row` records no parent: an open
    /// row is an ancestor exactly when its subtree spans `row_ix`. O(row_ix), paid once per
    /// jump rather than per frame.
    pub fn ancestors_of(&self, row_ix: usize) -> Vec<usize> {
        let mut ancestors = Vec::new();
        for (ix, row) in self.rows.iter().enumerate().take(row_ix) {
            if row.kind.is_open() && ix + row.subtree_len as usize >= row_ix {
                ancestors.push(ix);
            }
        }
        ancestors
    }

    /// Build the visible-row index for a set of folded rows.
    ///
    /// O(rows), and the only thing that changes when you fold — the rows themselves are
    /// never rewritten.
    pub fn visible_rows(&self, folded: &[bool]) -> Vec<u32> {
        let mut visible = Vec::with_capacity(self.rows.len());
        let mut ix = 0usize;

        while ix < self.rows.len() {
            visible.push(ix as u32);
            let row = &self.rows[ix];

            if row.kind.is_open() && folded.get(ix).copied().unwrap_or(false) {
                // Skip the whole subtree, including the matching close row.
                ix += 1 + row.subtree_len as usize;
            } else {
                ix += 1;
            }
        }

        visible
    }
}

/// The source range a row covers, as `(start, end)`.
///
/// A key comes before its value, so the pair spans from the key's first byte to the value's
/// last. `inherited` — the previous row's end — covers rows carrying no span at all: close
/// rows, and open rows that aren't object members.
fn row_bounds(row: &Row, inherited: u32) -> (u32, u32) {
    let key = (!row.key.is_none()).then(|| (row.key.start, row.key.start + row.key.len));
    let value = (!row.value.is_none()).then(|| (row.value.start, row.value.start + row.value.len));

    match (key, value) {
        (Some((start, _)), Some((_, end))) => (start, end),
        (Some((start, end)), None) | (None, Some((start, end))) => (start, end),
        // Nothing recorded, so the row is treated as a zero-width point just past the
        // previous one — enough to order it without claiming a position it doesn't have.
        (None, None) => (inherited, inherited),
    }
}

/// Translate a byte offset to 1-based line and column, for error messages.
pub fn line_col(source: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let mut line = 1;
    let mut last_newline = 0;

    for (ix, byte) in source[..offset].iter().enumerate() {
        if *byte == b'\n' {
            line += 1;
            last_newline = ix + 1;
        }
    }

    (line, offset - last_newline + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(json: &'static str) -> JsonOutline {
        JsonOutline::parse(Bytes::from_static(json.as_bytes())).expect("valid json")
    }

    #[test]
    fn a_scalar_document_is_one_row() {
        let outline = outline("42");
        assert_eq!(outline.len(), 1);
        assert_eq!(outline.rows()[0].kind, RowKind::Scalar(ScalarKind::Number));
        assert_eq!(outline.text(outline.rows()[0].value), "42");
    }

    #[test]
    fn an_object_flattens_to_open_members_close() {
        let outline = outline(r#"{"a":1,"b":"two"}"#);
        let kinds: Vec<_> = outline.rows().iter().map(|row| row.kind).collect();

        assert_eq!(
            kinds,
            vec![
                RowKind::ObjectOpen,
                RowKind::Scalar(ScalarKind::Number),
                RowKind::Scalar(ScalarKind::String),
                RowKind::ObjectClose,
            ]
        );

        // Keys keep their quotes, so rendering can style them without re-quoting.
        assert_eq!(outline.text(outline.rows()[1].key), "\"a\"");
        assert_eq!(outline.text(outline.rows()[1].value), "1");
        assert_eq!(outline.text(outline.rows()[2].value), "\"two\"");
    }

    #[test]
    fn depth_tracks_nesting_and_closes_match_their_opens() {
        let outline = outline(r#"{"a":{"b":[1]}}"#);
        let depths: Vec<_> = outline.rows().iter().map(|row| row.depth).collect();
        //  {        "a":{     "b":[     1         ]         }         }
        assert_eq!(depths, vec![0, 1, 2, 3, 2, 1, 0]);
    }

    #[test]
    fn subtree_len_skips_exactly_past_the_close() {
        let outline = outline(r#"{"a":{"b":1,"c":2},"d":3}"#);
        let rows = outline.rows();

        // Row 1 is the nested object's open.
        assert_eq!(rows[1].kind, RowKind::ObjectOpen);
        let after = 1 + 1 + rows[1].subtree_len as usize;
        assert_eq!(
            rows[after].kind,
            RowKind::Scalar(ScalarKind::Number),
            "skipping a folded subtree should land on \"d\""
        );
        assert_eq!(outline.text(rows[after].key), "\"d\"");
    }

    #[test]
    fn an_empty_container_still_has_a_close_row() {
        let outline = outline(r#"{"a":{},"b":[]}"#);
        let kinds: Vec<_> = outline.rows().iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::ObjectOpen,
                RowKind::ObjectOpen,
                RowKind::ObjectClose,
                RowKind::ArrayOpen,
                RowKind::ArrayClose,
                RowKind::ObjectClose,
            ]
        );
        assert_eq!(outline.rows()[1].subtree_len, 1);
        assert_eq!(outline.rows()[1].child_count, 0);
    }

    #[test]
    fn child_count_counts_direct_children_only() {
        let outline = outline(r#"{"a":1,"b":{"c":2,"d":3},"e":4}"#);
        assert_eq!(outline.rows()[0].child_count, 3, "a, b, e");
        assert_eq!(outline.rows()[2].child_count, 2, "c, d");
    }

    #[test]
    fn trailing_commas_are_recorded_for_rendering() {
        let outline = outline(r#"{"a":1,"b":2}"#);
        let rows = outline.rows();
        assert!(rows[1].trailing_comma, "\"a\":1 is followed by a comma");
        assert!(!rows[2].trailing_comma, "\"b\":2 is last");
    }

    #[test]
    fn a_closing_brace_carries_the_comma_when_a_sibling_follows() {
        let outline = outline(r#"{"a":{"x":1},"b":2}"#);
        let rows = outline.rows();
        let close_ix = rows
            .iter()
            .position(|row| row.kind == RowKind::ObjectClose)
            .unwrap();
        assert!(
            rows[close_ix].trailing_comma,
            "the inner }} is followed by a comma"
        );
    }

    #[test]
    fn folding_hides_a_subtree_but_keeps_its_open_row() {
        let outline = outline(r#"{"a":{"b":1,"c":2},"d":3}"#);
        let mut folded = vec![false; outline.len()];
        folded[1] = true; // fold the nested object

        let visible = outline.visible_rows(&folded);
        let kinds: Vec<_> = visible
            .iter()
            .map(|ix| outline.rows()[*ix as usize].kind)
            .collect();

        assert_eq!(
            kinds,
            vec![
                RowKind::ObjectOpen,               // {
                RowKind::ObjectOpen,               //   "a": { … }
                RowKind::Scalar(ScalarKind::Number), //   "d": 3
                RowKind::ObjectClose,              // }
            ]
        );
    }

    #[test]
    fn folding_the_root_leaves_a_single_row() {
        let outline = outline(r#"{"a":1,"b":2}"#);
        let mut folded = vec![false; outline.len()];
        folded[0] = true;
        assert_eq!(outline.visible_rows(&folded).len(), 1);
    }

    #[test]
    fn nested_folds_compose() {
        let outline = outline(r#"[{"a":[1,2]},{"b":3}]"#);
        let mut folded = vec![false; outline.len()];
        folded[0] = true; // fold the outer array
        assert_eq!(outline.visible_rows(&folded).len(), 1);
    }

    #[test]
    fn unfolded_visible_rows_are_every_row() {
        let outline = outline(r#"{"a":[1,{"b":2}],"c":null}"#);
        let folded = vec![false; outline.len()];
        assert_eq!(outline.visible_rows(&folded).len(), outline.len());
    }

    #[test]
    fn non_utf8_is_rejected_with_a_clear_message() {
        let error = JsonOutline::parse(Bytes::from_static(&[b'"', 0xff, b'"'])).unwrap_err();
        assert_eq!(error.message, "response is not valid UTF-8");
    }

    #[test]
    fn line_col_points_at_the_offending_byte() {
        let source = b"{\n  \"a\": ?\n}";
        let offset = source.iter().position(|b| *b == b'?').unwrap();
        assert_eq!(line_col(source, offset), (2, 8));
    }

    /// The row a needle's first occurrence lands in — the whole point of the mapping.
    fn row_of(json: &'static str, needle: &str) -> u32 {
        let outline = outline(json);
        let hits = crate::search::find(outline.source(), needle);
        assert!(!hits.is_empty(), "{needle:?} should be present in the fixture");
        outline.rows_for_offsets(&hits.offsets)[0]
    }

    #[test]
    fn a_match_in_a_key_maps_to_that_key_s_row() {
        let json = r#"{"alpha":1,"beta":2,"gamma":3}"#;
        //             row 0      row 1     row 2     row 3
        assert_eq!(row_of(json, "beta"), 2);
        assert_eq!(row_of(json, "gamma"), 3);
    }

    #[test]
    fn a_match_in_a_value_maps_to_its_row_not_the_next_one() {
        // The off-by-one that matters: a value sits *after* its key, so a greedy walk that
        // advanced past the row would report the following key's row instead.
        let json = r#"{"a":"needle","b":"other"}"#;
        assert_eq!(row_of(json, "needle"), 1);
        assert_eq!(row_of(json, "other"), 2);
    }

    #[test]
    fn a_match_deep_inside_nesting_maps_to_the_innermost_row() {
        let json = r#"{"a":{"b":{"c":"found"}}}"#;
        // rows: 0 {  1 "a":{  2 "b":{  3 "c":"found"  4 }  5 }  6 }
        assert_eq!(row_of(json, "found"), 3);
    }

    #[test]
    fn matches_inside_an_array_map_to_the_element_rows() {
        let json = r#"["zero","one","two"]"#;
        assert_eq!(row_of(json, "zero"), 1);
        assert_eq!(row_of(json, "one"), 2);
        assert_eq!(row_of(json, "two"), 3);
    }

    #[test]
    fn every_offset_gets_a_row_and_the_rows_never_go_backwards() {
        // The merge holds one peeked row between offsets; getting that wrong shows up as a
        // row index that regresses, or as a short result.
        let json = r#"{"a":"x","b":{"c":"x","d":["x","x"]},"e":"x"}"#;
        let outline = outline(json);
        let hits = crate::search::find(outline.source(), "x");
        let rows = outline.rows_for_offsets(&hits.offsets);

        assert_eq!(rows.len(), hits.len(), "one row per match, always");
        assert_eq!(hits.len(), 5, "the fixture has five values of \"x\"");
        assert!(
            rows.windows(2).all(|pair| pair[0] <= pair[1]),
            "ascending offsets must yield non-decreasing rows: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| (*row as usize) < outline.len()),
            "no row index may run past the outline: {rows:?}"
        );
    }

    #[test]
    fn an_offset_on_a_closing_brace_resolves_to_its_own_close_row() {
        // Free precision from inheriting the previous row's *end*: the close row's zero-width
        // point sits exactly at the `}`, so it wins. Inheriting the start instead put every
        // trailing close row on top of the last scalar, which is the bug this pins.
        let outline = outline(r#"{"a":1}"#);
        let close = outline.source().len() as u32 - 1;
        assert_eq!(outline.rows_for_offsets(&[close]), vec![2], "the closing-brace row");
    }

    #[test]
    fn nesting_depth_does_not_drag_a_match_out_to_the_outermost_brace() {
        // The exact shape of the first version's bug: N trailing close rows all inherited the
        // innermost scalar's start, so a match in it resolved to the last row in the document.
        // The deeper the nesting, the further off — so assert on depth, not just one case.
        let outline = outline(r#"{"a":{"b":{"c":{"d":"deep"}}}}"#);
        let row = row_of(r#"{"a":{"b":{"c":{"d":"deep"}}}}"#, "deep");

        assert_eq!(outline.text(outline.rows()[row as usize].key), "\"d\"");
        assert!(
            (row as usize) < outline.len() - 1,
            "the match must not land on a close row: {row} of {}",
            outline.len()
        );
    }

    #[test]
    fn no_offsets_means_no_rows() {
        assert!(outline(r#"{"a":1}"#).rows_for_offsets(&[]).is_empty());
    }

    #[test]
    fn ancestors_are_the_containers_that_have_to_open() {
        let outline = outline(r#"{"a":{"b":[1]}}"#);
        // rows: 0 {   1 "a":{   2 "b":[   3 1   4 ]   5 }   6 }
        assert_eq!(outline.ancestors_of(3), vec![0, 1, 2]);
        assert_eq!(outline.ancestors_of(1), vec![0]);
        assert!(
            outline.ancestors_of(0).is_empty(),
            "the root has no ancestor to open"
        );
    }

    #[test]
    fn a_sibling_container_is_not_an_ancestor() {
        // The discriminating case for the `subtree_len` bound: without it, every earlier open
        // row would count and folding a sibling would look like it hides the target.
        let outline = outline(r#"{"a":{"x":1},"b":{"y":2}}"#);
        // rows: 0 {  1 "a":{  2 "x":1  3 }  4 "b":{  5 "y":2  6 }  7 }
        assert_eq!(
            outline.ancestors_of(5),
            vec![0, 4],
            "\"a\" closes before row 5 and must not appear"
        );
    }

    #[test]
    fn revealing_a_row_needs_exactly_its_ancestors_unfolded() {
        // Ties the two halves together: fold everything, and unfolding just the ancestors of
        // a target is enough to make it visible.
        let outline = outline(r#"{"a":{"b":{"c":1}},"d":2}"#);
        let target = outline
            .rows()
            .iter()
            .position(|row| outline.text(row.key) == "\"c\"")
            .expect("the c row");

        let mut folded = vec![false; outline.len()];
        for (ix, row) in outline.rows().iter().enumerate() {
            folded[ix] = row.kind.is_open();
        }
        assert!(
            !outline.visible_rows(&folded).contains(&(target as u32)),
            "fully folded, the target must be hidden"
        );

        for ancestor in outline.ancestors_of(target) {
            folded[ancestor] = false;
        }
        assert!(
            outline.visible_rows(&folded).contains(&(target as u32)),
            "unfolding the ancestors must reveal it"
        );
    }

    #[test]
    fn spans_never_copy_the_source() {
        // The whole point of Span: text() borrows, it does not allocate.
        let outline = outline(r#"{"key":"value"}"#);
        let text = outline.text(outline.rows()[1].value);
        assert_eq!(text, "\"value\"");
        assert!(
            outline.source().as_ptr() <= text.as_ptr(),
            "text should point into the source buffer"
        );
    }
}
