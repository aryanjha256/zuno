//! Text editing primitives.
//!
//! GPUI ships the IME plumbing (`EntityInputHandler`, `ElementInputHandler`) but no
//! editor, so the single-line input here is built from scratch on top of it —
//! adapted from gpui's own `examples/input.rs`. See architecture.md §7.

use std::ops::Range;

pub mod editor;
pub mod text_input;

pub use editor::Editor;
pub use text_input::TextInput;

/// Where the caret lands on a word-level move.
///
/// **Shared by `TextInput` and `Editor` on purpose.** Both dispatch the same
/// `text_input::WordLeft`/`WordRight` actions, and two copies of this would be two chances to
/// disagree about what a word is — which the URL bar would expose immediately, since a URL is
/// mostly punctuation.
///
/// Three character classes, and the runs between them are the boundaries: whitespace,
/// word characters (alphanumeric plus `_`), and everything else. That is what makes
/// `https://api.example.com` step `https` → `://` → `api` → `.` rather than jumping the whole
/// string, which a whitespace-only rule would do.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// The previous word boundary before `offset`: skip whitespace, then the run it lands in.
///
/// Newline counts as whitespace, so this crosses lines in the editor — matching the plain
/// left/right movement, which does too.
pub fn prev_word_boundary(text: &str, offset: usize) -> usize {
    let mut iter = text[..offset].char_indices().rev().peekable();
    while let Some(&(_, c)) = iter.peek() {
        if class(c) == Class::Space {
            iter.next();
        } else {
            break;
        }
    }
    let Some(&(_, first)) = iter.peek() else {
        return 0;
    };
    let target = class(first);
    let mut boundary = 0;
    while let Some(&(i, c)) = iter.peek() {
        if class(c) == target {
            boundary = i;
            iter.next();
        } else {
            break;
        }
    }
    boundary
}

/// The next word boundary after `offset`. Lands on the *end* of the run rather than the start
/// of the following one, which is the asymmetry every code editor ships: `Ctrl+Right` stops at
/// a word's end, `Ctrl+Left` at its start.
pub fn next_word_boundary(text: &str, offset: usize) -> usize {
    let mut iter = text[offset..].char_indices().peekable();
    while let Some(&(_, c)) = iter.peek() {
        if class(c) == Class::Space {
            iter.next();
        } else {
            break;
        }
    }
    let Some(&(_, first)) = iter.peek() else {
        return text.len();
    };
    let target = class(first);
    let mut end = offset;
    while let Some(&(i, c)) = iter.peek() {
        if class(c) == target {
            end = offset + i + c.len_utf8();
            iter.next();
        } else {
            break;
        }
    }
    end
}

#[cfg(test)]
mod word_tests {
    use super::{next_word_boundary, prev_word_boundary};

    /// Walk right from 0 to the end, collecting every stop.
    fn stops_right(text: &str) -> Vec<usize> {
        let mut at = 0;
        let mut out = Vec::new();
        loop {
            let next = next_word_boundary(text, at);
            if next == at {
                return out;
            }
            at = next;
            out.push(at);
        }
    }

    fn stops_left(text: &str) -> Vec<usize> {
        let mut at = text.len();
        let mut out = Vec::new();
        loop {
            let prev = prev_word_boundary(text, at);
            if prev == at {
                return out;
            }
            at = prev;
            out.push(at);
        }
    }

    #[test]
    fn a_url_steps_through_its_punctuation() {
        // The case that matters most: a URL is mostly punctuation, so a whitespace-only rule
        // would jump the entire string in one press and be useless in the one input people
        // edit most.
        let url = "https://api.example.com/posts";
        let right: Vec<&str> = stops_right(url).iter().map(|&i| &url[..i]).collect();
        assert_eq!(
            right,
            vec![
                "https",
                "https://",
                "https://api",
                "https://api.",
                "https://api.example",
                "https://api.example.",
                "https://api.example.com",
                "https://api.example.com/",
                "https://api.example.com/posts",
            ]
        );
    }

    #[test]
    fn moving_left_lands_on_word_starts() {
        let text = "one two three";
        let left = stops_left(text);
        assert_eq!(left, vec![8, 4, 0], "starts of three, two, one: {left:?}");
    }

    #[test]
    fn whitespace_runs_are_crossed_in_one_step() {
        let text = "a    b";
        assert_eq!(next_word_boundary(text, 1), 6, "skip the gap and take `b`");
        assert_eq!(prev_word_boundary(text, 5), 0, "back over the gap to `a`");
    }

    #[test]
    fn the_ends_are_fixed_points() {
        // The movement handlers rely on this: a no-op means "already there", never a panic
        // and never a wrap to the other end.
        assert_eq!(next_word_boundary("abc", 3), 3);
        assert_eq!(prev_word_boundary("abc", 0), 0);
        assert_eq!(next_word_boundary("", 0), 0);
        assert_eq!(prev_word_boundary("", 0), 0);
        assert_eq!(next_word_boundary("   ", 0), 3, "all whitespace goes to the end");
        assert_eq!(prev_word_boundary("   ", 3), 0);
    }

