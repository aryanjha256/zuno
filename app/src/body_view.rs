//! Deciding how to display a response body, and holding the fold state.
//!
//! Everything here is built on a background executor — parsing, UTF-8 validation, line
//! scanning — and only the finished index crosses back to the renderer
//! (architecture.md §1, rule 2).
//!
//! Fold state lives here rather than inside `JsonOutline` so the outline can stay
//! immutable behind an `Arc` and be shared into a render closure with no locking.

use std::sync::Arc;

use bytes::Bytes;
use zuno_core::{JsonOutline, LineIndex, json};

/// Above this, JSON is not parsed automatically.
///
/// Not a performance limit — 10MB flattens in ~50ms. It's a *memory* limit: 10MB of
/// JSON is 1.3M rows at ~32 bytes each, so the index costs more than the body. Past
/// this the user gets a raw view and an explicit choice, because silently spending
/// hundreds of MB is worse than asking.
pub const MAX_AUTO_PARSE: usize = 10 * 1024 * 1024;

pub enum BodyKind {
    Empty,
    Json(Arc<JsonOutline>),
    Text(Arc<LineIndex>),
    Binary { len: usize },
}

/// Why the body isn't shown as JSON, when it might have been.
#[derive(Debug, Clone)]
pub enum BodyNotice {
    TooLarge { len: usize },
    ParseFailed { message: String },
}

pub struct BodyView {
    pub kind: BodyKind,
    pub notice: Option<BodyNotice>,
    /// Parallel to the outline's rows. Never cloned into a render closure — see
    /// `is_folded_at` for how the renderer infers fold state from `visible` instead.
    folded: Vec<bool>,
    visible: Arc<Vec<u32>>,
    /// Where the reader is standing, as a **row** index rather than a visible one.
    ///
    /// Row index because folding rewrites `visible` underneath it: holding a visible index
    /// would silently retarget the selection at whatever row slid into that slot. Deliberately
    /// separate from the search cursor — a match and where you are standing are different
    /// questions, and sharing one highlight would make stepping through matches move a
    /// selection the user placed.
    selected: Option<u32>,
}

impl BodyView {
    /// Classify and index a body. **Background executor only.**
    pub fn build(body: Bytes, content_type: Option<String>, force_parse: bool) -> Self {
        if body.is_empty() {
            return Self::plain(BodyKind::Empty, None);
        }

        let is_json = looks_like_json(&body, content_type.as_deref());
        let too_large = body.len() > MAX_AUTO_PARSE;

        if is_json && too_large && !force_parse {
            let len = body.len();
            return Self::plain(text_or_binary(body), Some(BodyNotice::TooLarge { len }));
        }

        if is_json {
            match JsonOutline::parse(body.clone()) {
                Ok(outline) => {
                    let folded = vec![false; outline.len()];
                    let visible = Arc::new(outline.visible_rows(&folded));
                    return Self {
                        kind: BodyKind::Json(Arc::new(outline)),
                        notice: None,
                        folded,
                        visible,
                        selected: None,
                    };
                }
                Err(error) => {
                    // Fall back to raw text and say precisely where it broke — a bad
                    // response is a thing you need to *see*, not a thing to hide.
                    let (line, column) = json::line_col(&body, error.offset);
                    let notice = BodyNotice::ParseFailed {
                        message: format!("{} at line {line}, column {column}", error.message),
                    };
                    return Self::plain(text_or_binary(body), Some(notice));
                }
            }
        }

        Self::plain(text_or_binary(body), None)
    }

    fn plain(kind: BodyKind, notice: Option<BodyNotice>) -> Self {
        Self {
            kind,
            notice,
            folded: Vec::new(),
            visible: Arc::new(Vec::new()),
            selected: None,
        }
    }

    pub fn outline(&self) -> Option<&Arc<JsonOutline>> {
        match &self.kind {
            BodyKind::Json(outline) => Some(outline),
            _ => None,
        }
    }

    pub fn visible(&self) -> Arc<Vec<u32>> {
        self.visible.clone()
    }

    /// Number of rows the list should render.
    pub fn row_count(&self) -> usize {
        match &self.kind {
            BodyKind::Json(_) => self.visible.len(),
            BodyKind::Text(lines) => lines.len(),
            BodyKind::Empty | BodyKind::Binary { .. } => 0,
        }
    }

    pub fn toggle_fold(&mut self, row_ix: usize) {
        let Some(outline) = self.outline().cloned() else {
            return;
        };
        let Some(row) = outline.row(row_ix) else { return };
        if !row.kind.is_open() {
            return;
        }

        if let Some(flag) = self.folded.get_mut(row_ix) {
            *flag = !*flag;
        }
        self.rebuild_visible(&outline);
    }

