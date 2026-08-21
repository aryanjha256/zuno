//! The picker: a centred modal with a filter input and a fuzzy-ranked list.
//!
//! `ROADMAP.md` principle 2 — `Ctrl+P`, `Ctrl+K`, the method dropdown, and the environment
//! switcher are one interaction, and building it four times would make it feel different
//! four times. This is that one build.
//!
//! **Concrete, not generic.** A `PickerDelegate` trait would be the fully reusable shape,
//! but there is exactly one consumer today and invariant 1 says API waits for a caller.
//! Instead the picker owns `Vec<Item>`, each carrying a `Target` the picker never
//! interprets — it hands the chosen `Target` back to `Workspace` and stays ignorant of what
//! any of it means. Adding `Ctrl+K` is then a new `Target` variant, not a rewrite; if a
//! third consumer wants genuinely different rendering, *that* is when the trait earns its
//! complexity.
//!
//! **Modal, not anchored.** This is a full-size `absolute` overlay with a centred child,
//! not an `anchored()` popover. An earlier version of this note claimed the method dropdown
//! "will want" anchoring; it didn't. Anchoring needs the triggering element's screen bounds,
//! which is real plumbing, and in a keyboard-first app a centred picker is *better* — one
//! idiom, reachable without the mouse, and no second selection implementation. `anchored()`
//! is still there in gpui 0.2.2 if something ever genuinely needs to hang off a point.

use std::path::PathBuf;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, ScrollStrategy, SharedString, Styled,
    UniformListScrollHandle, Window, div, px, uniform_list,
};

use crate::input::TextInput;
use crate::theme::{ActiveTheme, Theme};

/// Row height, and the unit `uniform_list` measures in. Must match the row's real height
/// or scrolling drifts.
const ROW_HEIGHT: f32 = 26.;
/// How many rows are visible before the list scrolls.
const VISIBLE_ROWS: f32 = 12.;

/// What choosing a row does. Opaque to the picker itself.
pub enum Target {
    /// Switch to an already-open buffer, by index into `Workspace::views`.
    ///
    /// An index rather than a `RequestId` because the workspace addresses buffers by
    /// index everywhere else; the picker is rebuilt on every open, so it cannot go stale.
    Buffer(usize),
    /// Open a request from a collection file.
    File(PathBuf),
    /// Dispatch an application action — the command palette.
    Action(Box<dyn gpui::Action>),
    /// Set the active request's method.
    Method(zuno_core::Method),
    /// Select the active environment, or `None` for none.
    Environment(Option<String>),
    /// Show a retained response: `0` is live, `1` the run before it.
    Run(usize),
    /// Set the request's body type, and its raw sub-kind when it has one.
    BodyType(crate::request_view::BodyType, Option<zuno_core::RawKind>),
}

// Hand-written because `Box<dyn Action>` isn't `Clone`; `boxed_clone` is the trait's own
// answer to that. Deriving would require `Action: Clone`, which no trait object can be.
impl Clone for Target {
    fn clone(&self) -> Self {
        match self {
            Self::Buffer(ix) => Self::Buffer(*ix),
            Self::File(path) => Self::File(path.clone()),
            Self::Action(action) => Self::Action(action.boxed_clone()),
            Self::Method(method) => Self::Method(method.clone()),
            Self::Environment(name) => Self::Environment(name.clone()),
            Self::Run(offset) => Self::Run(*offset),
            Self::BodyType(body_type, kind) => Self::BodyType(*body_type, *kind),
        }
    }
}

pub struct Item {
    /// What the filter matches against, and the row's main text.
    pub label: SharedString,
    /// Dimmed trailing context — a URL, or a relative path.
    pub detail: SharedString,
    pub target: Target,
}

