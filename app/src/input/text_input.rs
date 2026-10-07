//! A single-line text input.
//!
//! Adapted from gpui 0.2.2's `examples/input.rs`, with five deliberate changes:
//!
//! 1. **Colors come from the theme**, not hardcoded blue/grey literals, so inputs
//!    follow light/dark like everything else.
//! 2. **Text style is inherited**, not set here. The parent div decides font and
//!    size; `window.text_style()` picks it up during prepaint. That's what makes one
//!    `TextInput` serve both the URL bar and the tiny header cells.
//! 3. **A caller-supplied key context identifier.** GPUI's context predicates match
//!    only the *leaf* context (`Identifier(name) => contexts.last().contains(name)`),
//!    so wrapping an input in an outer `key_context` div would not let a binding
//!    target it. Instead both identifiers go in one context — `"TextInput UrlBar"` —
//!    because `KeyContext::parse` accepts whitespace-separated identifiers.
//! 4. **The single-line invariant is enforced at the edit boundary.** `shape_line`
//!    carries `debug_assert!(text.find('\n').is_none())`; the example only sanitizes
//!    its paste path, which leaves IME and drop paths able to trip it.
//! 5. **`character_index_for_point` no longer asserts.** The example's
//!    `assert_eq!(last_layout.text, self.content)` panics whenever the placeholder is
//!    showing, because an empty input lays out placeholder text instead of content.
//! 6. **The composed-selection offset uses `range.start` for both ends.** The example adds
//!    `range.end` to the end, which overshoots by the width of whatever was replaced — invisible
//!    at an insertion point, a panic in `copy` once a non-empty range is replaced mid-composition.
//!
//! Keybindings live in `main.rs` under the `TextInput` context — including the
//! `ctrl-` translations, since the upstream example ships macOS `cmd-` bindings that
//! never fire on Linux.

use std::borrow::Cow;
use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::ActiveTheme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordLeft,
        DeleteWordRight,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        Undo,
        Redo,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Cut,
        Copy,
        // `{{` completion. Bound only under `VarComplete`, which an input adds to its own key
        // context while its list is open — so these never shadow an arrow or Enter otherwise.
        VarCompleteNext,
        VarCompletePrev,
        VarCompleteAccept,
        VarCompleteDismiss,
    ]
);

/// Emitted whenever the content changes, and only then — not on cursor or selection moves.
///
/// Carries no payload: a subscriber that wants the text reads it back off the entity, the same
/// way `RequestView::spec` reads its inputs rather than mirroring them. Putting the string in
/// the event would create a second copy that can disagree with the first.
///
/// This exists because the alternative is polling. `Picker` used to notice typing by keeping
/// the query it last ranked and comparing it in `render`, with a comment saying it did so only
/// because `TextInput` emitted nothing — which is a mirror of state the input already owns, and
/// the convention here is to derive rather than mirror. It also does not generalise: response
/// search has to *spawn a background task* when the query changes, and starting one from
/// `render` on a string compare is a much worse shape than reacting to an event.
pub struct Changed;

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    history: crate::input::History,
    placeholder: SharedString,
    /// Extra key context identifiers, e.g. `"UrlBar"`. Always joined with
    /// `TextInput` so the shared editing bindings apply too.
    key_context: SharedString,

    selected_range: Range<usize>,
    selection_reversed: bool,
    /// The IME pre-edit region, underlined while composing.
    marked_range: Option<Range<usize>>,

    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// How far the text is shifted left, in pixels.
    ///
    /// A single-line input can't wrap and can't grow, so text longer than the box has to
    /// scroll. Without this the overflow clip hides the end of a long URL with no way to
    /// reach it. Recomputed each prepaint to keep the cursor in view, which is what makes
    /// it feel like an input rather than a viewport you have to drive.
    scroll_offset: Pixels,
    is_selecting: bool,
    /// Draw `*` in place of the content — a password field.
    ///
    /// **One `*` per byte, not per character**, so every byte offset into the display text is
    /// the same offset into the content and the cursor, selection and click mapping need no
    /// translation. The cost is that a multi-byte character shows as two to four stars.
    masked: bool,
    /// Whether `{{variables}}` are coloured and explained on hover — only for fields whose text
    /// is sent, where a variable means something. A find bar or a rename box is not one.
    variables: bool,
    /// The `{{variable}}` under the pointer, by byte range, while it is there.
    hovered_variable: Option<Range<usize>>,
    /// The completion list's highlighted row, keyed by where the name being completed begins so
    /// a new `{{` starts from the top. The list itself is derived from the text and cursor.
    var_highlight: Option<(usize, usize)>,
    /// Where `Escape` closed the list — see `variables::completion`.
    var_dismissed: Option<usize>,
}