    /// Fold or unfold every container at once.
    pub fn set_all_folded(&mut self, folded: bool) {
        let Some(outline) = self.outline().cloned() else {
            return;
        };

        for (ix, row) in outline.rows().iter().enumerate() {
            // Never fold the root — collapsing to a single `{ … }` looks like a bug.
            let is_root = ix == 0;
            self.folded[ix] = folded && row.kind.is_open() && !is_root;
        }
        self.rebuild_visible(&outline);
    }

    pub fn is_json(&self) -> bool {
        matches!(self.kind, BodyKind::Json(_))
    }

    /// The raw source of whatever is on screen, for searching.
    ///
    /// `None` for an empty or binary body: there is nothing rendered to jump *to*, so
    /// offering a match count over bytes nobody can see would be a lie about what the find
    /// bar can do.
    pub fn searchable_source(&self) -> Option<&Bytes> {
        match &self.kind {
            BodyKind::Json(outline) => Some(outline.source()),
            BodyKind::Text(lines) => Some(lines.source()),
            BodyKind::Empty | BodyKind::Binary { .. } => None,
        }
    }

    /// Turn byte offsets into the rows that hold them.
    ///
    /// The two index types answer this differently — a merge for the outline, a binary search
    /// for lines — which is why this dispatches rather than the caller matching on `kind`.
    pub fn rows_for_offsets(&self, offsets: &[u32]) -> Vec<u32> {
        match &self.kind {
            BodyKind::Json(outline) => outline.rows_for_offsets(offsets),
            BodyKind::Text(lines) => lines.lines_for_offsets(offsets),
            BodyKind::Empty | BodyKind::Binary { .. } => Vec::new(),
        }
    }

    /// Unfold whatever is needed for `row_ix` to be on screen, and return where it now sits
    /// in the visible index — which is what `uniform_list` scrolls by.
    ///
    /// **Unfolding is the point.** A match inside a folded subtree has no visible row at all,
    /// so scrolling to it without opening its ancestors lands somewhere arbitrary and the
    /// search looks broken. Folding a subtree and then searching into it is not an edge case;
    /// it's the normal way of working through a large response.
    pub fn reveal(&mut self, row_ix: usize) -> Option<usize> {
        match &self.kind {
            // No folding in the raw view, so a line's row index *is* its visible index.
            BodyKind::Text(lines) => (row_ix < lines.len()).then_some(row_ix),
            BodyKind::Json(outline) => {
                let outline = outline.clone();
                if row_ix >= outline.len() {
                    return None;
                }

                let mut changed = false;
                for ancestor in outline.ancestors_of(row_ix) {
                    if let Some(flag) = self.folded.get_mut(ancestor) {
                        if *flag {
                            *flag = false;
                            changed = true;
                        }
                    }
                }
                if changed {
                    self.rebuild_visible(&outline);
                }

                // The visible index is a sorted list of row indices, so this is a lookup, not
                // a scan. It cannot miss now that the ancestors are open.
                self.visible.binary_search(&(row_ix as u32)).ok()
            }
            BodyKind::Empty | BodyKind::Binary { .. } => None,
        }
    }

    /// Recompute the visible index, keeping the selection on a row that still exists.
    ///
    /// Folding a container the selection sits inside removes its row from `visible`, and a
    /// selection nothing draws is a cursor the user has lost track of: the next `down` would
    /// jump from wherever it secretly was. Snapping to the nearest visible row at or above it
    /// lands on the container that was just folded, which is where the reader is looking.
    fn rebuild_visible(&mut self, outline: &JsonOutline) {
        self.visible = Arc::new(outline.visible_rows(&self.folded));

        if let Some(selected) = self.selected {
            if self.visible.binary_search(&selected).is_err() {
                let above = self.visible.partition_point(|row| *row < selected);
                self.selected = above
                    .checked_sub(1)
                    .and_then(|ix| self.visible.get(ix).copied());
            }
        }
    }

    /// The selected **row** index, for highlighting and for the copy verbs.
    pub fn selected(&self) -> Option<u32> {
        self.selected
    }

    /// Where the selection sits in the visible index, which is what `uniform_list` scrolls by.
    fn selected_visible_ix(&self) -> Option<usize> {
        let selected = self.selected?;
        match &self.kind {
            BodyKind::Text(_) => Some(selected as usize),
            BodyKind::Json(_) => self.visible.binary_search(&selected).ok(),
            BodyKind::Empty | BodyKind::Binary { .. } => None,
        }
    }