    #[test]
    fn multibyte_offsets_stay_on_char_boundaries() {
        // Slicing `text[..offset]` panics on a non-boundary, so this is the guard against a
        // crash rather than a cosmetic check.
        let text = "héllo wörld";
        for &at in &stops_right(text) {
            assert!(text.is_char_boundary(at), "offset {at} split a char in {text:?}");
        }
        for &at in &stops_left(text) {
            assert!(text.is_char_boundary(at), "offset {at} split a char in {text:?}");
        }
        assert_eq!(&text[..next_word_boundary(text, 0)], "héllo");
    }

    #[test]
    fn underscores_hold_an_identifier_together() {
        // Header names and JSON keys are full of them; splitting on `_` would make word
        // movement useless for `x_request_id`.
        let text = "x_request_id: 1";
        assert_eq!(&text[..next_word_boundary(text, 0)], "x_request_id");
    }
}

/// The span of the word under `offset` — what a double-click selects.
///
/// Uses the same three classes as word movement, so double-clicking inside `api` in a URL
/// selects `api` and not the whole thing. An offset sitting in whitespace selects the
/// whitespace run, which is what leaves a double-click between words visibly doing something
/// rather than looking broken.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    // Clamp to a char boundary and prefer the character *under* the caret; at the very end
    // there is none, so fall back to the one before it.
    let offset = offset.min(text.len());
    let anchor = text[offset..]
        .chars()
        .next()
        .or_else(|| text[..offset].chars().next_back());
    let Some(anchor) = anchor else { return 0..0 };
    let target = class(anchor);

    let mut start = offset;
    for (i, c) in text[..offset].char_indices().rev() {
        if class(c) == target {
            start = i;
        } else {
            break;
        }
    }
    let mut end = offset;
    for (i, c) in text[offset..].char_indices() {
        if class(c) == target {
            end = offset + i + c.len_utf8();
        } else {
            break;
        }
    }
    start..end
}

/// The span of the line containing `offset`, excluding its trailing newline — what a
/// triple-click selects in the editor.
pub fn line_at(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |i| offset + i);
    start..end
}

/// How many entries of undo each text surface keeps.
///
/// Snapshots are whole `String`s, so this is bounded by content size rather than edit count —
/// fine for a URL or a header value, and for a hand-authored body it is the same bet §7 makes
/// about not needing a rope. A 100KB body at this depth is ~20MB worst case, which only
/// materialises if you make 200 separate edits without ever undoing.
const MAX_HISTORY: usize = 200;

/// One point in a text surface's history: the content and where the caret was.
///
/// **The selection is part of the snapshot, not an afterthought.** Undo that restores the text
/// but leaves the caret where it happened to be is disorienting — the caret should return to
/// where the edit was made, which is the only way a second undo lands somewhere predictable.
#[derive(Clone, Debug, PartialEq)]
pub struct EditSnapshot {
    pub content: String,
    pub selection: Range<usize>,
    pub reversed: bool,
}

/// Undo/redo for one text surface.
///
/// **Structural coalescing, and deliberately no clock.** A run of typed characters collapses
/// into a single entry, and the run is closed by anything that isn't a contiguous
/// single-character insertion: a deletion, a paste, a newline, or moving the caret. The
/// alternative is an idle timer, which is what most editors use and what feels most natural —
/// rejected because it puts wall-clock time in the edit path, which would make every undo test
/// depend on simulated timing. This codebase already lost six hours of CI to one timing race;
/// a deterministic rule that is 95% as good is the better trade.
///
/// One history per entity, so undoing in the URL bar cannot reach into the body.
#[derive(Default)]
pub struct History {
    past: Vec<EditSnapshot>,
    future: Vec<EditSnapshot>,
    /// Byte offset just past the last coalescable insertion, while its run is still open.
    /// `None` means the next edit starts a fresh entry.
    open_run_end: Option<usize>,
}

impl History {
    /// Record the state *before* an edit is applied.
    ///
    /// `insert_at` is `Some(offset)` only for a plain insertion that replaced nothing — the
    /// caller passes `None` for a deletion, a replacement, or anything containing a newline, and
    /// that always begins a new entry.
    pub fn record(&mut self, before: EditSnapshot, insert_at: Option<usize>, inserted_len: usize) {
        // Any new edit invalidates the redo branch: you cannot redo forward into a future that
        // this edit has just replaced.
        self.future.clear();

        let continues_run =
            matches!((self.open_run_end, insert_at), (Some(end), Some(at)) if end == at);
        if !continues_run {
            self.past.push(before);
            if self.past.len() > MAX_HISTORY {
                self.past.remove(0);
            }
        }
        self.open_run_end = insert_at.map(|at| at + inserted_len);
    }

    /// Close the current run, so the next insertion starts its own entry. Called when the caret
    /// moves: typing, arrowing away, then typing again is two edits to a person, and one entry
    /// would undo both at once.
    pub fn break_run(&mut self) {
        self.open_run_end = None;
    }