/// The inputs whose text reaches the wire and so is substituted at send time.
const SENT_FIELDS: &[&str] = &["UrlBar", "HeaderCell", "QueryCell", "FormCell", "PartCell", "AuthField"];

impl TextInput {
    pub fn new(
        text: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        extra_context: &str,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = sanitize(text.into());
        let cursor = content.len();

        Self {
            // tab_index stays at the default 0 so inputs sort among themselves by
            // paint order — `TabStopNode` orders by tab_index path first, then
            // insertion index, which makes visual order the tab order for free. The
            // body and response panes take 1 and 2 to land after every input.
            focus_handle: cx.focus_handle().tab_stop(true),
            content,
            history: crate::input::History::default(),
            placeholder: sanitize(placeholder.into()),
            key_context: SharedString::from(format!("TextInput {extra_context}")),
            selected_range: cursor..cursor,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_offset: px(0.),
            is_selecting: false,
            masked: false,
            variables: SENT_FIELDS.contains(&extra_context),
            hovered_variable: None,
            var_highlight: None,
            var_dismissed: None,
        }
    }

    /// The `{{` completion as things stand — only while this input has focus and nothing is
    /// selected, since a list under a field you are not typing in is noise.
    pub fn var_completion(&self, window: &Window, cx: &App) -> Option<crate::variables::Completion> {
        if !self.variables
            || self.masked
            || !self.selected_range.is_empty()
            || !self.focus_handle.is_focused(window)
        {
            return None;
        }
        crate::variables::completion(&self.content, self.cursor_offset(), self.var_dismissed, cx)
    }

    fn var_highlighted(&self, completion: &crate::variables::Completion) -> usize {
        match self.var_highlight {
            Some((start, ix)) if start == completion.replace.start => {
                ix.min(completion.items.len() - 1)
            }
            _ => 0,
        }
    }

    fn var_step(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(completion) = self.var_completion(window, cx) else { return };
        let len = completion.items.len() as isize;
        let next = (self.var_highlighted(&completion) as isize + delta).rem_euclid(len) as usize;
        self.var_highlight = Some((completion.replace.start, next));
        cx.notify();
    }

    fn var_complete_next(&mut self, _: &VarCompleteNext, window: &mut Window, cx: &mut Context<Self>) {
        self.var_step(1, window, cx);
    }

    fn var_complete_prev(&mut self, _: &VarCompletePrev, window: &mut Window, cx: &mut Context<Self>) {
        self.var_step(-1, window, cx);
    }

    fn var_complete_accept(
        &mut self,
        _: &VarCompleteAccept,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(completion) = self.var_completion(window, cx) else { return };
        let ix = self.var_highlighted(&completion);
        self.accept_variable(&completion, ix, window, cx);
    }

    fn var_complete_dismiss(
        &mut self,
        _: &VarCompleteDismiss,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(completion) = self.var_completion(window, cx) {
            self.var_dismissed = Some(completion.replace.start);
            cx.notify();
        }
    }

    /// Write the chosen name through the ordinary edit path, so `Ctrl+Z` undoes it, and leave the
    /// cursor after the closing braces — the next thing typed belongs outside the placeholder.
    fn accept_variable(
        &mut self,
        completion: &crate::variables::Completion,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = completion.items.get(ix) else { return };
        let text = crate::variables::inserted(name, completion.closed);
        self.selected_range = completion.replace.clone();
        self.replace_text_in_range(None, &text, window, cx);
        let after = completion.replace.start + name.len() + 2;
        if after <= self.content.len() {
            self.selected_range = after..after;
        }
        self.var_highlight = None;
        cx.notify();
    }

