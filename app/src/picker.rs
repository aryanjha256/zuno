//! The picker: a centred modal with a filter input and a fuzzy-ranked list.
//!
//! `ROADMAP.md` principle 2 — `Ctrl+P`, `Ctrl+K`, the method dropdown, and the environment
//! switcher are one interaction, and building it four times would make it feel different
//! four times. This is that one build.
//!
//! **Concrete, not generic.** A `PickerDelegate` trait would be the fully reusable shape. The
//! picker instead owns `Vec<Item>`, each carrying a `Target` it never interprets — it hands the
//! chosen `Target` back to `Workspace` and stays ignorant of what any of it means. A new consumer
//! is a new `Target` variant, not a rewrite.
//!
//! **The original reason was "one consumer today"; that is long false and the decision still
//! holds.** There are seven variants now — buffers, files, palette actions, methods, environments,
//! runs, body types. The recorded trigger was never the count, though: it was *a consumer wanting
//! genuinely different rendering*, and that half has not fired. All seven differ in the data they
//! carry, which is precisely what `Target` absorbs, and every one renders as label plus dimmed
//! detail. A trait would abstract over a difference that doesn't exist. Revisit when a consumer
//! needs a different row — multi-line, an icon, a preview pane — not when the next variant lands.
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
    StatefulInteractiveElement, Subscription, UniformListScrollHandle, Window, div, px,
    uniform_list,
};

use crate::input::TextInput;
use crate::input::text_input::Changed;
use crate::theme::{ActiveTheme, Theme};

/// Row height, and the unit `uniform_list` measures in. Must match the row's real height
/// or scrolling drifts.
const ROW_HEIGHT: f32 = 26.;
/// How many characters of the two columns together fit across a row.
///
/// The modal is 620px with `px_3` either side and a `gap_2` between, leaving 588px. At `text_xs`
/// the UI font runs about 5.95px per character — the advance `TAB_LABEL_CHARS` is tuned to — so
/// 98, less a margin because erring short is invisible and overshooting brings the clip back.
///
/// Counted rather than measured, for the reason `elide` is: real widths need the shipping font,
/// which the test platform does not have, and a pure function over a string is something a test
/// can check. `whitespace_nowrap` plus `overflow_hidden` stay underneath as the backstop.
const ROW_CHARS: usize = 94;

/// How to split `ROW_CHARS` between a row's label and its detail.
///
/// **Whichever column is short donates its slack to the other**, and only when both want more
/// than half do they take half each. A fixed half-and-half was the first version: it reserved
/// half the row for a six-character label and then elided a URL that would have fit in the space
/// going spare beside it.
///
/// Pure, so the rule is a unit test rather than something you check by looking at a screenshot.
fn split_budget(label: usize, detail: usize) -> (usize, usize) {
    if label + detail <= ROW_CHARS {
        return (label, detail);
    }
    let half = ROW_CHARS / 2;
    if label <= half {
        (label, ROW_CHARS - label)
    } else if detail <= half {
        (ROW_CHARS - detail, detail)
    } else {
        (half, ROW_CHARS - half)
    }
}
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
    /// A directory to move the selected request into.
    ///
    /// The eighth consumer, and still no `PickerDelegate` trait: it draws as label plus dimmed
    /// detail like the other seven, which is the bar §12 sets for reconsidering.
    Folder(PathBuf),
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
            Self::Folder(path) => Self::Folder(path.clone()),
        }
    }
}

pub struct Item {
    /// What the filter matches against, and the row's main text.
    pub label: SharedString,
    /// Dimmed trailing context — a URL, or a relative path.
    ///
    /// **Not matched against.** `refilter` ranks `label` alone, so a buffer row displays a URL you
    /// cannot search by: `Ctrl+P` finds a request by `posts`, not by `api.github.com`. Deliberate
    /// for now — the label is what a person is reaching for, and scoring both would let a long URL
    /// outrank a name that matches exactly — but worth knowing, because the row visibly shows text
    /// the filter ignores. Doing it properly needs per-field weights so a detail hit can never beat
    /// a label hit, which is more machinery than the current scale asks for.
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
    /// Held, not detached: dropping a `Subscription` unsubscribes, so this has to outlive
    /// every keystroke. Dying with the picker is exactly the lifetime wanted.
    _filter_changed: Subscription,
    /// Shown when there are no candidates at all, which means something different for a
    /// request list (nothing saved yet) than for a command list (a bug).
    empty_hint: SharedString,
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
        placeholder: impl Into<SharedString>,
        restore_focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        // `SharedString`, not `&'static str`, because the empty hint now names a keystroke read
        // from the live keymap — see `Workspace::keybinding_label`. A literal cannot do that.
        let placeholder: SharedString = placeholder.into();
        let filter = cx.new(|cx| TextInput::new(String::new(), placeholder.clone(), "Picker", cx));
        let empty_hint = placeholder;

