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
use zuno_core::{JsonOutline, LineIndex, hex, html, json};

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
    /// A hex dump of a body that isn't text, indexed as lines.
    ///
    /// **A variant of its own rather than reusing `Text`**, even though it holds the same type
    /// and behaves identically in almost every accessor. `raw_is_json` is `Text(_)` plus a
    /// notice, meaning "this was meant to be JSON and fell back to raw" — and a truncated dump
    /// carries a notice, so reusing `Text` would have had the viewer syntax-highlight a hex dump
    /// as JSON. The distinction costs one extra pattern in the arms that treat them alike.
    Hex(Arc<LineIndex>),

}

/// Image bodies over this are not decoded for preview: the hex dump still answers "is this the
/// thing I asked for", and decoding a huge one would hold its full-size pixels in memory.
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// Images declaring more pixels than this are not decoded: about 200 MB once decoded. For an
/// animated GIF it is every frame together, since each one is decoded and kept.
pub const MAX_IMAGE_PIXELS: u64 = 50_000_000;
/// An image whose size cannot be read from its header is previewed only below this, since
/// nothing else bounds what decoding it would allocate.
const MAX_UNSIZED_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// Turns an image body into pixels: gpui's own decoder, with the SVG renderer it needs already
/// inside. A closure because gpui does not export that renderer's type, so it cannot be named
/// in a signature, only captured.
pub type ImageDecoder = Box<dyn FnOnce(gpui::Image) -> Result<Arc<gpui::RenderImage>, String> + Send>;

/// A body that decoded as an image.
///
/// **Decoded here, on the background executor, and handed to `img()` finished**, rather than
/// giving `img()` the bytes. Given bytes, gpui decodes into an app-wide asset cache that nothing
/// empties, so every image response would stay decoded until Zuno quit. Held here, the pixels go
/// with the body; the GPU's copy is released by `RequestView::set_body_view`, which is the one
/// place a body is replaced.
pub struct ImagePreview {
    pub image: Arc<gpui::RenderImage>,
    pub format: &'static str,
    /// The image, rather than its bytes, is on screen.
    pub showing: bool,
}

/// Why the body isn't shown as JSON, when it might have been.
#[derive(Debug, Clone)]
pub enum BodyNotice {
    /// Declared or recognised as an image, but its bytes did not decode as one.
    ImageUndecodable { message: String },
    /// An image too large to preview, by bytes or by pixels.
    ImageTooLarge,
    TooLarge { len: usize },
    ParseFailed { message: String },
    /// An HTML body past `html::MAX_EXTRACT_BYTES`, so there is no text view to offer.
    HtmlTooLarge { len: usize },
    /// A binary body past `hex::MAX_DUMP_BYTES`; the dump shows its first megabyte.
    HexTruncated { len: usize },
}

/// Which half of an HTML body is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HtmlView {
    /// The text pulled out of the markup — the default, because in an API client an HTML body
    /// arriving is overwhelmingly a framework saying something went wrong, and the message is
    /// what you came for. Anyone here to read markup flips it once and it sticks.
    #[default]
    Text,
    Raw,
}

impl HtmlView {
    pub fn other(self) -> Self {
        match self {
            Self::Text => Self::Raw,
            Self::Raw => Self::Text,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Raw => "raw",
        }
    }
}

/// Both halves of an HTML body, and which is showing.
///
/// **Held here while `kind` points at one of them**, rather than becoming a `BodyKind` variant of
/// its own. A variant would be the tidier model and would cost an arm in each of the ten
/// accessors that match on `kind` — `row_count`, `searchable_source`, `rows_for_offsets`,
/// `select_visible` and the rest — every one of which would then have to re-ask which half is
/// showing. Swapping the `Arc` inside `BodyKind::Text` instead means *none* of them change, and
/// the text view gets the raw view's search, selection and horizontal scrolling for free.
struct HtmlBody {
    raw: Arc<LineIndex>,
    text: Arc<LineIndex>,
    showing: HtmlView,
}

pub struct BodyView {
    pub kind: BodyKind,
    pub notice: Option<BodyNotice>,
    /// `Some` only for an HTML body small enough to have been extracted. `None` is what makes
    /// the toggle absent rather than present-and-inert on every other kind of body.
    html: Option<HtmlBody>,
    /// Parallel to the outline's rows. Never cloned into a render closure — see
    /// `is_folded_at` for how the renderer infers fold state from `visible` instead.
    folded: Vec<bool>,
    visible: Arc<Vec<u32>>,
    /// The **visible** index of the row to measure the horizontal scroll region from, and that
    /// row's indent depth and drawn character count. See `widest_json_row`.
    ///
    /// Recomputed on every fold, because folding is how a wide response is made readable and a
    /// horizontal extent that ignores it defeats the point: collapsing a document left the
    /// region as wide as the longest row it *used* to show, so the view stayed scrolled into
    /// blank space with nothing left out there to find.
    widest_visible: usize,
    widest_extent: (u16, u32),
    /// Where the reader is standing, as a **row** index rather than a visible one.
    ///
    /// Row index because folding rewrites `visible` underneath it: holding a visible index
    /// would silently retarget the selection at whatever row slid into that slot. Deliberately
    /// separate from the search cursor — a match and where you are standing are different
    /// questions, and sharing one highlight would make stepping through matches move a
    /// selection the user placed.
    selected: Option<u32>,
    /// The open and close rows of the container the selection sits in, whose indent guide is
    /// drawn brighter. Kept beside `selected` and set only with it: finding it scans back from
    /// the selection, which inside a large array is too long a walk to repeat every frame.
    scope: Option<(u32, u32)>,
    /// `Some` for a body that decoded as an image. `kind` still holds its hex dump (or, for an
    /// SVG, its source), so search, copy and every accessor keep working on the bytes, and the
    /// toggle only changes what is drawn.
    image: Option<ImagePreview>,
}

