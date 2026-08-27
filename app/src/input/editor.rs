//! A multi-line text editor for the request body.
//!
//! Shares `TextInput`'s model — byte offsets into a `String`, plus the same
//! `EntityInputHandler` contract — and adds line awareness: vertical movement, per-line
//! Home/End, newline insertion, and selection that spans lines.
//!
//! ### Why not a rope
//!
//! architecture.md §7 called for `ropey`. Two things changed that: its current release is
//! `2.0.0-beta.1` (so a stable requirement wouldn't even resolve to it), and the benefit
//! is unmeasurable at the sizes this editor sees. Request bodies are hand-authored — a
//! 100KB body means the line index rescan is a ~10µs `memchr` sweep per keystroke,
//! against a 16ms frame budget. Keeping one text model in the codebase is worth more than
//! an O(log n) edit we can't feel. Revisit when bodies routinely exceed ~1MB, or when
//! in-buffer undo history arrives and needs cheap snapshots.
//!
//! ### Rendering
//!
//! Only the lines intersecting the viewport are shaped, using the scroll handle's offset.
//! Pasting a 50,000-line body should cost a scroll-region shape, not 50,000 of them.
//!
//! Soft-wrap is off (§7). Long lines scroll horizontally instead of wrapping, with the
//! offset following the cursor — clamped against the *cursor's* line width rather than the
//! widest visible line, which would jitter as you scroll vertically. That avoids having to
//! measure every line just to know how far right the content goes.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollHandle, ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
    actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::input::text_input;
use crate::theme::ActiveTheme;

actions!(
    editor,
    [Up, Down, SelectUp, SelectDown, Newline, PageUp, PageDown, SelectPageUp, SelectPageDown]
);

pub struct Editor {
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    content: String,
    history: crate::input::History,
    /// Byte offset where each line starts. Always contains at least `0`.
    line_starts: Vec<usize>,
    placeholder: SharedString,

    selection: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,

    /// Shaped layouts from the last paint, keyed by line index — only the visible
    /// window, which is all hit-testing needs.
    last_layouts: Vec<(usize, ShapedLine)>,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Pixels,
    /// Horizontal scroll, in pixels. Vertical scrolling is the container's job; this is
    /// ours, because soft-wrap is off and long lines otherwise run off the edge with no
    /// way to reach them.
    h_offset: Pixels,
    is_selecting: bool,
}

