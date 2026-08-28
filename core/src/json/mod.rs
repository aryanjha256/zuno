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

    /// The path to a row, in JSONPath notation — `$.users[0].email`.
    ///
    /// Built top-down from `ancestors_of`, which yields the enclosing open rows outermost
    /// first, so consecutive pairs in the chain are parent and child. Each pair contributes
    /// one segment, and how it reads is decided by the *parent*: an object member is named by
    /// the child's key, an array element by counting the siblings before it.
    ///
    /// A close row has no value of its own, so it answers with the path of the container it
    /// closes — `}` is a row you can land on, and reporting no path for it would make the
    /// verb look broken on every third row of a nested document.
    ///
    /// **A bracket segment carries the key's source token verbatim, quotes and all.** That is
    /// already valid JSONPath, and it means a key containing a quote or a `\u` escape needs no
    /// decoding here — the one place this could produce a wrong path is the one place it does
    /// no work. Only a plain identifier takes the `.name` form.
    pub fn path_to(&self, row_ix: usize) -> Option<String> {
        let target = self.value_row(row_ix)?;

        let mut chain = self.ancestors_of(target);
        chain.push(target);

        let mut path = String::from("$");
        for pair in chain.windows(2) {
            let (parent, child) = (pair[0], pair[1]);
            match self.rows[parent].kind {
                RowKind::ObjectOpen => {
                    let token = self.text(self.rows[child].key);
                    match identifier(token) {
                        Some(name) => {
                            path.push('.');
                            path.push_str(name);
                        }
                        None => {
                            path.push('[');
                            path.push_str(token);
                            path.push(']');
                        }
                    }
                }
                RowKind::ArrayOpen => {
                    path.push('[');
                    path.push_str(&self.element_index(parent, child).to_string());
                    path.push(']');
                }
                // Only containers have children, so a scalar parent means the chain is not a
                // chain and any path built from it would be a guess.
                _ => return None,
            }
        }

        Some(path)
    }

    /// The source range a row's value occupies — the scalar token, or the whole container
    /// for an open or close row.
    ///
    /// Scalars are free, because the span is recorded. Containers are not: the tokenizer
    /// consumes `{`, `[`, `}` and `]` without noting where, for the same reason
    /// `rows_for_offsets` has to reconstruct positions. So this walks forward from the start
    /// of the document resolving each structural token against the source, and stops at the
    /// container's own close row — copying a small object near the top of a huge body costs
    /// a short walk, and only the root costs a full one.
    ///
    /// **Reconstructing the close brace is why the walk cannot stop at the last recorded
    /// span.** Close rows are zero-width points in `row_bounds`, so several nested `}` all
    /// inherit the same position and a scan forward from it finds the innermost one — which
    /// would copy a container missing its own closing brace, and the deeper the nesting the
    /// more it loses. Tracking a cursor *past* each brace as it is resolved is what keeps the
    /// depth straight.
    ///
    /// Scanning between tokens crosses whitespace, `:` and `,` and nothing else. Reaching any
    /// other byte means the reconstruction has lost the thread, so it gives up rather than
    /// returning a range that would copy the wrong text.
    pub fn value_span(&self, row_ix: usize) -> Option<Span> {
        let row = *self.rows.get(row_ix)?;
        if !row.value.is_none() {
            return Some(row.value);
        }

        let open_ix = self.value_row(row_ix)?;
        let open = self.rows.get(open_ix)?;
        if !open.kind.is_open() {
            return None;
        }
        let close_ix = open_ix + open.subtree_len as usize;
        if close_ix >= self.rows.len() {
            return None;
        }

        let mut cursor = 0u32;
        let mut opens_at = None;

        for (ix, row) in self.rows.iter().enumerate().take(close_ix + 1) {
            if row.kind.is_open() {
                // A key is recorded and precedes its brace, so start looking past it.
                let from = if row.key.is_none() {
                    cursor
                } else {
                    row.key.start + row.key.len
                };
                let at = self.scan_to(from, |byte| byte == b'{' || byte == b'[')?;
                if ix == open_ix {
                    opens_at = Some(at);
                }
                cursor = at + 1;
            } else if row.kind.is_close() {
                cursor = self.scan_to(cursor, |byte| byte == b'}' || byte == b']')? + 1;
            } else {
                cursor = row.value.start + row.value.len;
            }
        }

        let start = opens_at?;
        Some(Span::new(start as usize, (cursor - start) as usize))
    }

    /// The row whose value `row_ix` refers to: itself, or for a close row, its matching open.
    fn value_row(&self, row_ix: usize) -> Option<usize> {
        let row = self.rows.get(row_ix)?;
        if !row.kind.is_close() {
            return Some(row_ix);
        }
        self.rows[..row_ix]
            .iter()
            .enumerate()
            .rfind(|(ix, open)| open.kind.is_open() && ix + open.subtree_len as usize == row_ix)
            .map(|(ix, _)| ix)
    }

    /// How many siblings precede `child` inside the array opened at `parent`.
    ///
    /// Counts by hopping whole subtrees rather than rows, since a nested container is one
    /// element however many rows it spans.
    fn element_index(&self, parent: usize, child: usize) -> usize {
        let mut ix = parent + 1;
        let mut index = 0;

        while ix < child {
            let row = &self.rows[ix];
            ix += if row.kind.is_open() {
                1 + row.subtree_len as usize
            } else {
                1
            };
            index += 1;
        }

        index
    }

    /// Move forward to the next byte matching `wanted`, crossing only structural filler.
    fn scan_to(&self, from: u32, wanted: impl Fn(u8) -> bool) -> Option<u32> {
        let mut ix = from as usize;

        while let Some(&byte) = self.source.get(ix) {
            if wanted(byte) {
                return Some(ix as u32);
            }
            if !(byte.is_ascii_whitespace() || byte == b':' || byte == b',') {
                return None;
            }
            ix += 1;
        }

        None
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

/// The contents of a JSON string token, escapes decoded and quotes removed.
///
/// Anything that isn't a quoted token comes back unchanged, so a number, `true` or `null`
/// can be passed straight through without the caller matching on `ScalarKind` first.
///
/// **Decoding rather than merely unquoting is the whole point.** Stripping the quotes alone
/// is the tempting version and it is the worst of the three options available: it *looks*
/// decoded, so a value holding `\n` pastes as a backslash and an `n` into whatever fixture or
/// bug report it was copied for, and nothing on screen says otherwise. Copying the token
/// verbatim would at least be honest; this is honest and useful.
///
/// **Permissive, matching the tokenizer.** `flatten` deliberately validates structure and not
/// string contents (architecture.md §6), so an unknown escape or a malformed `\u` is passed
/// through exactly as written rather than swallowed or refused — an inspector that mangles a
/// response to display it is worse than one that shows it raw.
pub fn unquote(token: &str) -> String {
    let Some(inner) = token
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return token.to_string();
    };

    if !inner.contains('\\') {
        return inner.to_string();
    }

    // Indexing chars beats an iterator here because `\u` needs to look ahead for a surrogate
    // pair. One allocation on a copy action, never on a render path.
    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::with_capacity(inner.len());
    let mut ix = 0;

    while ix < chars.len() {
        if chars[ix] != '\\' {
            out.push(chars[ix]);
            ix += 1;
            continue;
        }

        let Some(&escape) = chars.get(ix + 1) else {
            out.push('\\');
            break;
        };
        ix += 2;

        match escape {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            // On failure `ix` already sits past the `\u`, so the hex digits fall through the
            // loop as ordinary characters and the escape comes out verbatim.
            'u' => match decode_escape(&chars, &mut ix) {
                Some(decoded) => out.push(decoded),
                None => out.push_str("\\u"),
            },
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }

    out
}

/// Decode the four hex digits of a `\u` escape, joining a surrogate pair if one follows.
///
/// `ix` points just past the `u` and is advanced only on success. A lone surrogate is not a
/// character, so it fails rather than being replaced — the caller then emits it verbatim,
/// which is the only lossless answer.
fn decode_escape(chars: &[char], ix: &mut usize) -> Option<char> {
    let high = hex_quad(chars, *ix)?;

    if (0xD800..0xDC00).contains(&high) {
        if chars.get(*ix + 4) != Some(&'\\') || chars.get(*ix + 5) != Some(&'u') {
            return None;
        }
        let low = hex_quad(chars, *ix + 6)?;
        if !(0xDC00..0xE000).contains(&low) {
            return None;
        }
        let joined = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
        let decoded = char::from_u32(joined)?;
        *ix += 10;
        return Some(decoded);
    }

    let decoded = char::from_u32(high)?;
    *ix += 4;
    Some(decoded)
}

fn hex_quad(chars: &[char], at: usize) -> Option<u32> {
    let mut value = 0u32;
    for offset in 0..4 {
        value = value * 16 + chars.get(at + offset)?.to_digit(16)?;
    }
    Some(value)
}

/// The unquoted key, when it is plain enough to write after a dot in a path.
///
/// Deliberately narrow — ASCII word characters not starting with a digit. A key that is
/// merely *probably* fine in dot form is not worth the risk: the bracket form is always
/// correct, so anything uncertain takes it.
fn identifier(token: &str) -> Option<&str> {
    let inner = token
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))?;

    let mut chars = inner.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return None;
    }

    Some(inner)
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

    // A shape with every path segment kind in it: an identifier key, a key that cannot be
    // written after a dot, an array of objects, and enough nesting for a close row to be
    // ambiguous about which container it ends.
    const NESTED: &str = r#"{"users":[{"email":"a@b.test","x-id":7},{"email":"c@d.test"}]}"#;

    #[test]
    fn a_path_names_an_object_key() {
        let outline = outline(NESTED);
        let row = outline
            .rows()
            .iter()
            .position(|row| outline.text(row.key) == "\"email\"")
            .expect("an email row");

        assert_eq!(outline.path_to(row).as_deref(), Some("$.users[0].email"));
    }

    #[test]
    fn a_path_counts_array_elements_by_subtree_not_by_row() {
        let outline = outline(NESTED);
        // The second element's email — six rows past the first, because the first element is a
        // whole object. Counting rows instead of subtrees would call this `[3]`.
        let row = outline
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| outline.text(row.key) == "\"email\"")
            .nth(1)
            .expect("a second email row")
            .0;

        assert_eq!(outline.path_to(row).as_deref(), Some("$.users[1].email"));
    }

    #[test]
    fn a_key_that_is_not_an_identifier_takes_the_bracket_form() {
        let outline = outline(NESTED);
        let row = outline
            .rows()
            .iter()
            .position(|row| outline.text(row.key) == "\"x-id\"")
            .expect("an x-id row");

        assert_eq!(
            outline.path_to(row).as_deref(),
            Some(r#"$.users[0]["x-id"]"#),
            "a hyphen cannot follow a dot, so the quoted token is used verbatim"
        );
    }

    #[test]
    fn the_root_has_a_path_and_so_does_a_close_row() {
        let outline = outline(NESTED);
        assert_eq!(outline.path_to(0).as_deref(), Some("$"));

        let last = outline.len() - 1;
        assert!(outline.rows()[last].kind.is_close());
        assert_eq!(
            outline.path_to(last).as_deref(),
            Some("$"),
            "a close row names the container it closes"
        );
    }

    #[test]
    fn a_close_row_deep_in_the_document_names_its_own_container() {
        let outline = outline(NESTED);
        // The `}` ending the first user, not the `]` ending the array or the outer `}`.
        let close = outline
            .rows()
            .iter()
            .position(|row| row.kind == RowKind::ObjectClose)
            .expect("a close row");

        assert_eq!(outline.path_to(close).as_deref(), Some("$.users[0]"));
    }

    #[test]
    fn a_path_for_a_row_that_does_not_exist_is_none() {
        assert_eq!(outline(NESTED).path_to(9_999), None);
    }

    #[test]
    fn a_scalars_value_span_is_the_token_itself() {
        let outline = outline(NESTED);
        let row = outline
            .rows()
            .iter()
            .position(|row| outline.text(row.key) == "\"x-id\"")
            .expect("an x-id row");

        let span = outline.value_span(row).expect("a span");
        assert_eq!(outline.text(span), "7");
    }

    #[test]
    fn a_containers_value_span_covers_its_own_braces() {
        let outline = outline(NESTED);
        let array = outline
            .rows()
            .iter()
            .position(|row| row.kind == RowKind::ArrayOpen)
            .expect("an array row");

        let span = outline.value_span(array).expect("a span");
        assert_eq!(
            outline.text(span),
            r#"[{"email":"a@b.test","x-id":7},{"email":"c@d.test"}]"#
        );
    }

    #[test]
    fn the_roots_value_span_is_the_whole_document_including_the_last_brace() {
        // The regression that motivates the cursor in `value_span`. Close rows are zero-width
        // points in `row_bounds`, so every nested `}` inherits the same position — scanning
        // forward from it finds the *innermost* one, and the root comes back one brace short
        // per level of nesting. Asserting on the root of a nested document is what makes that
        // visible; a flat object passes either way.
        let outline = outline(NESTED);
        let span = outline.value_span(0).expect("a span");
        assert_eq!(outline.text(span), NESTED);
    }

    #[test]
    fn a_close_rows_value_span_is_its_whole_container() {
        let outline = outline(NESTED);
        let close = outline
            .rows()
            .iter()
            .position(|row| row.kind == RowKind::ObjectClose)
            .expect("a close row");

        let span = outline.value_span(close).expect("a span");
        assert_eq!(outline.text(span), r#"{"email":"a@b.test","x-id":7}"#);
    }

    #[test]
    fn a_value_span_survives_whitespace_between_every_token() {
        // Pretty-printed input puts newlines and indentation where the minified fixture has
        // nothing, which is exactly what `scan_to` has to cross.
        let outline = outline("{\n  \"a\" : [\n    1 ,\n    2\n  ]\n}");
        let array = outline
            .rows()
            .iter()
            .position(|row| row.kind == RowKind::ArrayOpen)
            .expect("an array row");

        assert_eq!(
            outline.text(outline.value_span(array).expect("a span")),
            "[\n    1 ,\n    2\n  ]"
        );
    }

    #[test]
    fn unquote_decodes_escapes_rather_than_only_stripping_quotes() {
        assert_eq!(unquote(r#""plain""#), "plain");
        assert_eq!(unquote(r#""a\nb""#), "a\nb");
        assert_eq!(unquote(r#""say \"hi\"""#), "say \"hi\"");
        assert_eq!(unquote(r#""a\\b""#), r"a\b");
        assert_eq!(unquote(r#""http:\/\/x.test""#), "http://x.test");
        assert_eq!(unquote(r#""café""#), "café");
    }

    #[test]
    fn unquote_joins_a_surrogate_pair() {
        // Two escapes, one character. Decoding them separately yields two replacement
        // characters and silently corrupts any body carrying an emoji.
        assert_eq!(unquote(r#""🚀""#), "\u{1f680}");
    }

    #[test]
    fn unquote_passes_a_broken_escape_through_verbatim() {
        // Permissive like the tokenizer: an inspector that refuses or mangles a response in
        // order to show it is worse than one that shows it as written.
        assert_eq!(unquote(r#""\q""#), r"\q");
        assert_eq!(unquote(r#""\u00zz""#), r"\u00zz");
        assert_eq!(
            unquote(r#""\ud83d""#),
            r"\ud83d",
            "a lone surrogate is not a character"
        );
    }

    #[test]
    fn unquote_leaves_a_non_string_token_alone() {
        // So a caller can hand over any scalar without first matching on ScalarKind.
        assert_eq!(unquote("42"), "42");
        assert_eq!(unquote("true"), "true");
        assert_eq!(unquote("null"), "null");
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