impl BodyView {
    /// Classify and index a body. **Background executor only.**
    ///
    /// `images` decodes an image body; `None` skips image previews, as for a WebSocket frame,
    /// where the bytes are payload rather than a picture to look at.
    pub fn build(
        body: Bytes,
        content_type: Option<String>,
        force_parse: bool,
        html_view: HtmlView,
        images: Option<ImageDecoder>,
    ) -> Self {
        if body.is_empty() {
            return Self::plain(BodyKind::Empty, None);
        }
        if let (Some(decode), Some((format, label))) =
            (images, image_format(&body, content_type.as_deref()))
        {
            return Self::decoded_image(body, format, label, decode);
        }
        let body = to_utf8(body, content_type.as_deref());

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
                    let mut view = Self {
                        kind: BodyKind::Json(Arc::new(outline)),
                        notice: None,
                        html: None,
                        folded,
                        visible: Arc::new(Vec::new()),
                        widest_visible: 0,
                        widest_extent: (0, 0),
                        selected: None,
                        scope: None,
                        image: None,
                    };
                    // The same path a fold takes, so the initial state cannot disagree with
                    // every later one.
                    let outline = view.outline().cloned().expect("just built");
                    view.rebuild_visible(&outline);
                    return view;
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

        if html::looks_like_html(&body, content_type.as_deref()) {
            return Self::html(body, html_view);
        }

        let len = body.len();
        let kind = text_or_binary(body);
        // Only a hex dump can be cut; text is indexed whole.
        let notice = matches!(kind, BodyKind::Hex(_))
            .then(|| hex::is_truncated(len).then_some(BodyNotice::HexTruncated { len }))
            .flatten();
        Self::plain(kind, notice)
    }

    /// An image body: its bytes indexed as usual, and the picture decoded beside them.
    ///
    /// Anything that goes wrong falls back to the bytes with a notice, never to a blank pane: an
    /// HTML error page served as `image/png` is exactly what the hex dump exists to show.
    fn decoded_image(
        body: Bytes,
        format: gpui::ImageFormat,
        label: &'static str,
        decode: ImageDecoder,
    ) -> Self {
        let len = body.len();
        let bytes_view = |notice: Option<BodyNotice>| {
            let kind = text_or_binary(body.clone());
            let truncated = matches!(kind, BodyKind::Hex(_)) && hex::is_truncated(len);
            Self::plain(kind, notice.or(truncated.then_some(BodyNotice::HexTruncated { len })))
        };
        if len > MAX_IMAGE_BYTES {
            return bytes_view(Some(BodyNotice::ImageTooLarge));
        }
        // Bytes that do not even start like the declared format are not that format: an HTML
        // error page served as `image/png` is the usual case, and reading its first bytes as a
        // PNG header would report a nonsense size rather than the truth.
        if !has_signature(&body, format) {
            return bytes_view(Some(BodyNotice::ImageUndecodable {
                message: format!("the bytes do not start like a {label}"),
            }));
        }
        // **Before decoding, from the header.** A few kilobytes can declare a picture of billions
        // of pixels, and the decoder allocates for what is declared, so a cap checked afterwards
        // has already paid for it. A body whose size cannot be read is previewed only while small.
        match declared_pixels(&body, format) {
            Some(pixels) if pixels > MAX_IMAGE_PIXELS => {
                return bytes_view(Some(BodyNotice::ImageTooLarge));
            }
            None if len > MAX_UNSIZED_IMAGE_BYTES => {
                return bytes_view(Some(BodyNotice::ImageTooLarge));
            }
            _ => {}
        }
        match decode(gpui::Image::from_bytes(format, body.to_vec())) {
            Ok(image) => {
                let size = image.size(0);
                if u64::from(size.width.0 as u32) * u64::from(size.height.0 as u32) > MAX_IMAGE_PIXELS {
                    return bytes_view(Some(BodyNotice::ImageTooLarge));
                }
                let mut view = bytes_view(None);
                view.image = Some(ImagePreview { image, format: label, showing: true });
                view
            }
            Err(message) => bytes_view(Some(BodyNotice::ImageUndecodable { message })),
        }
    }

