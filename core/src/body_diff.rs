//! The inline body diff: *what* changed, where `ResponseDiff` answers only *whether* anything did.
//!
//! **Both sides are normalized before comparing, and that is the whole reason this is useful.**
//! A JSON API overwhelmingly answers on one line — `{"id":1,"name":"ada"}` — so a line diff over
//! the raw bytes has exactly one line to report and concludes "it changed", which is precisely
//! what the summary diff already said. Running both sides through `json::pretty` first puts one
//! field per line, so the diff lands on the field that moved. That reuses the formatter the
//! viewer already trusts, which copies tokens from their spans rather than re-serializing, so
//! key order and number formatting survive and two responses cannot differ here because of how
//! they were *printed*.
//!
//! **Pretty-printing is all-or-nothing across the pair.** Formatting one side and not the other
//! makes every line differ. An endpoint that starts returning an HTML error instead of JSON is a
//! total rewrite either way, but reformatting the JSON side buries the one thing worth seeing
//! under a wall of re-indented lines it would have to be scrolled past.
//!
//! **Patience, not Myers.** Myers is `similar`'s default and it is the wrong default *here*:
//! pretty-printed JSON is full of interchangeable `},` and `],` lines, and Myers is happy to
//! pair a closing brace in one document with an unrelated one in the other to shave the edit
//! script. The result is a diff whose hunks straddle object boundaries. Patience only anchors on
//! lines that are *unique to both sides*, which for JSON means the keys, so hunks land on the
//! object that actually changed. It is the same reason `git diff --patience` exists.
//!
//! **A flat line list, not nested hunks.** The renderer is a `uniform_list`, which addresses
//! items by a single index, so hunks are flattened and the gaps between them become a
//! `Skipped` line carrying the count. That keeps every list mechanic — scrolling, sizing,
//! selection — indistinguishable from the body viewer's.

use std::ops::Range;
use std::time::Duration;

use bytes::Bytes;
use similar::{Algorithm, ChangeTag, DiffTag, InlineChangeMode, InlineChangeOptions, TextDiff};

use crate::json;
use crate::response::ResponseData;

/// Past this, neither side is diffed at all.
///
/// A *memory and usefulness* limit rather than a speed one, the same argument as the viewer's
/// `MAX_AUTO_PARSE`: pretty-printing 4MB of JSON produces two multi-megabyte strings plus a
/// line table over each, and a diff nobody can read is not worth the allocation.
pub const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

/// Emitted lines past which the result is truncated.
///
/// The bound that matters is not the body size but how much of it *differs*: two documents
/// sharing nothing produce one hunk containing every line of both, so the size cap above does
/// not constrain this case on its own.
pub const MAX_DIFF_LINES: usize = 5_000;

/// Unchanged lines kept either side of a hunk.
const CONTEXT: usize = 3;

/// Myers and Patience are both worst-case quadratic. `similar` checks this deadline inside the
/// algorithm and degrades to a coarser — still correct — edit script rather than returning an
/// error, so the only cost of hitting it is a less tidy diff.
const BUDGET: Duration = Duration::from_millis(750);

/// Longest line that gets character-level refinement.
///
/// Refinement is a second diff *within* one line, so a minified HTML body — one line, half a
/// megabyte — would run Patience over half a million character tokens to colour a line nobody
/// can read across anyway. Above this the line is still shown, just marked changed as a whole.
const MAX_REFINE_BYTES: usize = 2_000;