pub struct Picker {
    filter: Entity<TextInput>,
    items: Vec<Item>,
    /// Indices into `items`, best match first. Rebuilt whenever the query changes.
    matches: Vec<usize>,
    selected: usize,
    scroll: UniformListScrollHandle,
    /// Where focus was when the picker opened, so dismissing puts it back.
    ///
    /// Without this the keymap goes dead on dismiss: focus would be left on a handle
    /// belonging to the dropped picker, no key context would match, and every binding
    /// would silently stop working. Same failure as switching tabs without moving focus.
    restore_focus: Option<FocusHandle>,
    /// The query the matches were computed for, so `render` can notice a keystroke landed
    /// in the input. The input owns its text, and it doesn't emit events yet.
    last_query: String,
    /// Shown when there are no candidates at all, which means something different for a
    /// request list (nothing saved yet) than for a command list (a bug).
    empty_hint: &'static str,
    /// Turns the query itself into an extra candidate.
    ///
    /// Exists for custom HTTP verbs: typing `PURGE` should offer it even though no row
    /// matches, which is what lets the method picker reach `Method::Other` and closes the
    /// last of §11's non-body gaps. A plain `fn` pointer rather than a boxed closure —
    /// nothing needs captured state, and it keeps `Picker` free of a type parameter.
    fallback: Option<fn(&str) -> Option<Item>>,
    /// The current query's derived candidate, if the fallback produced one.
    ///
    /// Held apart from `items` rather than appended to it: appending would mean truncating
    /// on every re-rank and every `extend`, and one missed truncate leaves a stale synthetic
    /// row in the list. Kept separate, it is structurally impossible for it to accumulate.
    derived: Option<Item>,
}

impl Picker {
    pub fn new(
        items: Vec<Item>,
        placeholder: &'static str,
        restore_focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| TextInput::new(String::new(), placeholder, "Picker", cx));
        let empty_hint = placeholder;

        let mut picker = Self {
            filter,
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            restore_focus,
            last_query: String::new(),
            empty_hint,
            fallback: None,
            derived: None,
        };
        picker.refilter();
        picker
    }

    /// Offer the query itself as a candidate when `build` yields one. See `fallback`.
    pub fn set_fallback(&mut self, build: fn(&str) -> Option<Item>) {
        self.fallback = Some(build);
        self.refilter();
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.read(cx).focus_handle(cx)
    }

    pub fn restore_focus(&self) -> Option<FocusHandle> {
        self.restore_focus.clone()
    }

    /// The query as typed. Read from the input rather than mirrored into a field, matching
    /// `RequestView::spec` — one owner, no desync.
    fn query(&self, cx: &App) -> String {
        self.filter.read(cx).text().to_string()
    }

    fn refilter(&mut self) {
        let labels: Vec<&str> = self.items.iter().map(|item| item.label.as_ref()).collect();
        self.matches = zuno_core::fuzzy::rank(&self.last_query, labels);
        // Deliberately not ranked among the real rows: it is the "or use what I typed"
        // escape hatch, so it belongs last regardless of how it would score.
        self.derived = self.fallback.and_then(|build| build(&self.last_query));
        self.selected = 0;
    }

    /// Rows on screen: the ranked matches, then the derived row if there is one.
    fn visible_count(&self) -> usize {
        self.matches.len() + usize::from(self.derived.is_some())
    }

    /// The item at a visible row, whether real or derived.
    fn item_at(&self, visible: usize) -> Option<&Item> {
        match self.matches.get(visible) {
            Some(&ix) => self.items.get(ix),
            // Past the end of the matches means it's the derived row.
            None => self.derived.as_ref(),
        }
    }

    /// Move the selection, wrapping, and keep it on screen.
    pub fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.visible_count() == 0 {
            return;
        }
        let count = self.visible_count() as isize;
        // `rem_euclid` so a negative step from row 0 wraps to the end rather than panicking
        // on an underflowing usize.
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    /// Add candidates that arrived after the picker opened.
    ///
    /// The collection scan runs off-thread, so the picker opens with the buffer rows and
    /// gains the saved requests when they land. Re-ranks against whatever has been typed in
    /// the meantime, which matters more than it looks: on a slow disk you can easily finish
    /// typing before the scan returns, and results appearing unfiltered would be worse than
    /// them appearing late.
    pub fn extend(&mut self, items: impl IntoIterator<Item = Item>, cx: &mut Context<Self>) {
        let before = self.selected_item_index();
        self.items.extend(items);
        self.refilter();
        // Keep the highlight on whatever row the user had chosen, rather than yanking it
        // back to the top underneath them.
        if let Some(item_ix) = before {
            if let Some(position) = self.matches.iter().position(|&ix| ix == item_ix) {
                self.selected = position;
            }
        }
        cx.notify();
    }

    /// Index into `items` of the current selection — stable across a re-rank, unlike
    /// `selected`, which is a position in `matches`.
    fn selected_item_index(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    /// The rows as displayed, in order. Test-only: asserting on this is far more durable
    /// than reaching into rendered elements, and it's the same data `render` reads.
    #[cfg(test)]
    pub fn visible_rows(&self) -> Vec<String> {
        (0..self.visible_count())
            .filter_map(|visible| self.item_at(visible))
            .map(|item| format!("{} — {}", item.label, item.detail))
            .collect()
    }

    #[cfg(test)]
    pub fn selection(&self) -> usize {
        self.selected
    }

    /// The target under the selection, if any. `None` when nothing matched — pressing
    /// enter on an empty list must do nothing rather than pick something arbitrary.
    pub fn chosen(&self) -> Option<&Target> {
        Some(&self.item_at(self.selected)?.target)
    }
}