impl Editor {
    pub fn new(text: impl Into<String>, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let content: String = text.into();
        let line_starts = compute_line_starts(&content);

        Self {
            focus_handle: cx.focus_handle().tab_stop(true),
            scroll: ScrollHandle::new(),
            content,
            line_starts,
            history: crate::input::History::default(),
            placeholder: placeholder.into(),
            selection: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layouts: Vec::new(),
            last_bounds: None,
            last_line_height: px(16.),
            h_offset: px(0.),
            is_selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }


    // ---- line geometry ------------------------------------------------------

    /// Index of the line containing `offset`.
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next.saturating_sub(1),
        }
    }

    fn line_start(&self, line: usize) -> usize {
        self.line_starts
            .get(line)
            .copied()
            .unwrap_or(self.content.len())
    }

    /// End of a line's text, excluding its newline.
    fn line_end(&self, line: usize) -> usize {
        match self.line_starts.get(line + 1) {
            Some(next_start) => {
                // Step back over the \n, and a \r if the content has CRLF endings.
                let mut end = next_start - 1;
                if end > self.line_start(line) && self.content.as_bytes()[end - 1] == b'\r' {
                    end -= 1;
                }
                end
            }
            None => self.content.len(),
        }
    }

    fn line_text(&self, line: usize) -> &str {
        let range = self.line_start(line)..self.line_end(line);
        self.content.get(range).unwrap_or("")
    }

    fn cursor(&self) -> usize {
        if self.selection_reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    /// Move vertically, preserving the byte column where the target line allows it.
    ///
    /// Byte columns rather than pixel columns: the body editor is monospaced, so they
    /// agree, and byte columns don't need a shaped layout for an off-screen line.
    fn move_vertically(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let line = self.line_of(cursor);
        let column = cursor - self.line_start(line);

        let target = line as isize + delta;
        if target < 0 || target as usize >= self.line_count() {
            // Already at the edge: fall through to the document boundary, which is what
            // every editor does.
            let offset = if delta < 0 { 0 } else { self.content.len() };
            if extend {
                self.select_to(offset, cx);
            } else {
                self.move_to(offset, cx);
            }
            return;
        }

        let target = target as usize;
        let start = self.line_start(target);
        let end = self.line_end(target);
        let mut offset = (start + column).min(end);
        // Landing mid-character is possible when lines differ in encoding width.
        while offset > start && !self.content.is_char_boundary(offset) {
            offset -= 1;
        }

        if extend {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    // ---- actions ------------------------------------------------------------

    fn left(&mut self, _: &text_input::Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.move_to(self.prev_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selection.start, cx);
        }
    }

    fn right(&mut self, _: &text_input::Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.move_to(self.next_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selection.end, cx);
        }
    }

    /// Word movement shares `input::prev_word_boundary` with `TextInput` rather than
    /// reimplementing it — two definitions of "a word" would drift, and the body editor and the
    /// URL bar behaving differently on the same keystroke is exactly the kind of thing nobody
    /// notices until it is annoying.
    fn word_left(&mut self, _: &text_input::WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selection.is_empty() {
            self.cursor()
        } else {
            self.selection.start
        };
        self.move_to(crate::input::prev_word_boundary(&self.content, from), cx);
    }

    fn word_right(&mut self, _: &text_input::WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selection.is_empty() {
            self.cursor()
        } else {
            self.selection.end
        };
        self.move_to(crate::input::next_word_boundary(&self.content, from), cx);
    }

    fn select_word_left(
        &mut self,
        _: &text_input::SelectWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let to = crate::input::prev_word_boundary(&self.content, self.cursor());
        self.select_to(to, cx);
    }

    fn select_word_right(
        &mut self,
        _: &text_input::SelectWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let to = crate::input::next_word_boundary(&self.content, self.cursor());
        self.select_to(to, cx);
    }

    /// A page is however many whole lines the viewport last showed, minus one so a landmark on
    /// the edge stays visible across the jump. Falls back to a single line before the first
    /// paint, when there is no measured height to divide.
    fn page_lines(&self) -> isize {
        let Some(bounds) = self.last_bounds else { return 1 };
        let line_height = self.last_line_height.max(px(1.));
        let lines = (bounds.size.height / line_height).floor() as isize;
        (lines - 1).max(1)
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let lines = self.page_lines();
        self.move_vertically(-lines, false, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let lines = self.page_lines();
        self.move_vertically(lines, false, cx);
    }

    fn select_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        let lines = self.page_lines();
        self.move_vertically(-lines, true, cx);
    }

    fn select_page_down(&mut self, _: &SelectPageDown, _: &mut Window, cx: &mut Context<Self>) {
        let lines = self.page_lines();
        self.move_vertically(lines, true, cx);
    }

    fn delete_word_left(
        &mut self,
        _: &text_input::DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection.is_empty() {
            let to = crate::input::prev_word_boundary(&self.content, self.cursor());
            self.selection = to..self.selection.end;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &text_input::DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection.is_empty() {
            let to = crate::input::next_word_boundary(&self.content, self.cursor());
            self.selection = self.selection.start..to;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// Unlike `Home`/`End`, which are per-line here, these are the document ends — the
    /// distinction that makes an editor an editor.
    fn doc_start(&mut self, _: &text_input::DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn doc_end(&mut self, _: &text_input::DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_doc_start(
        &mut self,
        _: &text_input::SelectDocStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(0, cx);
    }

    fn select_doc_end(
        &mut self,
        _: &text_input::SelectDocEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.content.len(), cx);
    }

    fn undo(&mut self, _: &text_input::Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.history.undo(self.snapshot()) {
            self.restore(previous, cx);
        }
    }

    fn redo(&mut self, _: &text_input::Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.history.redo(self.snapshot()) {
            self.restore(next, cx);
        }
    }

    fn snapshot(&self) -> crate::input::EditSnapshot {
        crate::input::EditSnapshot {
            content: self.content.clone(),
            selection: self.selection.clone(),
            reversed: self.selection_reversed,
        }
    }

    /// **The line index has to be rebuilt here.** It is derived from the content, so restoring
    /// text without it leaves every offset lookup reading stale line starts — which is a panic
    /// waiting in `line_of`, not a cosmetic problem.
    fn restore(&mut self, snapshot: crate::input::EditSnapshot, cx: &mut Context<Self>) {
        self.content = snapshot.content;
        self.line_starts = compute_line_starts(&self.content);
        self.selection = snapshot.selection;
        self.selection_reversed = snapshot.reversed;
        self.marked_range = None;
        cx.notify();
    }

    fn select_left(&mut self, _: &text_input::SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.prev_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &text_input::SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, false, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, true, cx);
    }

    /// Home and End are per-line here, not per-document — the difference between an
    /// editor and a single-line input.
    fn home(&mut self, _: &text_input::Home, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_of(self.cursor());
        self.move_to(self.line_start(line), cx);
    }

    fn end(&mut self, _: &text_input::End, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_of(self.cursor());
        self.move_to(self.line_end(line), cx);
    }

    fn select_home(&mut self, _: &text_input::SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_of(self.cursor());
        self.select_to(self.line_start(line), cx);
    }

    fn select_end(&mut self, _: &text_input::SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_of(self.cursor());
        self.select_to(self.line_end(line), cx);
    }

    fn select_all(&mut self, _: &text_input::SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &text_input::Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.select_to(self.prev_boundary(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &text_input::Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.select_to(self.next_boundary(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// Newline, with the leading whitespace of the current line carried over. Hand-typing
    /// nested JSON without it is miserable.
    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_of(self.cursor());
        let indent: String = self
            .line_text(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();

        let insert = format!("\n{indent}");
        self.replace_text_in_range(None, &insert, window, cx);
    }

    fn paste(&mut self, _: &text_input::Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // Multi-line paste is the point of this editor, so newlines are kept.
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &text_input::Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selection.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &text_input::Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selection.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    // ---- mouse --------------------------------------------------------------

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.offset_for_position(event.position);

        // Triple-click takes the line *without* its newline: including it would make
        // triple-click-then-type join this line to the next.
        if event.click_count >= 3 {
            self.selection = crate::input::line_at(&self.content, offset);
            self.selection_reversed = false;
            self.history.break_run();
            cx.notify();
            return;
        }
        if event.click_count == 2 {
            self.selection = crate::input::word_at(&self.content, offset);
            self.selection_reversed = false;
            self.history.break_run();
            cx.notify();
            return;
        }

        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.offset_for_position(event.position);
            self.select_to(offset, cx);
        }
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return self.cursor();
        };
        let line_height = self.last_line_height.max(px(1.));

        let relative_y = (position.y - bounds.top()).max(px(0.));
        let line = ((relative_y / line_height) as usize).min(self.line_count().saturating_sub(1));

        let column = self
            .last_layouts
            .iter()
            .find(|(ix, _)| *ix == line)
            .map(|(_, layout)| {
                layout.closest_index_for_x(position.x - bounds.left() + self.h_offset)
            })
            // Off-screen line: clamp to its end rather than guessing.
            .unwrap_or_else(|| self.line_end(line) - self.line_start(line));

        (self.line_start(line) + column).min(self.line_end(line))
    }

    // ---- internals ----------------------------------------------------------

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selection = offset..offset;
        self.selection_reversed = false;
        self.history.break_run();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selection.start = offset;
        } else {
            self.selection.end = offset;
        }
        if self.selection.end < self.selection.start {
            self.selection_reversed = !self.selection_reversed;
            self.selection = self.selection.end..self.selection.start;
        }
        cx.notify();
    }

    fn prev_boundary(&self, offset: usize) -> usize {
        let mut offset = offset.saturating_sub(1);
        while offset > 0 && !self.content.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let mut offset = (offset + 1).min(self.content.len());
        while offset < self.content.len() && !self.content.is_char_boundary(offset) {
            offset += 1;
        }
        offset
    }

    fn offset_from_utf16(&self, target: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.content.chars() {
            if utf16 >= target {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, target: usize) -> usize {
        let mut utf16 = 0;
        let mut utf8 = 0;
        for ch in self.content.chars() {
            if utf8 >= target {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

/// Byte offsets where each line begins.
///
/// **A trailing newline *does* create a final empty line here** — the opposite of
/// `LineIndex`, which drops it. That difference is not an inconsistency: `LineIndex`
/// displays finished text, where a phantom blank line at the end is noise, whereas an
/// editor must let you press Enter at the end of the buffer and put the cursor on the
/// new line. Without this, "a\n" reported one line, `line_end` returned `content.len()`,
/// and the last line's text came back as "a\n" — which trips `shape_line`'s newline
/// assertion and strands the cursor on the wrong row.
fn compute_line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (ix, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(ix + 1);
        }
    }
    starts
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        self.content.get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selection),
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
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.selection.clone());

        // Deliberately *not* requiring an empty range: replacing a selection with a typed
        // character opens a run too, so select-all-then-type undoes in one press instead of
        // leaving the first character behind as its own entry.
        let typed_one_char = new_text.chars().count() == 1 && !new_text.contains('\n');
        self.history.record(
            self.snapshot(),
            typed_one_char.then_some(range.start),
            new_text.len(),
        );

        self.content
            .replace_range(range.clone(), new_text);
        // Line starts shift on every edit. A full rescan is a memchr sweep — ~10µs on a
        // 100KB body, which is what makes a rope unnecessary here.
        self.line_starts = compute_line_starts(&self.content);

        let cursor = range.start + new_text.len();
        self.selection = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
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
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.selection.clone());

        // Only as the composition opens — see the same note in `text_input`.
        if self.marked_range.is_none() {
            self.history.record(self.snapshot(), None, 0);
        }

        self.content.replace_range(range.clone(), new_text);
        self.line_starts = compute_line_starts(&self.content);

        self.marked_range = if new_text.is_empty() {
            None
        } else {
            Some(range.start..range.start + new_text.len())
        };
        self.selection = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + range.start..r.end + range.start)
            .unwrap_or_else(|| {
                let cursor = range.start + new_text.len();
                cursor..cursor
            });
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let line = self.line_of(range.start);
        let (_, layout) = self.last_layouts.iter().find(|(ix, _)| *ix == line)?;

        let line_start = self.line_start(line);
        let top = element_bounds.top() + self.last_line_height * (line as f32);

        let origin_x = element_bounds.left() - self.h_offset;
        Some(Bounds::from_corners(
            point(
                origin_x + layout.x_for_index(range.start.saturating_sub(line_start)),
                top,
            ),
            point(
                origin_x + layout.x_for_index(range.end.saturating_sub(line_start)),
                top + self.last_line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        if !bounds.contains(&point) {
            return None;
        }
        Some(self.offset_to_utf16(self.offset_for_position(point)))
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("body-editor")
            // Both identifiers: the shared text bindings match on `TextInput`, the
            // line-aware ones on `BodyEditor`. GPUI matches only the leaf context, so
            // they have to live in one string.
            .key_context("TextInput BodyEditor")
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
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(EditorElement {
                editor: cx.entity(),
            })
    }
}

// ---------------------------------------------------------------------------
// The element
// ---------------------------------------------------------------------------

struct EditorElement {
    editor: Entity<Editor>,
}

struct Prepaint {
    lines: Vec<(usize, ShapedLine)>,
    line_height: Pixels,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
    h_offset: Pixels,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

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
        let line_height = window.line_height();
        let lines = self.editor.read(cx).line_count().max(1);

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        // Full document height, so the enclosing scroll container knows how far to go.
        style.size.height = (line_height * lines as f32).into();
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
        let editor = self.editor.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        // Only shape the lines that can actually be seen. The scroll offset is negative
        // as the view moves down, so -offset.y is how far we've scrolled.
        let scroll_top = -editor.scroll.offset().y;
        let viewport = editor.scroll.bounds().size.height.max(line_height);
        let first_line = ((scroll_top / line_height).floor().max(0.0)) as usize;
        let visible = ((viewport / line_height).ceil() as usize) + 2;
        let last_line = (first_line + visible).min(editor.line_count());

        let is_empty = editor.content.is_empty();
        let previous_h_offset = editor.h_offset;
        let selection = editor.selection.clone();
        let cursor_offset = editor.cursor();
        let cursor_line = editor.line_of(cursor_offset);

        let mut lines = Vec::with_capacity(last_line.saturating_sub(first_line));
        let mut selections = Vec::new();
        let mut cursor = None;

        for line_ix in first_line..last_line {
            let text: SharedString = if is_empty && line_ix == 0 {
                editor.placeholder.clone()
            } else {
                SharedString::from(editor.line_text(line_ix).to_string())
            };
            let color = if is_empty { theme.text_muted } else { style.color };

            let line_start = editor.line_start(line_ix);
            let line_end = editor.line_end(line_ix);

            // Underline the IME pre-edit region if it falls on this line.
            let runs = match &editor.marked_range {
                Some(marked) if marked.start < line_end && marked.end > line_start && !is_empty => {
                    let local_start = marked.start.max(line_start) - line_start;
                    let local_end = (marked.end.min(line_end)) - line_start;
                    build_marked_runs(&text, &style, color, local_start, local_end)
                }
                _ => vec![TextRun {
                    len: text.len(),
                    font: style.font(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
            };

            let layout = window
                .text_system()
                .shape_line(text, font_size, &runs, None);
            lines.push((line_ix, layout));
        }

        // Horizontal scroll, driven by the cursor's line. Long lines are not wrapped
        // (§7), so without this the end of a long JSON line is unreachable — the same
        // problem the URL bar had. Clamped against the cursor line's own width rather
        // than the widest visible line, which would jitter as you scroll vertically.
        let mut h_offset = previous_h_offset;
        if let Some((_, layout)) = lines.iter().find(|(ix, _)| *ix == cursor_line) {
            let cursor_x = layout.x_for_index(cursor_offset - editor.line_start(cursor_line));
            let visible = bounds.size.width;
            let caret = px(2.);

            if cursor_x - h_offset > visible - caret {
                h_offset = cursor_x - visible + caret;
            }
            if cursor_x - h_offset < px(0.) {
                h_offset = cursor_x;
            }
            let max_offset = (layout.width - visible + caret).max(px(0.));
            h_offset = h_offset.max(px(0.)).min(max_offset);
        }

        let origin_x = bounds.left() - h_offset;

        for (line_ix, layout) in &lines {
            let line_ix = *line_ix;
            let line_start = editor.line_start(line_ix);
            let line_end = editor.line_end(line_ix);
            let top = bounds.top() + line_height * (line_ix as f32);

            // Selection segment for this line, if any.
            if !selection.is_empty() && selection.start <= line_end && selection.end >= line_start {
                let from = selection.start.max(line_start) - line_start;
                let to = selection.end.min(line_end) - line_start;
                let x_from = origin_x + layout.x_for_index(from);
                let mut x_to = origin_x + layout.x_for_index(to);

                // A selection continuing past this line should visibly cover the newline.
                if selection.end > line_end {
                    x_to += px(4.);
                }
                if x_to > x_from {
                    selections.push(fill(
                        Bounds::from_corners(point(x_from, top), point(x_to, top + line_height)),
                        theme.selection,
                    ));
                }
            }

            if selection.is_empty() && line_ix == cursor_line {
                let x = origin_x + layout.x_for_index(cursor_offset - line_start);
                cursor = Some(fill(
                    Bounds::new(point(x, top), size(px(1.5), line_height)),
                    theme.cursor,
                ));
            }
        }

        Prepaint {
            lines,
            line_height,
            cursor,
            selections,
            h_offset,
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
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        for selection in prepaint.selections.drain(..) {
            window.paint_quad(selection);
        }

        let line_height = prepaint.line_height;
        for (line_ix, layout) in &prepaint.lines {
            let origin = point(
                bounds.left() - prepaint.h_offset,
                bounds.top() + line_height * (*line_ix as f32),
            );
            layout.paint(origin, line_height, window, cx).ok();
        }

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        let lines = std::mem::take(&mut prepaint.lines);
        let h_offset = prepaint.h_offset;
        self.editor.update(cx, |editor, _| {
            editor.last_layouts = lines;
            editor.last_bounds = Some(bounds);
            editor.last_line_height = line_height;
            editor.h_offset = h_offset;
        });
    }
}

fn build_marked_runs(
    text: &SharedString,
    style: &gpui::TextStyle,
    color: gpui::Hsla,
    start: usize,
    end: usize,
) -> Vec<TextRun> {
    let base = TextRun {
        len: 0,
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };

    vec![
        TextRun {
            len: start,
            ..base.clone()
        },
        TextRun {
            len: end.saturating_sub(start),
            underline: Some(UnderlineStyle {
                color: Some(color),
                thickness: px(1.0),
                wavy: false,
            }),
            ..base.clone()
        },
        TextRun {
            len: text.len().saturating_sub(end),
            ..base
        },
    ]
    .into_iter()
    .filter(|run| run.len > 0)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::compute_line_starts;

    #[test]
    fn line_starts_for_a_single_line() {
        assert_eq!(compute_line_starts("hello"), vec![0]);
    }

    #[test]
    fn line_starts_for_multiple_lines() {
        //          0123 4567 89
        assert_eq!(compute_line_starts("abc\ndef\ngh"), vec![0, 4, 8]);
    }

    #[test]
    fn a_trailing_newline_creates_an_empty_final_line() {
        // Editors need this: after pressing Enter at the end of the buffer, the cursor
        // has to have a line to sit on. `LineIndex` deliberately does the opposite.
        assert_eq!(compute_line_starts("abc\n"), vec![0, 4]);
        assert_eq!(compute_line_starts("a\nb\n"), vec![0, 2, 4]);
    }

    #[test]
    fn blank_lines_are_real_lines() {
        assert_eq!(compute_line_starts("a\n\nb"), vec![0, 2, 3]);
    }

    #[test]
    fn empty_content_is_one_line() {
        assert_eq!(compute_line_starts(""), vec![0]);
    }
}