/// Refine by **characters**, not words, and this is not a preference.
///
/// `InlineChangeMode::Auto` resolves to whitespace-separated words without the `unicode`
/// feature — and JSON has almost no whitespace, so `"https://example.com/v1/users"` is a
/// *single token*. Bumping a path segment then marks the entire URL as changed, which is
/// exactly the coarse answer refinement exists to improve on. Measured, not assumed: the
/// test `a_changed_line_marks_only_the_part_that_moved` fails that way against `Auto`.
///
/// Character tokens fragment, so `semantic_cleanup` shifts the boundaries back out to
/// something a person would have drawn. The `unicode` feature would buy grapheme-aware
/// tokens and a `unicode-segmentation` dependency; the point of choosing this crate was that
/// it brings none, and a diff highlight landing inside a grapheme cluster is a rendering
/// concern the row can absorb.
fn refinement() -> InlineChangeOptions {
    let mut options = InlineChangeOptions::new();
    options
        .mode(InlineChangeMode::Chars)
        .semantic_cleanup(true);
    options
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A run of identical lines between two hunks, not shown. Carries how many.
    Skipped(usize),
    Equal,
    Insert,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line number in the previous response, where the line exists there.
    pub old: Option<u32>,
    /// 1-based line number in the current response, where the line exists there.
    pub new: Option<u32>,
    /// The line, newline stripped.
    pub text: String,
    /// Byte ranges within `text` that actually differ from the counterpart line.
    ///
    /// Empty means "no finer detail than the line itself" — which is the honest answer in three
    /// different situations, all of which should render the same way: an `Equal` line, a line
    /// with no counterpart to compare against, and a line too long to refine.
    pub changed: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyDiff {
    /// Byte-identical bodies. Distinct from an empty `Changed` so the pane can say so rather
    /// than drawing an empty list, which reads as a failure.
    Identical,
    /// At least one side is not valid UTF-8, so it has no lines to compare.
    NotText,
    /// At least one side is past `MAX_DIFF_BYTES`. Carries the larger length, to report it.
    TooLarge { len: usize },
    Changed {
        lines: Vec<DiffLine>,
        /// Whether `MAX_DIFF_LINES` cut the result short. Surfaced rather than silently
        /// dropped: a diff that stops early while claiming to be complete is a lie about
        /// the one thing the reader came here to establish.
        truncated: bool,
    },
}

impl BodyDiff {
    /// Compare two response bodies line by line. **Background executor only** — this parses,
    /// formats and diffs, any one of which disqualifies it from the UI thread.
    pub fn between(previous: &ResponseData, current: &ResponseData) -> Self {
        if previous.body == current.body {
            return Self::Identical;
        }

        let len = previous.body.len().max(current.body.len());
        if len > MAX_DIFF_BYTES {
            return Self::TooLarge { len };
        }

        let Some((old, new)) = normalize(&previous.body, &current.body) else {
            return Self::NotText;
        };

        let (lines, truncated) = build(&old, &new);
        Self::Changed { lines, truncated }
    }

    pub fn lines(&self) -> &[DiffLine] {
        match self {
            Self::Changed { lines, .. } => lines,
            _ => &[],
        }
    }

    /// `(added, removed)` over the lines actually emitted.
    ///
    /// Derived rather than stored, so it cannot disagree with what is on screen — including
    /// when `truncated` means the two are deliberately not the whole story.
    pub fn counts(&self) -> (usize, usize) {
        let mut added = 0;
        let mut removed = 0;
        for line in self.lines() {
            match line.kind {
                LineKind::Insert => added += 1,
                LineKind::Delete => removed += 1,
                _ => {}
            }
        }
        (added, removed)
    }
}

/// Decode both bodies to text, pretty-printing them only if *both* parse as JSON.
fn normalize(previous: &Bytes, current: &Bytes) -> Option<(String, String)> {
    // `JsonOutline::parse` validates UTF-8 itself, but it is not reached when either side
    // isn't JSON — so the check has to happen here too, and it is what separates `NotText`
    // from a body that simply isn't JSON.
    let old = std::str::from_utf8(previous).ok()?;
    let new = std::str::from_utf8(current).ok()?;

    // Bytes::clone is a refcount bump, not a copy.
    if let (Ok(a), Ok(b)) = (
        json::JsonOutline::parse(previous.clone()),
        json::JsonOutline::parse(current.clone()),
    ) {
        return Some((json::format::pretty(&a), json::format::pretty(&b)));
    }

    Some((old.to_string(), new.to_string()))
}

fn build(old: &str, new: &str) -> (Vec<DiffLine>, bool) {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .timeout(BUDGET)
        .diff_lines(old, new);

    let mut out: Vec<DiffLine> = Vec::new();
    let mut truncated = false;
    // Where the previous hunk stopped, in old-side line indices, so the gap to the next one
    // can be reported as a count instead of as nothing.
    let mut cursor = 0usize;

    for hunk in diff.grouped_ops(CONTEXT) {
        let Some(first) = hunk.first() else { continue };
        let start = first.old_range().start;
        if start > cursor {
            out.push(DiffLine {
                kind: LineKind::Skipped(start - cursor),
                old: None,
                new: None,
                text: String::new(),
                changed: Vec::new(),
            });
        }

        for op in &hunk {
            // Equal runs carry no refinement to find, and asking for it allocates a segment
            // list per line for an answer that is always "nothing".
            if op.tag() == DiffTag::Equal {
                for change in diff.iter_changes(op) {
                    out.push(plain(LineKind::Equal, &change));
                }
            } else {
                for change in diff.iter_inline_changes_with_options(op, refinement()) {
                    let kind = match change.tag() {
                        ChangeTag::Insert => LineKind::Insert,
                        ChangeTag::Delete => LineKind::Delete,
                        ChangeTag::Equal => LineKind::Equal,
                    };
                    let mut text = String::new();
                    let mut changed = Vec::new();
                    for (is_changed, value) in change.values() {
                        let start = text.len();
                        text.push_str(value);
                        if *is_changed {
                            changed.push(start..text.len());
                        }
                    }
                    let text = strip_newline(text);
                    // A refinement covering the entire line says the same thing the line's own
                    // `kind` already says, and painting every character as "changed" reads as
                    // noise rather than as detail.
                    if text.len() > MAX_REFINE_BYTES || covers_all(&changed, text.len()) {
                        changed.clear();
                    } else {
                        changed.retain(|range| range.end <= text.len() && range.start < range.end);
                    }
                    out.push(DiffLine {
                        kind,
                        old: number(change.old_index()),
                        new: number(change.new_index()),
                        text,
                        changed,
                    });
                }
            }

            if out.len() >= MAX_DIFF_LINES {
                out.truncate(MAX_DIFF_LINES);
                truncated = true;
                return (out, truncated);
            }
        }

        cursor = hunk
            .last()
            .map(|op| op.old_range().end)
            .unwrap_or(cursor);
    }

    (out, truncated)
}

fn plain(kind: LineKind, change: &similar::Change<&str>) -> DiffLine {
    DiffLine {
        kind,
        old: number(change.old_index()),
        new: number(change.new_index()),
        text: strip_newline(change.value().to_string()),
        changed: Vec::new(),
    }
}

/// `similar` yields lines with their terminator attached; the renderer draws one row per line
/// and a trailing `\n` would shape as a glyph or, worse, trip `shape_line`'s newline assert.
fn strip_newline(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

/// Indices are 0-based inside `similar` and 1-based everywhere a person reads them.
fn number(ix: Option<usize>) -> Option<u32> {
    ix.map(|ix| ix as u32 + 1)
}

fn covers_all(ranges: &[Range<usize>], len: usize) -> bool {
    let covered: usize = ranges.iter().map(|range| range.len()).sum();
    covered >= len
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use crate::request::Header;
    use crate::response::{Connection, HttpVersion, SizeInfo, Timing};

    fn response(body: impl Into<Bytes>, content_type: &str) -> ResponseData {
        let body = body.into();
        ResponseData {
            status: 200,
            status_text: "OK".into(),
            version: HttpVersion::Http11,
            headers: vec![Header::new("content-type", content_type)],
            size: SizeInfo {
                declared: Some(body.len() as u64),
                decoded: body.len() as u64,
            },
            body,
            timing: Timing {
                connection: Connection::Unknown,
                ttfb: Duration::from_millis(1),
                total: Duration::from_millis(2),
            },
        }
    }

    fn json(body: &str) -> ResponseData {
        response(Bytes::copy_from_slice(body.as_bytes()), "application/json")
    }

    /// Lines rendered the way a reader would read them, for assertions that care about shape.
    fn rendered(diff: &BodyDiff) -> Vec<String> {
        diff.lines()
            .iter()
            .map(|line| match line.kind {
                LineKind::Skipped(n) => format!("… {n}"),
                LineKind::Equal => format!("  {}", line.text.trim()),
                LineKind::Insert => format!("+ {}", line.text.trim()),
                LineKind::Delete => format!("- {}", line.text.trim()),
            })
            .collect()
    }

    /// The reason this module exists. A JSON API answers on one line, so diffing the raw bytes
    /// reports "the line changed" — exactly what the summary diff already said and no more.
    #[test]
    fn a_minified_json_body_diffs_field_by_field() {
        let a = json(r#"{"id":1,"name":"ada","role":"admin"}"#);
        let b = json(r#"{"id":1,"name":"grace","role":"admin"}"#);

        let diff = BodyDiff::between(&a, &b);
        let lines = rendered(&diff);

        assert!(
            lines.iter().any(|line| line.starts_with("- ") && line.contains("ada")),
            "the old value should be its own removed line: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("+ ") && line.contains("grace")),
            "the new value should be its own added line: {lines:?}"
        );
        assert_eq!(diff.counts(), (1, 1), "one field moved: {lines:?}");
        assert!(
            lines.iter().any(|line| line.starts_with("  ") && line.contains("\"id\"")),
            "untouched fields stay as context: {lines:?}"
        );
    }

    /// Word-level refinement inside a changed line — the half that makes a long line readable.
    #[test]
    fn a_changed_line_marks_only_the_part_that_moved() {
        let a = json(r#"{"url":"https://example.com/v1/users"}"#);
        let b = json(r#"{"url":"https://example.com/v2/users"}"#);

        let diff = BodyDiff::between(&a, &b);
        let inserted = diff
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Insert)
            .expect("a line was added");

        assert!(
            !inserted.changed.is_empty(),
            "refinement should narrow the line: {inserted:?}"
        );
        let marked: String = inserted
            .changed
            .iter()
            .map(|range| &inserted.text[range.clone()])
            .collect();
        // Character refinement narrows to the one character that moved — `1` -> `2` — rather
        // than to the `v2` segment, because the `v` is shared. That is the minimal true answer,
        // and asserting the whole segment would be asserting a coarser diff than we want.
        assert!(
            marked.contains('2'),
            "the marked span should cover the change, got {marked:?} in {:?}",
            inserted.text
        );
        assert!(
            !marked.contains("example.com") && !marked.contains("users"),
            "nothing shared with the previous line should be marked: {marked:?}"
        );
        assert!(
            marked.len() < inserted.text.len() / 4,
            "refinement should be a narrow span, not most of the line: {marked:?}"
        );
    }

    /// Two lines sharing nothing have no finer detail to give, and marking every character of
    /// one reads as noise rather than as detail.
    #[test]
    fn a_wholly_different_line_carries_no_refinement() {
        let a = json(r#"{"a":"aaaaaaaaaa"}"#);
        let b = json(r#"{"a":"zzzzzzzzzz"}"#);

        let diff = BodyDiff::between(&a, &b);
        for line in diff.lines() {
            let covered: usize = line.changed.iter().map(|range| range.len()).sum();
            assert!(
                covered < line.text.len() || line.changed.is_empty(),
                "a full-line refinement should have been cleared: {line:?}"
            );
        }
    }

    #[test]
    fn identical_bodies_are_reported_as_identical() {
        let a = json(r#"{"a":1}"#);
        let b = json(r#"{"a":1}"#);
        assert_eq!(BodyDiff::between(&a, &b), BodyDiff::Identical);
        assert!(BodyDiff::between(&a, &b).lines().is_empty());
    }

    #[test]
    fn a_body_that_is_not_utf8_is_refused_rather_than_mangled() {
        let a = response(Bytes::from_static(&[0xff, 0xfe, 0x00]), "image/png");
        let b = response(Bytes::from_static(&[0xff, 0xfe, 0x01]), "image/png");
        assert_eq!(BodyDiff::between(&a, &b), BodyDiff::NotText);
    }

    #[test]
    fn an_oversized_body_reports_its_length_rather_than_diffing() {
        let big = Bytes::from(vec![b'a'; MAX_DIFF_BYTES + 1]);
        let a = response(big.clone(), "text/plain");
        let b = response(Bytes::from_static(b"small"), "text/plain");

        match BodyDiff::between(&a, &b) {
            BodyDiff::TooLarge { len } => assert_eq!(len, MAX_DIFF_BYTES + 1),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    /// Pretty-printing one side and not the other would make every line differ, burying the
    /// one thing worth seeing under a wall of re-indented JSON.
    #[test]
    fn json_is_not_reformatted_when_only_one_side_parses() {
        let a = json(r#"{"error":false,"items":[1,2,3]}"#);
        let b = response(
            Bytes::from_static(b"<html><body>Server Error (500)</body></html>"),
            "text/html",
        );

        let diff = BodyDiff::between(&a, &b);
        let removed: Vec<_> = diff
            .lines()
            .iter()
            .filter(|line| line.kind == LineKind::Delete)
            .collect();

        assert_eq!(
            removed.len(),
            1,
            "the JSON side should stay on its one line, not be expanded: {removed:?}"
        );
        assert!(removed[0].text.contains(r#"{"error":false"#));
    }

    /// Without context collapsing a one-field change in a large document is unreadable, and
    /// the gap has to be *reported* — a silent jump in line numbers reads as a bug.
    #[test]
    fn unchanged_runs_are_collapsed_and_the_gap_is_counted() {
        let mut before = String::from("{\n");
        for ix in 0..40 {
            before.push_str(&format!("  \"key{ix}\": {ix},\n"));
        }
        before.push_str("  \"last\": 0\n}\n");
        let after = before.replace("\"key20\": 20", "\"key20\": 999");

        let a = response(Bytes::from(before), "text/plain");
        let b = response(Bytes::from(after), "text/plain");

        let diff = BodyDiff::between(&a, &b);
        let lines = diff.lines();

        let skipped: usize = lines
            .iter()
            .filter_map(|line| match line.kind {
                LineKind::Skipped(n) => Some(n),
                _ => None,
            })
            .sum();
        assert!(skipped > 0, "a 42-line document with one change should collapse: {lines:?}");
        assert!(
            lines.len() < 20,
            "collapsing should leave a short list, got {}",
            lines.len()
        );
        assert_eq!(diff.counts(), (1, 1));
    }

    /// Two documents sharing nothing produce one hunk holding every line of both, which the
    /// byte cap does not constrain.
    #[test]
    fn a_diff_with_nothing_in_common_is_truncated_and_says_so() {
        let before: String = (0..MAX_DIFF_LINES).map(|ix| format!("old line {ix}\n")).collect();
        let after: String = (0..MAX_DIFF_LINES).map(|ix| format!("new line {ix}\n")).collect();

        let a = response(Bytes::from(before), "text/plain");
        let b = response(Bytes::from(after), "text/plain");

        match BodyDiff::between(&a, &b) {
            BodyDiff::Changed { lines, truncated } => {
                assert!(truncated, "{} lines should have tripped the cap", lines.len());
                assert_eq!(lines.len(), MAX_DIFF_LINES);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    /// Line numbers are what let a reader carry a finding back to the body tab.
    #[test]
    fn line_numbers_are_one_based_and_side_specific() {
        let a = response(Bytes::from_static(b"alpha\nbeta\n"), "text/plain");
        let b = response(Bytes::from_static(b"alpha\ngamma\n"), "text/plain");

        let diff = BodyDiff::between(&a, &b);
        let lines = diff.lines();

        let first = &lines[0];
        assert_eq!(first.kind, LineKind::Equal);
        assert_eq!((first.old, first.new), (Some(1), Some(1)));

        let deleted = lines.iter().find(|l| l.kind == LineKind::Delete).unwrap();
        assert_eq!((deleted.old, deleted.new), (Some(2), None));

        let inserted = lines.iter().find(|l| l.kind == LineKind::Insert).unwrap();
        assert_eq!((inserted.old, inserted.new), (None, Some(2)));
    }

    /// `shape_line` has a `debug_assert!` against newlines, so a terminator reaching a row is a
    /// panic in a debug build rather than a cosmetic slip.
    #[test]
    fn no_line_carries_its_terminator() {
        let a = response(Bytes::from_static(b"one\r\ntwo\r\n"), "text/plain");
        let b = response(Bytes::from_static(b"one\r\nthree\r\n"), "text/plain");

        for line in BodyDiff::between(&a, &b).lines() {
            assert!(!line.text.contains('\n'), "{line:?}");
            assert!(!line.text.contains('\r'), "{line:?}");
        }
    }
}