impl Render for Picker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-rank when the query changed. Polled in render rather than driven by an input
        // event because `TextInput` doesn't emit one, and every keystroke already causes a
        // frame — so this runs exactly as often as it needs to, and the string compare is
        // far cheaper than the ranking it guards.
        let query = self.query(cx);
        if query != self.last_query {
            self.last_query = query;
            self.refilter();
        }

        let theme = cx.theme().clone();
        let count = self.visible_count();
        let selected = self.selected;

        // Owned clones, so the row closure borrows nothing from `self`.
        let rows: Vec<(SharedString, SharedString)> = (0..count)
            .filter_map(|visible| self.item_at(visible))
            .map(|item| (item.label.clone(), item.detail.clone()))
            .collect();

        // Full-window scrim. Clicking it dismisses, which is the one mouse affordance a
        // modal has to have.
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .bg(gpui::hsla(0., 0., 0., 0.4))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.emit(PickerEvent::Dismissed);
                }),
            )
            .child(
                div()
                    .id("picker")
                    .mt(px(96.))
                    .w(px(620.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border)
                    // Swallow clicks so choosing a row doesn't hit the scrim behind it.
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(self.filter.clone()),
                    )
                    .child(if count == 0 {
                        empty_state(&self.last_query, self.empty_hint, &theme).into_any_element()
                    } else {
                        result_list(rows, selected, self.scroll.clone(), &theme, cx)
                            .into_any_element()
                    }),
            )
    }
}

/// What the picker tells `Workspace`. One event type rather than two, so there's a single
/// subscription and a single place that closes the modal.
pub enum PickerEvent {
    /// Close without choosing.
    Dismissed,
    /// Act on `Picker::chosen`, then close.
    Confirmed,
}

impl gpui::EventEmitter<PickerEvent> for Picker {}

fn empty_state(query: &str, empty_hint: &str, theme: &Theme) -> impl IntoElement {
    // Distinguishes "you filtered everything out" from "there was nothing to begin with",
    // because the fix for each is completely different.
    let message = if query.is_empty() {
        empty_hint.to_string()
    } else {
        format!("Nothing matches “{query}”")
    };

    div()
        .flex_none()
        .px_3()
        .py_3()
        .text_xs()
        .text_color(theme.text_muted)
        .child(message)
}

fn result_list(
    rows: Vec<(SharedString, SharedString)>,
    selected: usize,
    scroll: UniformListScrollHandle,
    theme: &Theme,
    cx: &mut Context<Picker>,
) -> impl IntoElement {
    let count = rows.len();
    let theme = theme.clone();
    // `uniform_list`'s closure gets `&mut App`, not `Context<Picker>`, so `cx.listener`
    // isn't available inside it. A weak handle is how `response_pane`'s fold chevrons
    // solve the same problem; weak so a row element can't keep the picker alive.
    let picker = cx.entity().downgrade();

    uniform_list("picker-results", count, move |range, _window, _cx| {
        let picker = picker.clone();
        range
            .map(|ix| {
                let (label, detail) = rows[ix].clone();
                let active = ix == selected;
                let picker = picker.clone();

                div()
                    .id(("picker-row", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .overflow_hidden()
                    .cursor_pointer()
                    .bg(if active {
                        theme.bg_hover
                    } else {
                        theme.bg_elevated
                    })
                    .hover(|style| style.bg(theme.bg_hover))
                    // Clicking a row selects *that* row and confirms it, rather than
                    // confirming whatever the keyboard had selected.
                    .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                        let _ = picker.update(cx, |picker, cx| {
                            picker.selected = ix;
                            cx.emit(PickerEvent::Confirmed);
                        });
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if active { theme.text } else { theme.text_muted })
                            .child(label),
                    )
                    .child(
                        // The detail is what gets clipped when space runs out, never the
                        // name — the name is what you searched for.
                        div()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_xs()
                            .text_color(theme.border)
                            .child(detail),
                    )
            })
            .collect()
    })
    .track_scroll(scroll)
    // A definite height, or `uniform_list` has no viewport to scroll within. Shrinks to
    // fit when there are fewer results than fit on screen.
    .h(px(ROW_HEIGHT * VISIBLE_ROWS.min(count as f32)))
}