    /// The decoded image, if this body is one.
    pub fn image(&self) -> Option<&ImagePreview> {
        self.image.as_ref()
    }

    /// Whether the picture, rather than its bytes, is on screen.
    pub fn showing_image(&self) -> bool {
        self.image.as_ref().is_some_and(|image| image.showing)
    }

    /// Show the picture or its bytes. Showing the picture clears the row selection, which belongs
    /// to the bytes: kept, the copy verbs would copy a row that is no longer on screen.
    pub fn set_image_showing(&mut self, showing: bool) {
        let Some(image) = self.image.as_mut() else { return };
        image.showing = showing;
        if showing {
            self.select_row(None);
        }
    }

    /// Hand back the decoded image so its GPU copy can be released.
    pub fn take_image(&mut self) -> Option<Arc<gpui::RenderImage>> {
        self.image.take().map(|preview| preview.image)
    }

    /// An HTML body, indexed twice — once as markup, once as the text inside it.
    ///
    /// Both up front rather than on demand: the extraction is the expensive half and it is
    /// already on the background executor here, whereas extracting at the moment of the toggle
    /// would put a hundred-millisecond job behind a keystroke and need a second loading state
    /// for a body that is, in the case this serves, a few kilobytes.
    fn html(body: Bytes, showing: HtmlView) -> Self {
        let len = body.len();
        let kind = text_or_binary(body.clone());
        let BodyKind::Text(raw) = kind else {
            // Declared HTML but not valid UTF-8, so there is no text to extract and no second
            // half to toggle to. It still gets a hex dump like any other undecodable body.
            let notice = hex::is_truncated(len).then_some(BodyNotice::HexTruncated { len });
            return Self::plain(kind, notice);
        };

        let Some(text) = html::to_text(&body) else {
            return Self::plain(
                BodyKind::Text(raw),
                (len > html::MAX_EXTRACT_BYTES).then_some(BodyNotice::HtmlTooLarge { len }),
            );
        };

        let text = Arc::new(LineIndex::build(Bytes::from(text)));
        let mut view = Self::plain(BodyKind::Text(Arc::clone(&raw)), None);
        view.html = Some(HtmlBody { raw, text, showing: HtmlView::Text });
        // Through the same path a later toggle takes, so the initial state cannot disagree with
        // every subsequent one — the trick `rebuild_visible` already plays for folds.
        view.set_html_view(showing);
        view
    }

    /// Which half of an HTML body is showing, or `None` if this body has no two halves.
    pub fn html_view(&self) -> Option<HtmlView> {
        self.html.as_ref().map(|html| html.showing)
    }

    /// Swap between the markup and the text pulled out of it.
    pub fn set_html_view(&mut self, view: HtmlView) {
        let Some(html) = self.html.as_mut() else { return };
        html.showing = view;
        let lines = match view {
            HtmlView::Text => Arc::clone(&html.text),
            HtmlView::Raw => Arc::clone(&html.raw),
        };
        // The selection is an index into the half being left, and the two halves share no line
        // numbering at all — row 40 of the markup is nowhere near row 40 of the text — so
        // carrying it over would land the cursor somewhere arbitrary and call it where you were.
        self.select_row(None);
        (self.widest_visible, self.widest_extent) = widest_of(&lines);
        self.kind = BodyKind::Text(lines);
    }

    fn plain(kind: BodyKind, notice: Option<BodyNotice>) -> Self {
        let (widest_visible, widest_extent) = match &kind {
            BodyKind::Text(lines) => widest_of(lines),
            _ => (0, (0, 0)),
        };

        Self {
            kind,
            notice,
            html: None,
            folded: Vec::new(),
            visible: Arc::new(Vec::new()),
            widest_visible,
            widest_extent,
            selected: None,
            scope: None,
            image: None,
        }
    }

    /// The widest row's `(depth, characters)`, for sizing the horizontal scroll region.
    ///
    /// **The pane converts this to pixels itself rather than letting `uniform_list` measure a
    /// row**, because that measurement is taken *before* `interactivity.prepaint` pushes the
    /// list's own text style — so the row is shaped in whatever font is ambient, not the
    /// `mono`/`text_xs` it will actually be drawn in. The two agree in the headless platform,
    /// which is why every test passed while the real window scrolled short of the line's end.
    pub fn widest_extent(&self) -> (u16, u32) {
        self.widest_extent
    }

    /// The row `uniform_list` should measure to size its horizontal scroll region.
    ///
    /// It measures exactly **one** row (`with_width_from_item`, default 0) rather than scanning
    /// for the widest, so handing it the wrong index is not a rounding error — it is the
    /// difference between scrolling and not. Row 0 of a JSON document is `{`, the narrowest row
    /// there is, so the default would size the region to nothing.
    ///
    /// Returned as a **visible** index, because that is how the list addresses items.
    pub fn widest_visible_ix(&self) -> usize {
        self.widest_visible
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
            BodyKind::Text(lines) | BodyKind::Hex(lines) => lines.len(),
            BodyKind::Empty => 0,
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

    /// Whether a raw-text body was *meant* to be JSON.
    ///
    /// True exactly when a notice explains why it isn't rendered as an outline — over the
    /// auto-parse cap, or failed to parse. Both are worth colouring: the first is ordinary JSON
    /// too big to index, and on the second the colour is how you find where it went wrong. A
    /// body with no notice is genuinely something else, and lexing HTML as JSON would tint a
    /// stray attribute green for no reason.
    pub fn raw_is_json(&self) -> bool {
        matches!(self.kind, BodyKind::Text(_)) && self.notice.is_some()
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
            BodyKind::Text(lines) | BodyKind::Hex(lines) => Some(lines.source()),
            BodyKind::Empty => None,
        }
    }