    /// Select the row currently drawn at `visible_ix`, returning it for scrolling.
    pub fn select_visible(&mut self, visible_ix: usize) -> Option<usize> {
        let row_ix = match &self.kind {
            BodyKind::Text(lines) => (visible_ix < lines.len()).then_some(visible_ix as u32),
            BodyKind::Json(_) => self.visible.get(visible_ix).copied(),
            BodyKind::Empty | BodyKind::Binary { .. } => None,
        }?;

        self.selected = Some(row_ix);
        Some(visible_ix)
    }

    /// Step the selection by `delta` visible rows, returning where it landed.
    ///
    /// Clamped rather than wrapping: running off the end of a 1.3M-row response and reappearing
    /// at the top loses your place completely, and there is no scrollbar cue that it happened.
    pub fn move_selection(&mut self, delta: isize) -> Option<usize> {
        let count = self.row_count();
        if count == 0 {
            return None;
        }

        let next = match self.selected_visible_ix() {
            Some(current) => (current as isize + delta).clamp(0, count as isize - 1) as usize,
            // The first press lands on an end rather than one step in from it, so `down` from
            // nothing selects the first row instead of the second.
            None if delta > 0 => 0,
            None => count - 1,
        };

        self.select_visible(next)
    }

    /// Whether the selected row opens a container, and so can be folded.
    pub fn selected_is_container(&self) -> bool {
        let Some(selected) = self.selected else {
            return false;
        };
        self.outline()
            .and_then(|outline| outline.row(selected as usize))
            .is_some_and(|row| row.kind.is_open())
    }

    /// Whether the selected container is currently folded, for the menu's label.
    pub fn selected_is_folded(&self) -> bool {
        let Some(selected) = self.selected else {
            return false;
        };
        self.folded.get(selected as usize).copied().unwrap_or(false)
    }

    /// Fold or unfold the selected container.
    ///
    /// Takes no row index, unlike `toggle_fold`, because all three surfaces that reach it —
    /// the chevron, a double-click, the context menu — select the row first. One verb on the
    /// selection beats three callers each naming their own row.
    pub fn toggle_selected_fold(&mut self) {
        if let Some(selected) = self.selected {
            self.toggle_fold(selected as usize);
        }
    }

    /// The selected row's value, ready for the clipboard.
    ///
    /// A JSON scalar comes back decoded — see `json::unquote` for why stripping the quotes
    /// alone is the wrong answer — and a container comes back as its own source text, braces
    /// included. `unquote` passes both containers and non-string scalars through untouched,
    /// which is why there is no match on `ScalarKind` here.
    ///
    /// In the raw view it is the **whole** line, not the drawn one: what you paste has to be
    /// what came back, the same rule `CopyResponse` follows.
    pub fn selected_value(&self) -> Option<String> {
        let selected = self.selected? as usize;

        match &self.kind {
            BodyKind::Json(outline) => {
                let span = outline.value_span(selected)?;
                Some(json::unquote(outline.text(span)))
            }
            BodyKind::Text(lines) => lines.full_line(selected).map(str::to_string),
            BodyKind::Empty | BodyKind::Binary { .. } => None,
        }
    }

    /// The selected row's JSONPath.
    ///
    /// `None` in the raw view, where there is no structure to name a position within — the
    /// verb is hidden there rather than copying a line number that no tool accepts.
    pub fn selected_path(&self) -> Option<String> {
        let selected = self.selected? as usize;
        self.outline()?.path_to(selected)
    }

    /// Whether a match at `offset` is inside the part of its line that's actually drawn.
    ///
    /// Only the raw view can answer no: it cuts lines at `MAX_DISPLAY_LINE`, and minified JSON
    /// is one line that can be megabytes wide. The JSON view renders whole tokens, so a match
    /// in a key or value is always visible once its row is.
    pub fn offset_is_displayed(&self, offset: u32) -> bool {
        match &self.kind {
            BodyKind::Text(lines) => lines.offset_is_displayed(offset),
            _ => true,
        }
    }
}

/// Whether a visible row is a folded container.
///
/// Derived from `visible` rather than from the fold flags, so the render closure never
/// has to capture (and therefore clone) a `Vec<bool>` that is one byte per row — 1.3MB
/// for a 10MB response. An unfolded open row is always followed immediately by either
/// its first child or its own close row, both at `row_ix + 1`; anything else means the
/// subtree was skipped.
pub fn is_folded_at(visible: &[u32], visible_ix: usize, row_ix: usize) -> bool {
    match visible.get(visible_ix + 1) {
        Some(next) => *next as usize != row_ix + 1,
        None => true,
    }
}