    pub fn undo(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        let previous = self.past.pop()?;
        self.future.push(current);
        self.open_run_end = None;
        Some(previous)
    }

    pub fn redo(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        let next = self.future.pop()?;
        self.past.push(current);
        self.open_run_end = None;
        Some(next)
    }
}

#[cfg(test)]
mod history_tests {
    use super::{EditSnapshot, History, line_at, word_at};

    fn snap(text: &str, at: usize) -> EditSnapshot {
        EditSnapshot {
            content: text.to_string(),
            selection: at..at,
            reversed: false,
        }
    }

    /// Type `text` one character at a time, coalescing as the entities do.
    fn type_out(history: &mut History, text: &str) {
        let mut so_far = String::new();
        for c in text.chars() {
            let at = so_far.len();
            history.record(snap(&so_far, at), Some(at), c.len_utf8());
            so_far.push(c);
        }
    }

    #[test]
    fn a_typed_run_collapses_into_one_entry() {
        let mut h = History::default();
        type_out(&mut h, "hello");
        let back = h.undo(snap("hello", 5)).expect("one entry");
        assert_eq!(back.content, "", "five keystrokes undo in one press");
        assert!(h.undo(snap("", 0)).is_none(), "and there is nothing behind it");
    }

    #[test]
    fn moving_the_caret_splits_the_run() {
        let mut h = History::default();
        type_out(&mut h, "abc");
        h.break_run();
        // Typing again after the caret moved starts a second entry.
        h.record(snap("abc", 3), Some(3), 1);
        assert_eq!(h.undo(snap("abcd", 4)).unwrap().content, "abc");
        assert_eq!(h.undo(snap("abc", 3)).unwrap().content, "");
    }

    #[test]
    fn a_non_contiguous_insert_starts_a_new_entry() {
        // Typing at the end, then typing at the start, must not merge — the offsets don't meet.
        let mut h = History::default();
        h.record(snap("", 0), Some(0), 1);
        h.record(snap("a", 1), Some(0), 1);
        assert_eq!(h.undo(snap("ba", 1)).unwrap().content, "a");
        assert_eq!(h.undo(snap("a", 1)).unwrap().content, "");
    }

    #[test]
    fn a_deletion_is_always_its_own_entry() {
        let mut h = History::default();
        type_out(&mut h, "ab");
        h.record(snap("ab", 2), None, 0); // backspace
        assert_eq!(h.undo(snap("a", 1)).unwrap().content, "ab", "undo the delete");
        assert_eq!(h.undo(snap("ab", 2)).unwrap().content, "", "then the typed run");
    }

    #[test]
    fn redo_replays_and_a_new_edit_discards_the_branch() {
        let mut h = History::default();
        type_out(&mut h, "one");
        let undone = h.undo(snap("one", 3)).unwrap();
        assert_eq!(undone.content, "");
        assert_eq!(h.redo(snap("", 0)).unwrap().content, "one", "redo comes back");

        // Undo, then type something else: the old future must be unreachable.
        h.undo(snap("one", 3)).unwrap();
        h.record(snap("", 0), Some(0), 1);
        assert!(h.redo(snap("x", 1)).is_none(), "a new edit kills the redo branch");
    }

    #[test]
    fn undo_restores_the_caret_not_just_the_text() {
        // The whole reason the selection is in the snapshot: a second undo has to land
        // somewhere predictable.
        let mut h = History::default();
        h.record(
            EditSnapshot { content: "hello".into(), selection: 2..4, reversed: true },
            None,
            0,
        );
        let back = h.undo(snap("hXo", 2)).unwrap();
        assert_eq!(back.selection, 2..4);
        assert!(back.reversed);
    }

    #[test]
    fn undo_on_an_untouched_surface_does_nothing() {
        let mut h = History::default();
        assert!(h.undo(snap("", 0)).is_none());
        assert!(h.redo(snap("", 0)).is_none());
    }

    #[test]
    fn double_click_picks_the_word_under_the_caret() {
        let url = "https://api.example.com/posts";
        assert_eq!(&url[word_at(url, 9)], "api", "inside a word");
        assert_eq!(&url[word_at(url, 8)], "api", "at its first character");
        assert_eq!(&url[word_at(url, 5)], "://", "punctuation selects its own run");
        assert_eq!(&url[word_at(url, url.len())], "posts", "at the very end");
        assert_eq!(word_at("", 0), 0..0);
    }

    #[test]
    fn triple_click_picks_the_line_without_its_newline() {
        let text = "first\nsecond\nthird";
        assert_eq!(&text[line_at(text, 0)], "first");
        assert_eq!(&text[line_at(text, 8)], "second");
        assert_eq!(&text[line_at(text, text.len())], "third");
        // A trailing newline leaves an empty last line, and selecting it must not include the
        // newline itself — that is what would let a triple-click delete join two lines.
        let ends = "a\n";
        assert_eq!(&ends[line_at(ends, 2)], "");
        assert_eq!(&ends[line_at(ends, 0)], "a");
    }
}