    /// The variable under the pointer, as `(name, byte range)`. Read by tests, and by the popover.
    pub fn hovered_variable(&self) -> Option<(&str, Range<usize>)> {
        let range = self.hovered_variable.clone()?;
        zuno_core::environment::variable_spans(&self.content)
            .into_iter()
            .find(|(span, _)| *span == range)
            .map(|(span, name)| (name, span))
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        if self.masked != masked {
            self.masked = masked;
            cx.notify();
        }
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = sanitize(placeholder.into());
        cx.notify();
    }

    /// Where this input last painted, in **window** coordinates.
    ///
    /// Already recorded for hit-testing and the IME rectangle; exposed so an overlay can be
    /// anchored under the box. It is one frame behind, which is the rule for anything read off
    /// a painted element — harmless here, because a cell does not move while you type into it,
    /// unlike the drag case where the same staleness produced visible flicker.
    pub fn last_bounds(&self) -> Option<Bounds<Pixels>> {
        self.last_bounds
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    // ---- movement -----------------------------------------------------------

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    /// Word-level movement collapses a selection to its edge first, the same way `left`/`right`
    /// do — jumping a word from the far end of a selection you just made is not what the
    /// keystroke means.
    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selected_range.is_empty() {
            self.cursor_offset()
        } else {
            self.selected_range.start
        };
        self.move_to(crate::input::prev_word_boundary(&self.content, from), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selected_range.is_empty() {
            self.cursor_offset()
        } else {
            self.selected_range.end
        };
        self.move_to(crate::input::next_word_boundary(&self.content, from), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let to = crate::input::prev_word_boundary(&self.content, self.cursor_offset());
        self.select_to(to, cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let to = crate::input::next_word_boundary(&self.content, self.cursor_offset());
        self.select_to(to, cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let to = crate::input::prev_word_boundary(&self.content, self.cursor_offset());
            self.selected_range = to..self.selected_range.end;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let to = crate::input::next_word_boundary(&self.content, self.cursor_offset());
            self.selected_range = self.selected_range.start..to;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// A single-line input has one line, so these are `Home`/`End` again. Bound anyway: the
    /// keystroke has to mean the same thing everywhere text is edited, and `Ctrl+Home` doing
    /// nothing in the URL bar while working in the body would read as a bug.
    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.history.undo(self.snapshot()) {
            self.restore(previous, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.history.redo(self.snapshot()) {
            self.restore(next, cx);
        }
    }

    fn snapshot(&self) -> crate::input::EditSnapshot {
        crate::input::EditSnapshot {
            content: self.content.to_string(),
            selection: self.selected_range.clone(),
            reversed: self.selection_reversed,
        }
    }

    /// Put a snapshot back, emitting `Changed` like any other edit — a subscriber such as the
    /// picker's re-rank or the find bar's re-scan has to see an undo as the content change it is.
    fn restore(&mut self, snapshot: crate::input::EditSnapshot, cx: &mut Context<Self>) {
        self.content = SharedString::from(snapshot.content);
        self.selected_range = snapshot.selection;
        self.selection_reversed = snapshot.reversed;
        self.marked_range = None;
        cx.emit(Changed);
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    /// Select everything, without going through the action.
    ///
    /// Public for the one caller that isn't a keystroke: reopening the find bar selects the
    /// existing query so typing replaces it. Dispatching `SelectAll` there would resolve
    /// against whatever holds focus at that moment, which is the input we are *about* to
    /// focus — a race the direct call doesn't have.
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    // ---- editing ------------------------------------------------------------

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // A hidden password is not copied out from under its mask; show it first. Cut still
        // deletes — it just does not hand the text to the clipboard either.
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            if !self.masked {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    self.content[self.selected_range.clone()].to_string(),
                ));
            }
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    // ---- mouse --------------------------------------------------------------

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A single line has no third level, so triple-click takes everything — which is also
        // what double-click yields on a one-word input.
        if event.click_count >= 3 {
            self.select_all_text(cx);
            return;
        }
        if event.click_count == 2 {
            let offset = self.index_for_mouse_position(event.position);
            self.selected_range = crate::input::word_at(&self.content, offset);
            self.selection_reversed = false;
            self.history.break_run();
            cx.notify();
            return;
        }

        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
        self.hover_variable_at(event.position, cx);
    }

    /// Track which `{{variable}}` the pointer is over. Per move rather than once per hover —
    /// the reason this is not a gpui `tooltip`, which is built once when it appears and so
    /// would keep naming the first variable after the pointer slid onto the next.
    fn hover_variable_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.variables || self.masked || self.is_selecting {
            return self.set_hovered_variable(None, cx);
        }
        // The glyph *under* the pointer, not the nearest caret gap: `index_for_mouse_position`
        // rounds, which would light up a variable from the half-character beside it.
        let index = self
            .last_layout
            .as_ref()
            .zip(self.last_bounds.as_ref())
            .filter(|(_, bounds)| bounds.contains(&position))
            .and_then(|(line, bounds)| {
                line.index_for_x(position.x - bounds.left() + self.scroll_offset)
            });
        let span = index.and_then(|index| {
            zuno_core::environment::variable_spans(&self.content)
                .into_iter()
                .map(|(range, _)| range)
                .find(|range| range.start <= index && index < range.end)
        });
        self.set_hovered_variable(span, cx);
    }

    fn set_hovered_variable(&mut self, span: Option<Range<usize>>, cx: &mut Context<Self>) {
        if self.hovered_variable != span {
            self.hovered_variable = span;
            cx.notify();
        }
    }

    // ---- internals ----------------------------------------------------------

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        // Typing, arrowing away, then typing again is two edits to a person; leaving the run
        // open would make one Ctrl+Z undo both.
        self.history.break_run();
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left() + self.scroll_offset)
    }

    /// Graphemes, not chars — so an emoji or a combining sequence moves as one
    /// unit rather than leaving the cursor mid-codepoint.
    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

/// Collapse newlines to spaces. `TextSystem::shape_line` carries a
/// `debug_assert!` against embedded newlines, and a single-line input has no way
/// to render them anyway. Borrowed in the common case so ordinary typing doesn't
/// allocate.
fn sanitize(text: SharedString) -> SharedString {
    if text.contains('\n') || text.contains('\r') {
        SharedString::from(text.replace(['\n', '\r'], " "))
    } else {
        text
    }
}

fn sanitize_str(text: &str) -> Cow<'_, str> {
    if text.contains('\n') || text.contains('\r') {
        Cow::Owned(text.replace(['\n', '\r'], " "))
    } else {
        Cow::Borrowed(text)
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Every insertion path funnels through here — typing, IME commit, paste,
        // and drop — so this is the one place the single-line invariant holds.
        let new_text = sanitize_str(new_text);

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        // Coalescable only for a plain single-character insertion that replaced nothing and
        // carries no newline. A paste, a deletion, or replacing a selection each begin their own
        // undo entry — see `input::History`.
        // Deliberately *not* requiring an empty range: replacing a selection with a typed
        // character opens a run too, so select-all-then-type undoes in one press instead of
        // leaving the first character behind as its own entry.
        let typed_one_char = new_text.chars().count() == 1 && !new_text.contains('\n');
        self.history.record(
            self.snapshot(),
            typed_one_char.then_some(range.start),
            new_text.len(),
        );

        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.marked_range.take();
        // Both content-mutating methods emit, and between them they are every edit: backspace,
        // delete, paste and cut all call *this* one, and the IME composition path calls the
        // other. Anything that only moves the cursor or the selection deliberately does not.
        cx.emit(Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_text = sanitize_str(new_text);

        // **Recorded only as the composition opens.** This is called on every keystroke while
        // an IME candidate is being edited, so recording each call would bury the history under
        // intermediate states nobody typed deliberately. With `marked_range` already set the
        // composition is in progress and its opening snapshot is on the stack, so one Ctrl+Z
        // after the commit steps back past the whole thing.
        if self.marked_range.is_none() {
            self.history.record(self.snapshot(), None, 0);
        }

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        self.marked_range = if new_text.is_empty() {
            None
        } else {
            Some(range.start..range.start + new_text.len())
        };
        // **Both ends shift by `range.start`.** `new_selected_range_utf16` is relative to the text
        // just inserted, so the old range's *end* has nothing to do with it — and adding it
        // produced a selection longer than the content whenever the replaced range was non-empty,
        // which `copy` and `cut` then slice with and panic on. Harmless only while
        // `range.start == range.end`, which is the ordinary insertion-point case and why this sat
        // unnoticed. `editor.rs` already had it right; this is a sixth deliberate divergence from
        // gpui's `examples/input.rs`, which is where the `range.end` came from.
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| {
                let cursor = range.start + new_text.len();
                cursor..cursor
            });

        cx.emit(Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start) - self.scroll_offset,
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end) - self.scroll_offset,
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;

        // The upstream example asserts `last_layout.text == self.content` here. That
        // panics whenever the placeholder is showing, since an empty input lays out
        // placeholder text. Bailing out is the correct response: there is no
        // character under the point.
        // By length rather than by text, so a masked field — laid out as `*`s of the same byte
        // length — still answers.
        if self.content.is_empty() || last_layout.text.len() != self.content.len() {
            return None;
        }

        let utf8_index = last_layout.index_for_x(point.x - line_point.x + self.scroll_offset)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

impl gpui::EventEmitter<Changed> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let completion = self.var_completion(window, cx);
        // `VarComplete` joins the leaf context only while the list is open, which is what lets
        // its bindings win then and stay out of the way otherwise.
        let key_context = match completion {
            Some(_) => format!("{} VarComplete", self.key_context),
            None => self.key_context.to_string(),
        };
        div()
            .key_context(key_context.as_str())
            .on_action(cx.listener(Self::var_complete_next))
            .on_action(cx.listener(Self::var_complete_prev))
            .on_action(cx.listener(Self::var_complete_accept))
            .on_action(cx.listener(Self::var_complete_dismiss))
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            // An id is what `on_hover` needs, and it is safe to repeat: a view scopes the ids
            // inside it by its own entity.
            .id("text-input")
            // `on_mouse_move` only fires while the pointer is inside, so leaving needs this or
            // the popover would stay up after the pointer had gone.
            .on_hover(cx.listener(|input, hovered: &bool, _, cx| {
                if !*hovered {
                    input.set_hovered_variable(None, cx);
                }
            }))
            .w_full()
            .child(TextElement {
                input: cx.entity(),
            })
            .children(self.variable_popover(cx))
            .children(completion.and_then(|completion| self.completion_list(completion, cx)))
    }
}