fn text_or_binary(body: Bytes) -> BodyKind {
    if std::str::from_utf8(&body).is_ok() {
        BodyKind::Text(Arc::new(LineIndex::build(body)))
    } else {
        BodyKind::Binary { len: body.len() }
    }
}

/// Content-Type first, then sniff the first byte.
///
/// Sniffing matters because plenty of real APIs return JSON as `text/plain` or with no
/// type at all; refusing to parse those would make the viewer useless exactly where it's
/// most needed. But an explicit non-JSON type is respected — an HTML error page that
/// happens to start with `{` should not be parsed as JSON.
fn looks_like_json(body: &[u8], content_type: Option<&str>) -> bool {
    if let Some(content_type) = content_type {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.contains("json") {
            return true;
        }
        if content_type.contains("html")
            || content_type.contains("xml")
            || content_type.starts_with("image/")
            || content_type.starts_with("audio/")
            || content_type.starts_with("video/")
            || content_type.contains("javascript")
            || content_type.contains("css")
        {
            return false;
        }
    }

    body.iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| matches!(byte, b'{' | b'['))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(body: &'static str, content_type: Option<&str>) -> BodyView {
        BodyView::build(
            Bytes::from_static(body.as_bytes()),
            content_type.map(str::to_string),
            false,
        )
    }

    #[test]
    fn json_content_type_is_parsed() {
        let view = build(r#"{"a":1}"#, Some("application/json; charset=utf-8"));
        assert!(view.is_json());
        assert!(view.notice.is_none());
    }

    #[test]
    fn json_sent_as_text_plain_is_still_parsed() {
        // Real APIs do this constantly.
        assert!(build(r#"{"a":1}"#, Some("text/plain")).is_json());
    }

    #[test]
    fn json_with_no_content_type_is_sniffed() {
        assert!(build(r#"[1,2,3]"#, None).is_json());
    }

    #[test]
    fn an_html_error_page_is_not_parsed_as_json() {
        // Even one that starts with a brace.
        let view = build("{not html but typed as html}", Some("text/html"));
        assert!(!view.is_json());
        assert!(view.notice.is_none(), "declining to parse HTML is not a failure");
    }

    #[test]
    fn plain_text_falls_back_to_lines_without_a_notice() {
        let view = build("hello\nworld", Some("text/plain"));
        assert!(!view.is_json());
        assert!(view.notice.is_none());
        assert_eq!(view.row_count(), 2);
    }

    #[test]
    fn malformed_json_falls_back_to_text_and_says_where() {
        let view = build(r#"{"a":1,}"#, Some("application/json"));
        assert!(!view.is_json(), "should not pretend to have parsed");

        let Some(BodyNotice::ParseFailed { message }) = &view.notice else {
            panic!("expected a ParseFailed notice, got {:?}", view.notice);
        };
        assert!(message.contains("line 1"), "message should locate it: {message}");
        assert!(view.row_count() > 0, "the raw body must still be visible");
    }

    #[test]
    fn a_non_utf8_body_is_binary() {
        let view = BodyView::build(Bytes::from_static(&[0xff, 0xfe, 0x00]), None, false);
        assert!(matches!(view.kind, BodyKind::Binary { len: 3 }));
        assert_eq!(view.row_count(), 0);
    }

    #[test]
    fn an_empty_body_is_empty() {
        let view = build("", Some("application/json"));
        assert!(matches!(view.kind, BodyKind::Empty));
    }

    #[test]
    fn an_oversized_body_is_not_parsed_but_is_still_shown() {
        let big = format!("[{}]", "1,".repeat(MAX_AUTO_PARSE / 2 + 8));
        let view = BodyView::build(Bytes::from(big.clone()), Some("application/json".into()), false);

        assert!(!view.is_json(), "over the cap, JSON must not be parsed");
        assert!(matches!(view.notice, Some(BodyNotice::TooLarge { .. })));
        assert!(view.row_count() > 0, "raw view must still render");

        // ...and forcing it parses. (Malformed here, since the generator leaves a
        // trailing comma — what matters is that force bypasses the cap.)
        let forced = BodyView::build(Bytes::from(big), Some("application/json".into()), true);
        assert!(
            forced.is_json() || matches!(forced.notice, Some(BodyNotice::ParseFailed { .. })),
            "forcing should attempt a parse rather than report TooLarge"
        );
    }

    #[test]
    fn folding_updates_the_visible_index() {
        let view = build(r#"{"a":{"b":1,"c":2},"d":3}"#, Some("application/json"));
        let before = view.row_count();

        let mut view = view;
        view.toggle_fold(1);
        assert!(view.row_count() < before, "folding should hide rows");

        view.toggle_fold(1);
        assert_eq!(view.row_count(), before, "unfolding should restore them");
    }

    #[test]
    fn folding_a_scalar_row_does_nothing() {
        let mut view = build(r#"{"a":1}"#, Some("application/json"));
        let before = view.row_count();
        view.toggle_fold(1); // the scalar
        assert_eq!(view.row_count(), before);
    }

    #[test]
    fn fold_all_leaves_the_root_open() {
        let mut view = build(r#"{"a":{"b":1},"c":[2,3]}"#, Some("application/json"));
        view.set_all_folded(true);

        // root open, "a": {…}, "c": […], root close
        assert_eq!(view.row_count(), 4);

        view.set_all_folded(false);
        assert_eq!(view.row_count(), view.outline().unwrap().len());
    }

    #[test]
    fn is_folded_at_infers_state_from_the_visible_index() {
        let view = build(r#"{"a":{"b":1},"c":2}"#, Some("application/json"));
        let visible = view.visible();

        // Row 1 is the nested open; unfolded, its child follows at row 2.
        assert!(!is_folded_at(&visible, 1, 1));

        let mut folded = view;
        folded.toggle_fold(1);
        let visible = folded.visible();
        assert!(
            is_folded_at(&visible, 1, 1),
            "after folding, row 1's successor is no longer row 2"
        );
    }

    /// `{ "a": {x,y}, "b": {p,q} }` — two sibling containers, so folding one shifts the other's
    /// visible indices away from its row indices.
    fn siblings() -> BodyView {
        build(
            r#"{"a":{"x":1,"y":2},"b":{"p":3,"q":4}}"#,
            Some("application/json"),
        )
    }

    #[test]
    fn reveal_returns_the_visible_index_not_the_row_index() {
        // **The distinction that makes or breaks the scroll.** `uniform_list` addresses items by
        // visible index, so scrolling to a row index once anything above is folded lands
        // somewhere else entirely — or past the end. Rows: 0 { 1 "a":{ 2 x 3 y 4 } 5 "b":{
        // 6 p 7 q 8 } 9 }.
        let mut view = siblings();
        view.toggle_fold(1); // fold "a", hiding rows 2..=4
        assert_eq!(view.row_count(), 7, "visible = [0,1,5,6,7,8,9]");

        // Row 6 is "p". Nothing hides it, so no unfolding is needed — only translation.
        assert_eq!(
            view.reveal(6),
            Some(3),
            "row 6 sits at visible index 3 once \"a\" is folded"
        );
    }

    #[test]
    fn reveal_unfolds_an_ancestor_and_then_translates() {
        let mut view = siblings();
        view.toggle_fold(5); // fold "b", hiding the row we're about to target

        let visible = view.reveal(6).expect("row 6 must be reachable");
        assert!(
            view.visible().contains(&6),
            "revealing has to open the container first"
        );
        assert_eq!(
            view.visible()[visible], 6,
            "and the returned index must address that row in the visible list"
        );
    }

    #[test]
    fn reveal_leaves_unrelated_folds_alone() {
        // Only ancestors open. A jump that unfolded everything would throw away the collapsing
        // someone did to make a large response readable in the first place.
        let mut view = siblings();
        view.toggle_fold(1);
        view.toggle_fold(5);

        view.reveal(6).expect("row 6");
        assert!(view.visible().contains(&6), "\"b\" opened");
        assert!(
            !view.visible().contains(&2),
            "\"a\" was not an ancestor and must stay folded"
        );
    }

    #[test]
    fn reveal_rejects_a_row_that_does_not_exist() {
        assert_eq!(siblings().reveal(9_999), None);
    }

    #[test]
    fn reveal_on_a_raw_text_body_is_the_line_itself() {
        // No folding in the raw view, so row and visible index coincide — and a line past the
        // end must still be refused rather than scrolling nowhere.
        let mut view = build("one\ntwo\nthree", Some("text/plain"));
        assert_eq!(view.reveal(2), Some(2));
        assert_eq!(view.reveal(3), None);
    }

    #[test]
    fn a_folded_last_row_is_detected() {
        // Folding the root leaves it as the only visible row, with no successor.
        let mut view = build(r#"{"a":1}"#, Some("application/json"));
        view.folded[0] = true;
        view.visible = Arc::new(view.outline().unwrap().visible_rows(&view.folded));

        let visible = view.visible();
        assert_eq!(visible.len(), 1);
        assert!(is_folded_at(&visible, 0, 0));
    }
}