    /// Turn byte offsets into the rows that hold them.
    ///
    /// The two index types answer this differently — a merge for the outline, a binary search
    /// for lines — which is why this dispatches rather than the caller matching on `kind`.
    pub fn rows_for_offsets(&self, offsets: &[u32]) -> Vec<u32> {
        match &self.kind {
            BodyKind::Json(outline) => outline.rows_for_offsets(offsets),
            BodyKind::Text(lines) | BodyKind::Hex(lines) => lines.lines_for_offsets(offsets),
            BodyKind::Empty => Vec::new(),
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
            BodyKind::Text(lines) | BodyKind::Hex(lines) => (row_ix < lines.len()).then_some(row_ix),
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
            BodyKind::Empty => None,
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

        // Over the rows now *drawn*, not every row in the document. A folded container's
        // contents are not on screen, so they must not decide how far the view can scroll —
        // once the extent shrinks, gpui's own prepaint clamp pulls the offset back in.
        let (widest_visible, widest_extent) = widest_json_row(outline, &self.visible);
        self.widest_visible = widest_visible;
        self.widest_extent = widest_extent;

        if let Some(selected) = self.selected {
            if self.visible.binary_search(&selected).is_err() {
                let above = self.visible.partition_point(|row| *row < selected);
                let snapped = above
                    .checked_sub(1)
                    .and_then(|ix| self.visible.get(ix).copied());
                self.select_row(snapped);
            }
        }
    }

    /// The selected **row** index, for highlighting and for the copy verbs.
    pub fn selected(&self) -> Option<u32> {
        self.selected
    }

    /// The open and close rows of the container holding the selection, if it is in one.
    pub fn selected_scope(&self) -> Option<(u32, u32)> {
        self.scope
    }

    /// Every change of selection comes through here, so `scope` cannot fall out of step.
    fn select_row(&mut self, row: Option<u32>) {
        self.selected = row;
        self.scope = match (&self.kind, row) {
            (BodyKind::Json(outline), Some(row)) => enclosing(outline, row as usize),
            _ => None,
        };
    }

    /// Where the selection sits in the visible index, which is what `uniform_list` scrolls by.
    fn selected_visible_ix(&self) -> Option<usize> {
        let selected = self.selected?;
        match &self.kind {
            BodyKind::Text(_) | BodyKind::Hex(_) => Some(selected as usize),
            BodyKind::Json(_) => self.visible.binary_search(&selected).ok(),
            BodyKind::Empty => None,
        }
    }

    /// Select the row currently drawn at `visible_ix`, returning it for scrolling.
    pub fn select_visible(&mut self, visible_ix: usize) -> Option<usize> {
        let row_ix = match &self.kind {
            BodyKind::Text(lines) | BodyKind::Hex(lines) => (visible_ix < lines.len()).then_some(visible_ix as u32),
            BodyKind::Json(_) => self.visible.get(visible_ix).copied(),
            BodyKind::Empty => None,
        }?;

        self.select_row(Some(row_ix));
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
            // A hex row copies as the row you can see — offset, bytes and gutter. Copying the
            // underlying 16 bytes instead would be the *other* plausible answer and is the wrong
            // one here: the pane is showing text, and what you paste should be what you read.
            BodyKind::Text(lines) | BodyKind::Hex(lines) => {
                lines.full_line(selected).map(str::to_string)
            }
            BodyKind::Empty => None,
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

/// The widest row *currently drawn*, estimated rather than measured.
///
/// One pass over the visible index — 1.31M entries for an unfolded 10MB body. That is the same
/// order as `visible_rows`, which `rebuild_visible` has always run right beside it, so folding
/// costs about twice what it did rather than something new.
///
/// **Not background-only, unlike `build`.** This comment said it was, and said so because the
/// only caller was `build`; folding now calls it too, from the UI thread. Deliberate: the extent
/// has to follow a fold or the scroll region keeps the width of rows that are no longer on
/// screen, and re-indexing off-thread for a keystroke would be worse than the pass itself.
///
/// The estimate only has to pick the right *index and extent* — the pane converts that to pixels
/// with an advance measured in the render font. Character counts work because the body is drawn
/// in a monospace face, which is the one assumption a proportional font would break.
fn widest_json_row(outline: &JsonOutline, visible: &[u32]) -> (usize, (u16, u32)) {
    // Per-character and per-depth-level advances at the viewer's text size. **Only the ratio
    // matters** — this is an argmax, never a measurement — but the ratio matters a great deal:
    // it decides a deep-short row against a shallow-long one, and getting it wrong points the
    // scroll region at a row that isn't the widest, so the end of the real one stays out of
    // reach. `CHAR` was 7.0 against a measured advance of ~8.5, which over-weighted depth.
    //
    // `DEPTH` is `response_pane::INDENT` exactly. `CHAR` is the monospace advance measured from
    // the rendered list, and `a_shallow_long_row_beats_a_deep_short_one` pins the consequence
    // rather than the number.
    const CHAR: f32 = 8.5;
    const DEPTH: f32 = 13.0;

    let mut widest = 0.;
    let mut best = 0;
    let mut extent = (0, 0);

    for (visible_ix, row_ix) in visible.iter().enumerate() {
        let Some(row) = outline.row(*row_ix as usize) else {
            continue;
        };
        let chars = row.key.len + row.value.len;
        let width = row.depth as f32 * DEPTH + chars as f32 * CHAR;
        if width > widest {
            widest = width;
            best = visible_ix;
            extent = (row.depth, chars);
        }
    }

    (best, extent)
}

/// Whether a body is an image Zuno can preview, and the format's name for the label.
///
/// The declared type decides when there is one. Only a body with no type, or the
/// `application/octet-stream` that means "unknown", is sniffed, and only for the four formats
/// whose leading bytes cannot be mistaken for text.
fn image_format(body: &[u8], content_type: Option<&str>) -> Option<(gpui::ImageFormat, &'static str)> {
    use gpui::ImageFormat;
    let declared = content_type
        .map(|value| value.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
    let format = match declared.as_deref() {
        None | Some("application/octet-stream") => {
            if body.starts_with(b"\x89PNG\r\n\x1a\n") {
                ImageFormat::Png
            } else if body.starts_with(&[0xFF, 0xD8, 0xFF]) {
                ImageFormat::Jpeg
            } else if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
                ImageFormat::Gif
            } else if body.len() > 12 && &body[..4] == b"RIFF" && &body[8..12] == b"WEBP" {
                ImageFormat::Webp
            } else {
                return None;
            }
        }
        Some(mime) => ImageFormat::from_mime_type(mime)?,
    };
    let label = match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Webp => "WebP",
        ImageFormat::Gif => "GIF",
        ImageFormat::Svg => "SVG",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Tiff => "TIFF",
    };
    Some((format, label))
}

/// Whether a body starts the way its declared image format must.
fn has_signature(body: &[u8], format: gpui::ImageFormat) -> bool {
    use gpui::ImageFormat;
    match format {
        ImageFormat::Png => body.starts_with(b"\x89PNG\r\n\x1a\n"),
        ImageFormat::Jpeg => body.starts_with(&[0xFF, 0xD8, 0xFF]),
        ImageFormat::Gif => body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a"),
        ImageFormat::Webp => body.len() > 12 && &body[..4] == b"RIFF" && &body[8..12] == b"WEBP",
        ImageFormat::Bmp => body.starts_with(b"BM"),
        ImageFormat::Tiff => body.starts_with(b"II*\0") || body.starts_with(b"MM\0*"),
        // XML may open with a declaration, a comment or a doctype first.
        ImageFormat::Svg => body.get(..body.len().min(4096)).is_some_and(|head| {
            head.windows(4).any(|window| window == b"<svg")
        }),
    }
}

/// How many pixels an image declares it will decode to, read from its header alone.
///
/// `None` when the header cannot be read, which the caller treats as "only if small". Each arm
/// reads the format's own size fields; nothing here decodes pixel data.
fn declared_pixels(body: &[u8], format: gpui::ImageFormat) -> Option<u64> {
    use gpui::ImageFormat;
    let u16_le = |at: usize| body.get(at..at + 2).map(|b| u64::from(u16::from_le_bytes([b[0], b[1]])));
    let u32_be = |at: usize| {
        body.get(at..at + 4).map(|b| u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    };
    let i32_le = |at: usize| {
        body.get(at..at + 4)
            .map(|b| u64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]]).unsigned_abs()))
    };
    let u24_le = |at: usize| {
        body.get(at..at + 3).map(|b| u64::from(b[0]) | u64::from(b[1]) << 8 | u64::from(b[2]) << 16)
    };
    match format {
        // IHDR is always the first chunk: width and height straight after its type.
        ImageFormat::Png => Some(u32_be(16)? * u32_be(20)?),
        ImageFormat::Bmp => Some(i32_le(18)? * i32_le(22)?),
        // The logical screen size, times every frame, since each one is decoded and kept.
        ImageFormat::Gif => Some(u16_le(6)? * u16_le(8)? * gif_frames(body)?.max(1)),
        ImageFormat::Jpeg => jpeg_size(body),
        ImageFormat::Webp => match body.get(12..16)? {
            b"VP8 " => Some((u16_le(26)? & 0x3FFF) * (u16_le(28)? & 0x3FFF)),
            b"VP8L" => {
                let bits = body.get(21..25)?;
                let width = 1 + (u64::from(bits[0]) | (u64::from(bits[1]) & 0x3F) << 8);
                let height = 1
                    + (u64::from(bits[1]) >> 6 | u64::from(bits[2]) << 2 | (u64::from(bits[3]) & 0x0F) << 10);
                Some(width * height)
            }
            b"VP8X" => Some((u24_le(24)? + 1) * (u24_le(27)? + 1)),
            _ => None,
        },
        ImageFormat::Svg => svg_size(body),
        ImageFormat::Tiff => None,
    }
}