impl TextInput {
    /// What the hovered `{{variable}}` resolves to, under its first brace. Positioned from
    /// `last_bounds`, a frame behind — harmless for the reason the header suggestions give: the
    /// text does not move while the pointer rests on it.
    fn variable_popover(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (name, range) = self.hovered_variable()?;
        let (line, bounds) = self.last_layout.as_ref().zip(self.last_bounds.as_ref())?;
        let at = point(
            bounds.left() + line.x_for_index(range.start) - self.scroll_offset,
            bounds.bottom() + px(4.),
        );
        Some(crate::variables::popover(name, at, cx))
    }

    /// The `{{` completion list, under the opening braces.
    fn completion_list(
        &self,
        completion: crate::variables::Completion,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let (line, bounds) = self.last_layout.as_ref().zip(self.last_bounds.as_ref())?;
        let braces = completion.replace.start.saturating_sub(2);
        let at = point(
            bounds.left() + line.x_for_index(braces) - self.scroll_offset,
            bounds.bottom() + px(4.),
        );
        let highlighted = self.var_highlighted(&completion);
        let items = completion.items.clone();
        Some(crate::variables::completion_list(
            at,
            &items,
            highlighted,
            cx,
            move |input: &mut TextInput, ix, window, cx| {
                input.accept_variable(&completion, ix, window, cx)
            },
        ))
    }
}

