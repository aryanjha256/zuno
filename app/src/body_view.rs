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
        self.visible = Arc::new(outline.visible_rows(&self.folded));
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
        self.visible = Arc::new(outline.visible_rows(&self.folded));
    }

    pub fn is_json(&self) -> bool {
        matches!(self.kind, BodyKind::Json(_))
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