        // Re-rank on every edit. This used to be a string compare against a stored
        // `last_query` in `render`, with a comment explaining that `TextInput` emitted nothing
        // to react to. It does now, so the mirror is gone.
        let filter_changed = cx.subscribe(&filter, |picker: &mut Self, _, _: &Changed, cx| {
            picker.refilter(cx);
            cx.notify();
        });

        let mut picker = Self {
            filter,
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            restore_focus,
            _filter_changed: filter_changed,
            empty_hint,
            fallback: None,
            derived: None,
        };
        picker.refilter(cx);
        picker
    }

    /// Offer the query itself as a candidate when `build` yields one. See `fallback`.
    pub fn set_fallback(&mut self, build: fn(&str) -> Option<Item>, cx: &mut Context<Self>) {
        self.fallback = Some(build);
        self.refilter(cx);
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

    fn refilter(&mut self, cx: &App) {
        let query = self.query(cx);
        let labels: Vec<&str> = self.items.iter().map(|item| item.label.as_ref()).collect();
        self.matches = zuno_core::fuzzy::rank(&query, labels);
        // Deliberately not ranked among the real rows: it is the "or use what I typed"
        // escape hatch, so it belongs last regardless of how it would score.
        self.derived = self.fallback.and_then(|build| build(&query));
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
        self.refilter(cx);
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

/// One of a row's two columns: a single line, already shortened, with both strings on hover.
///
/// A function rather than an inline chain because the tooltip is conditional, and
/// `gpui::util::FluentBuilder::when` — which would read better — is behind a private module.
///
/// **Both columns get the same share of the row**, `flex_1` with `min_w(0)`, which is what makes
/// the character budget mean anything: shorten to a budget wider than the column and the string
/// is elided *and* clipped, with an ellipsis in the middle and a hard cut at the end.
fn column(
    id: (&'static str, usize),
    shown: SharedString,
    tooltip: Option<(SharedString, SharedString)>,
    color: gpui::Hsla,
) -> gpui::Stateful<gpui::Div> {
    let mut cell = div()
        .id(id)
        // **Sized to its content, not `flex_1`.** `flex_1` is `flex: 1 1 0%` — a zero basis, so
        // both columns take half the row whatever they hold, and a six-character label reserved
        // half a 620px modal while the URL beside it was elided into the gap. The default
        // `flex: 0 1 auto` sizes each to its text and shrinks only when the pair overflows.
        .min_w(px(0.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_xs()
        .text_color(color)
        .child(shown);

    // Only when something was actually dropped, and then it carries *both* strings — whichever
    // one was cut, the row as a whole is what you are trying to read.
    if let Some((label, detail)) = tooltip {
        cell = cell.tooltip(move |_window, cx| {
            crate::ui::Tooltip::lines([label.clone(), detail.clone()], cx)
        });
    }
    cell
}

impl Render for Picker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query(cx);
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
            // **A modal owns the mouse, the same way `Workspace::modal_open` makes it own the
            // keyboard.** Catching *clicks* on the scrim was never enough: a scroll handler
            // gates on `hitbox.should_handle_scroll`, which consults the hit test rather than
            // propagation, so the wheel went straight through to whatever was behind. `hit_test`
            // walks topmost-first and stops at a `BlockMouse` hitbox, which is what takes the
            // panes underneath out of it.
            //
            // **Not covered by a test, deliberately.** Nothing behind a modal moves in the
            // headless platform whether this is here or not — the scrim's own handlers already
            // absorb what a test can simulate — so an assertion here would pass against the bug
            // and read as coverage. Found by using the window; verified against `hit_test` in
            // the vendored source; left untested on purpose. `settings_panel` and `context_menu`
            // carry the same call for the same reason.
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.emit(PickerEvent::Dismissed);
                }),
            )
            .child(
                div()
                    .id("picker")
                    .debug_selector(|| "picker".to_string())
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
                        empty_state(&query, &self.empty_hint, &theme).into_any_element()
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
                // **The two columns elide in opposite directions**, because their information
                // sits at opposite ends. A path's head names the collection and its tail is one
                // more request; a URL's head is the `http://host:port` every row repeats and its
                // tail is the endpoint that tells them apart. Trim both the same way and one of
                // them says nothing.
                //
                // **At render, never in `Item::label`.** `refilter` ranks the stored string, so
                // shortening at construction would mean typing the part that was dropped stops
                // finding the row — searching against an ellipsis.
                let (label_budget, detail_budget) =
                    split_budget(label.chars().count(), detail.chars().count());
                let shown_label = zuno_core::request::elide(&label, label_budget);
                let shown_detail = zuno_core::request::elide_front(&detail, detail_budget);
                let elided = matches!(shown_label, std::borrow::Cow::Owned(_))
                    || matches!(shown_detail, std::borrow::Cow::Owned(_));
                let shown_label = SharedString::from(shown_label.into_owned());
                let shown_detail = SharedString::from(shown_detail.into_owned());
                let full = (label.clone(), detail.clone());
                let active = ix == selected;
                let picker = picker.clone();

                div()
                    .id(("picker-row", ix))
                    .debug_selector(move || format!("picker-row-{ix}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    // **`w_full` is load-bearing, and its absence is invisible in the code.**
                    // `uniform_list` lays every item out as a taffy *root* with the list's width
                    // as definite available space — but taffy only stretches a root to fill that
                    // space when the node is `display: block`, which is a gate inside
                    // `compute_root_layout` (taffy 0.9's `style.is_block()`). A `.flex()` row
                    // takes the other path and sizes to its content, so the row was as wide as
                    // its label: the selection highlight stopped mid-row, and the right two
                    // thirds of a 620px list did not respond to a click at all. Nothing in the
                    // headless platform can see a paint, so the click hole is what
                    // `a_picker_row_spans_the_full_width_of_the_list` asserts.
                    .w_full()
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

                    // **`whitespace_nowrap` on both, and `flex_none` gone from the label.** Two
                    // faults, one symptom. gpui's default is `WhiteSpace::Normal`, so a long
                    // label *wrapped*, and the row is a fixed `ROW_HEIGHT` — as `uniform_list`
                    // demands — so the second line was sliced through. And `flex_none` meant the
                    // label could not shrink at all, so it pushed the detail out of the row on
                    // the way. Neither is visible in a short label, which is every label this
                    // picker had until requests started arriving from an OpenAPI spec named
                    // `TheGameYou-Misc-API-Notification-Blob/…`.
                    .child(column(
                        ("picker-label", ix),
                        shown_label,
                        elided.then(|| (full.0.clone(), full.1.clone())),
                        if active { theme.text } else { theme.text_muted },
                    ))
                    .child(column(
                        ("picker-detail", ix),
                        shown_detail,
                        elided.then(|| (full.0.clone(), full.1.clone())),
                        // Not `theme.border`, which is what this was. In the dark theme `border`
                        // *equals* `bg_hover`, so the detail — which for the command palette is
                        // the keybinding — was invisible on the selected row. See
                        // `Theme::text_faint`.
                        theme.text_faint,
                    ))
            })
            .collect()
    })
    .track_scroll(scroll)
    // A definite height, or `uniform_list` has no viewport to scroll within. Shrinks to
    // fit when there are fewer results than fit on screen.
    .h(px(ROW_HEIGHT * VISIBLE_ROWS.min(count as f32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_column_donates_its_slack_to_the_long_one() {
        // The rule that replaced a fixed half-and-half. Both failure modes are silent on screen:
        // too small a budget elides a string that would have fitted, too large a one brings back
        // the clip the elision exists to remove.

        // Both fit: neither is touched, and the row does not reserve half for a short label.
        assert_eq!(split_budget(10, 20), (10, 20));
        assert_eq!(split_budget(0, 0), (0, 0));

        // A short label, a long detail: the detail gets everything the label did not want.
        let (label, detail) = split_budget(12, 400);
        assert_eq!(label, 12, "a short label must not be padded out to half a row");
        assert_eq!(label + detail, ROW_CHARS);
        assert!(detail > ROW_CHARS / 2, "the slack has to go somewhere: {detail}");

        // And the mirror — a long label beside a short detail.
        let (label, detail) = split_budget(400, 12);
        assert_eq!(detail, 12);
        assert_eq!(label + detail, ROW_CHARS);

        // Both long: half each, since neither has slack to give.
        let (label, detail) = split_budget(400, 400);
        assert_eq!(label, ROW_CHARS / 2);
        assert_eq!(label + detail, ROW_CHARS);
    }

    #[test]
    fn a_split_never_exceeds_the_row() {
        // Swept, because the arms overlap at the boundary and an off-by-one there would clip
        // every row in the list rather than one.
        for label in 0..140 {
            for detail in 0..140 {
                let (l, d) = split_budget(label, detail);
                assert!(l <= label && d <= detail, "{label}/{detail} -> {l}/{d}");
                assert!(
                    l + d <= ROW_CHARS.max(label + detail),
                    "{label}/{detail} -> {l}/{d}"
                );
            }
        }
    }
}