/// A JPEG's size, from its first start-of-frame marker.
fn jpeg_size(body: &[u8]) -> Option<u64> {
    let mut at = 2;
    while at + 9 < body.len() {
        if body[at] != 0xFF {
            return None;
        }
        let marker = body[at + 1];
        match marker {
            // Padding, and markers with no length.
            0xFF => at += 1,
            0x01 | 0xD0..=0xD9 => at += 2,
            // Start of frame: every SOFn except DHT (C4), JPG (C8) and DAC (CC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let height = u64::from(u16::from_be_bytes([body[at + 5], body[at + 6]]));
                let width = u64::from(u16::from_be_bytes([body[at + 7], body[at + 8]]));
                return Some(width * height);
            }
            _ => at += 2 + usize::from(u16::from_be_bytes([body[at + 2], body[at + 3]])),
        }
    }
    None
}

/// How many frames a GIF holds, by walking its blocks without decoding any of them.
fn gif_frames(body: &[u8]) -> Option<u64> {
    // Skip a run of sub-blocks: each is a length byte and that many bytes, ending at a zero.
    let skip_sub_blocks = |mut at: usize| -> Option<usize> {
        loop {
            let size = usize::from(*body.get(at)?);
            at += 1 + size;
            if size == 0 {
                return Some(at);
            }
        }
    };
    let flags = *body.get(10)?;
    let mut at = 13 + if flags & 0x80 != 0 { 3 << ((flags & 0x07) + 1) } else { 0 };
    let mut frames = 0;
    loop {
        match *body.get(at)? {
            0x3B => return Some(frames),
            0x21 => at = skip_sub_blocks(at + 2)?,
            0x2C => {
                frames += 1;
                let local = *body.get(at + 9)?;
                at += 10 + if local & 0x80 != 0 { 3 << ((local & 0x07) + 1) } else { 0 };
                // The LZW minimum code size, then the image data.
                at = skip_sub_blocks(at + 1)?;
            }
            _ => return None,
        }
    }
}