/// The element that actually shapes and paints the line. It has to be a custom
/// `Element` rather than styled divs because it needs the shaped line's glyph
/// positions to place the cursor and selection, and it needs `paint` in order to
/// register the platform input handler.
struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    scroll_offset: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = cx.theme().clone();
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor_offset = input.cursor_offset();
        let marked_range = input.marked_range.clone();
        let previous_scroll_offset = input.scroll_offset;

        // Font and size are inherited from the parent div, which is what lets one
        // TextInput serve both the URL bar and the small table cells.
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), theme.text_muted)
        } else if input.masked {
            (SharedString::from("*".repeat(content.len())), style.color)
        } else {
            (content, style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // `{{variables}}`, coloured by whether they resolve — green or red, the convention every
        // API client shares. Not on a masked field (the stars would give away where one is) and
        // not over the placeholder text.
        let variables: Vec<(Range<usize>, gpui::Hsla)> =
            if input.variables && !input.masked && !input.content.is_empty() {
                zuno_core::environment::variable_spans(&input.content)
                    .into_iter()
                    .map(|(range, name)| {
                        let colour = match crate::auth::is_defined(name, cx) {
                            Some(true) => theme.status_success,
                            Some(false) => theme.status_server_error,
                            None => theme.accent,
                        };
                        (range, colour)
                    })
                    .collect()
            } else {
                Vec::new()
            };
        let marked = marked_range.filter(|_| !display_text.is_empty());

        // `ui::split_spans` cuts at every boundary, since a pre-edit region can start inside a
        // variable; while composing (IME), the pre-edit region is underlined.
        let runs: Vec<TextRun> = crate::ui::split_spans(display_text.len(), &variables, marked)
            .into_iter()
            .map(|(range, colour, composing)| {
                let color = colour.unwrap_or(run.color);
                TextRun {
                    len: range.len(),
                    color,
                    underline: composing.then_some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                }
            })
            .collect();
        // An empty display (no content, no placeholder) still gets the one run it always had.
        let runs = if runs.is_empty() { vec![run] } else { runs };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        // Scroll just enough to keep the cursor inside the box, then clamp so the text
        // never scrolls past its end or leaves a gap when it fits.
        let cursor_x = line.x_for_index(cursor_offset);
        let visible = bounds.size.width;
        let caret = px(2.);

        let mut scroll_offset = previous_scroll_offset;
        if cursor_x - scroll_offset > visible - caret {
            scroll_offset = cursor_x - visible + caret;
        }
        if cursor_x - scroll_offset < px(0.) {
            scroll_offset = cursor_x;
        }
        let max_offset = (line.width - visible + caret).max(px(0.));
        scroll_offset = scroll_offset.max(px(0.)).min(max_offset);

        let origin_x = bounds.left() - scroll_offset;

        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(origin_x + cursor_x, bounds.top()),
                        size(px(1.5), bounds.bottom() - bounds.top()),
                    ),
                    theme.cursor,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            origin_x + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            origin_x + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    theme.selection,
                )),
                None,
            )
        };

        PrepaintState {
            line: Some(line),
            cursor,
            selection,
            scroll_offset,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();

        // Registering the platform input handler is what routes real keystrokes and
        // IME composition into `EntityInputHandler`.
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        let Some(line) = prepaint.line.take() else {
            return;
        };
        // Ignore rather than unwrap: a failed glyph paint should not take the window
        // down mid-frame.
        // Shifted by the scroll offset; the parent's `overflow_hidden` provides the clip
        // that keeps the overflow from painting over its neighbours.
        line.paint(
            point(bounds.left() - prepaint.scroll_offset, bounds.top()),
            window.line_height(),
            window,
            cx,
        )
        .ok();

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll_offset = prepaint.scroll_offset;
        });
    }
}