/// An SVG's rendered size, from the root element's `width` and `height`, or its `viewBox`.
///
/// gpui renders an SVG at the size it declares, so a document claiming a vast canvas would
/// allocate it. Units other than plain numbers and `px` are not worth interpreting here; they
/// return `None`, and the caller previews only a small body.
fn svg_size(body: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(body.get(..body.len().min(4096))?).ok()?;
    let tag = &text[text.find("<svg")?..];
    let tag = &tag[..tag.find('>')?];
    let attr = |name: &str| -> Option<&str> {
        let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
        Some(&tag[start..start + tag[start..].find('"')?])
    };
    let number = |value: &str| value.trim().trim_end_matches("px").parse::<f64>().ok();
    let (width, height) = match (attr("width").and_then(number), attr("height").and_then(number)) {
        (Some(width), Some(height)) => (width, height),
        _ => {
            let parts: Vec<f64> = attr("viewBox")?.split_whitespace().filter_map(|part| part.parse().ok()).collect();
            (*parts.get(2)?, *parts.get(3)?)
        }
    };
    (width.is_finite() && height.is_finite()).then(|| (width.max(0.) * height.max(0.)) as u64)
}

/// The open and close rows of the container `row` sits in, or `None` at the top level.
///
/// Every row between a container's open and its close is deeper than the open, so walking back
/// from `row` to the first shallower row lands on that open. A close row belongs to the same
/// container as its own open, which the walk gets for free since the two share a depth.
pub fn enclosing(outline: &JsonOutline, row: usize) -> Option<(u32, u32)> {
    let depth = outline.row(row)?.depth;
    let open = (0..row).rev().find(|ix| outline.row(*ix).is_some_and(|r| r.depth < depth))?;
    let close = open + outline.row(open)?.subtree_len as usize;
    Some((open as u32, close as u32))
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

/// The widest line's `(visible index, (depth, characters))`, for the horizontal scroll region.
fn widest_of(lines: &LineIndex) -> (usize, (u16, u32)) {
    let ix = lines.widest_line();
    (ix, (0, lines.line(ix).0.chars().count() as u32))
}

/// The body re-encoded as UTF-8 for display, when it is text in another encoding.
///
/// **Only what is shown changes**: the response keeps the bytes the server sent, so saving,
/// diffing and the size line are untouched. Before this a Latin-1, Windows-1252 or UTF-16 body
/// fell through to a hex dump even when its `charset` said exactly what it was.
///
/// Valid UTF-8 is left alone whatever the header claims, because a label that disagrees with the
/// bytes is wrong more often than the bytes are, and decoding UTF-8 as Latin-1 turns `é` into
/// `Ã©`. Only a body that says it is text is decoded — a textual type or a declared `charset` —
/// and then by its byte-order mark, its `charset`, or Windows-1252, the Encoding Standard's
/// default for legacy text. **A byte-order mark alone is not evidence**: a binary body can open
/// with `FF FE`, and decoding one as UTF-16 replaces its hex dump with nonsense.
fn to_utf8(body: Bytes, content_type: Option<&str>) -> Bytes {
    let bom = encoding_rs::Encoding::for_bom(&body).map(|(encoding, _)| encoding);
    if bom.is_none() && std::str::from_utf8(&body).is_ok() {
        return body;
    }

    let lower = content_type.unwrap_or_default().to_ascii_lowercase();
    let declared = lower.split(';').find_map(|part| {
        let label = part.trim().strip_prefix("charset=")?;
        encoding_rs::Encoding::for_label(label.trim_matches('"').as_bytes())
    });
    let textual = ["text/", "json", "xml", "html", "javascript"]
        .iter()
        .any(|kind| lower.contains(kind));
    if !textual && declared.is_none() {
        return body;
    }

    match bom.or(declared).or(Some(encoding_rs::WINDOWS_1252)) {
        // Declared UTF-8 and not valid as UTF-8: nothing to decode it as, so the hex dump stays.
        Some(encoding) if encoding != encoding_rs::UTF_8 || bom.is_some() => {
            let (text, _, _) = encoding.decode(&body);
            Bytes::from(text.into_owned())
        }
        _ => body,
    }
}

/// Text if it decodes, otherwise a hex dump of the bytes.
///
/// **`BodyKind::Binary` used to be the end of the road** — one line saying how many bytes came
/// back, and no way to tell a JPEG from an HTML error page served with the wrong content type.
/// A hex dump answers that with no per-format decoder, and because it is *text* the viewer's
/// existing search, selection and scrolling apply to it unchanged.
///
/// An empty body never reaches here, so `Binary` now means only "too large to dump".
fn text_or_binary(body: Bytes) -> BodyKind {
    if std::str::from_utf8(&body).is_ok() {
        return BodyKind::Text(Arc::new(LineIndex::build(body)));
    }
    BodyKind::Hex(Arc::new(LineIndex::build(Bytes::from(hex::dump(&body)))))
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
            HtmlView::default(),
            None,
        )
    }

    /// **A tiny file declaring a vast image is refused before it is decoded.** The decoder here
    /// fails the test if it is ever called, so the cap cannot be the after-the-fact kind.
    #[test]
    fn an_image_bomb_is_refused_from_its_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&30_000u32.to_be_bytes());
        png.extend_from_slice(&30_000u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        let decoder: ImageDecoder = Box::new(|_| panic!("a bomb must not reach the decoder"));
        let view = BodyView::build(Bytes::from(png), Some("image/png".into()), false, HtmlView::default(), Some(decoder));
        assert!(view.image().is_none());
        assert!(matches!(view.notice, Some(BodyNotice::ImageTooLarge)), "{:?}", view.notice);
    }

    #[test]
    fn declared_sizes_are_read_from_each_header() {
        use gpui::ImageFormat;
        // A JPEG: an APP0 segment to skip, then a baseline start-of-frame, 512 wide and 256 high.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        jpeg.extend_from_slice(&[0; 14]);
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 8, 0x01, 0x00, 0x02, 0x00]);
        jpeg.extend_from_slice(&[0; 12]);
        assert_eq!(declared_pixels(&jpeg, ImageFormat::Jpeg), Some(512 * 256));

        // A GIF of three 1x1 frames, each counted, since each is decoded and kept.
        let mut gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00".to_vec();
        for _ in 0..3 {
            gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0]);
        }
        gif.push(0x3B);
        assert_eq!(declared_pixels(&gif, ImageFormat::Gif), Some(3));

        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="100000"></svg>"#;
        assert_eq!(declared_pixels(svg, ImageFormat::Svg), Some(10_000_000_000));
        let svg = br#"<svg viewBox="0 0 24 24"><path d="M0 0"/></svg>"#;
        assert_eq!(declared_pixels(svg, ImageFormat::Svg), Some(576));
    }

    /// The brighter guide belongs to the block the selection is in, and moves with it.
    #[test]
    fn the_selected_rows_enclosing_block_is_found() {
        // Rows: 0 {  1 "a": {  2 "b": 1  3 }  4 "c": [  5 2  6 ]  7 }
        let mut view = build(r#"{"a":{"b":1},"c":[2]}"#, Some("application/json"));
        let at = |view: &mut BodyView, row: u32| {
            let visible_ix = view.visible().binary_search(&row).expect("row is drawn");
            view.select_visible(visible_ix);
            view.selected_scope()
        };
        assert_eq!(at(&mut view, 2), Some((1, 3)), "\"b\" is inside \"a\"");
        assert_eq!(at(&mut view, 3), Some((0, 7)), "a close row belongs to its own parent");
        assert_eq!(at(&mut view, 5), Some((4, 6)));
        assert_eq!(at(&mut view, 0), None, "the root sits in nothing");
    }

    /// **Text in another encoding is shown as text.** Each of these was a hex dump.
    #[test]
    fn a_body_in_another_encoding_is_decoded_for_display() {
        let first_line = |bytes: Vec<u8>, content_type: &str| -> Option<String> {
            let view = BodyView::build(Bytes::from(bytes), Some(content_type.into()), false, HtmlView::default(), None);
            match &view.kind {
                BodyKind::Text(lines) => Some(lines.line(0).0.to_string()),
                _ => None,
            }
        };

        // Declared.
        let latin1 = encoding_rs::WINDOWS_1252.encode("Café crème").0.into_owned();
        assert_eq!(first_line(latin1.clone(), "text/plain; charset=iso-8859-1").as_deref(), Some("Café crème"));
        // Undeclared, but a textual type: the Encoding Standard's legacy default.
        assert_eq!(first_line(latin1.clone(), "text/plain").as_deref(), Some("Café crème"));
        // A binary type is never guessed at.
        assert!(first_line(latin1, "application/octet-stream").is_none(), "still a hex dump");
        // Valid UTF-8 is kept as UTF-8 even under a Latin-1 label — decoding it would make `Ã©`.
        assert_eq!(first_line("Café".as_bytes().to_vec(), "text/plain; charset=latin1").as_deref(), Some("Café"));

        // UTF-16 JSON, found by its byte-order mark, parses as JSON.
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("{\"ok\":true}".encode_utf16().flat_map(u16::to_le_bytes));
        let view = BodyView::build(Bytes::from(utf16), Some("application/json".into()), false, HtmlView::default(), None);
        assert!(view.outline().is_some(), "UTF-16 JSON should open as a tree");
    }

    #[test]
    fn a_shallow_long_row_beats_a_deep_short_one() {
        // The ratio between the per-depth indent and the per-character advance is the only
        // thing `widest_json_row` decides with, and this is the shape that exposes it: the
        // first row has ten more characters, the second sits six levels deeper. At the real
        // advance the shallow row is wider; at the 7.0 this shipped with, the deep one wins and
        // the scroll region is sized to the wrong row — so the end of the long value can never
        // be reached, which is exactly what it felt like.
        let long = "x".repeat(41);
        let deep = "y".repeat(31);
        let json = format!(r#"{{"k":"{long}","a":{{"b":{{"c":{{"d":{{"e":{{"f":{{"d":"{deep}"}}}}}}}}}}}}}}"#);
        let view = BodyView::build(Bytes::from(json), Some("application/json".into()), false, HtmlView::default(), None);

        let outline = view.outline().expect("json");
        let row = outline.row(view.widest_visible_ix()).expect("a row");
        assert_eq!(
            outline.text(row.key),
            "\"k\"",
            "the shallow row with the long value is the widest one actually drawn"
        );
        assert_eq!(row.depth, 1);
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

    /// Renamed from `a_non_utf8_body_is_binary`, which asserted `row_count() == 0` — the old
    /// behaviour, where a binary response was a dead end showing a single sentence. It now has
    /// rows like any other body, which is what makes search, selection and copy work on it.
    #[test]
    fn a_non_utf8_body_becomes_a_hex_dump() {
        let view = BodyView::build(
            Bytes::from_static(&[0xff, 0xfe, 0x00]),
            None,
            false,
            HtmlView::default(),
            None,
        );

        let BodyKind::Hex(lines) = &view.kind else {
            panic!("expected a hex dump")
        };
        assert_eq!(lines.len(), 1);
        assert!(lines.line(0).0.contains("ff fe 00"), "{:?}", lines.line(0).0);
        assert_eq!(view.row_count(), 1, "the dump is rows, not a dead end");
        assert!(view.notice.is_none(), "three bytes is not a truncation");
        assert!(!view.is_json() && !view.raw_is_json(), "hex must never be JSON-lexed");
    }

    #[test]
    fn an_empty_body_is_empty() {
        let view = build("", Some("application/json"));
        assert!(matches!(view.kind, BodyKind::Empty));
    }

    #[test]
    fn an_oversized_body_is_not_parsed_but_is_still_shown() {
        let big = format!("[{}]", "1,".repeat(MAX_AUTO_PARSE / 2 + 8));
        let view = BodyView::build(Bytes::from(big.clone()), Some("application/json".into()), false, HtmlView::default(), None);

        assert!(!view.is_json(), "over the cap, JSON must not be parsed");
        assert!(matches!(view.notice, Some(BodyNotice::TooLarge { .. })));
        assert!(view.row_count() > 0, "raw view must still render");

        // ...and forcing it parses. (Malformed here, since the generator leaves a
        // trailing comma — what matters is that force bypasses the cap.)
        let forced = BodyView::build(Bytes::from(big), Some("application/json".into()), true, HtmlView::default(), None);
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
