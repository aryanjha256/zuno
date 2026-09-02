//! An anchored menu of actions, opened by right-click.
//!
//! The picker is a *centred* modal, which is right for a palette and wrong here: a context
//! menu's whole premise is that it appears where you clicked. So this is the first consumer of
//! `anchored()` — a question architecture.md §12 left open twice, once when the picker chose
//! modal and again when the method dropdown turned out not to want anchoring either.
//!
//! Deliberately built as a primitive with one caller rather than as a response-pane feature,
//! for ROADMAP principle 2's reason: saved requests want delete/rename, the tab strip wants
//! close/rename, and header rows want toggle/remove. Same bet the picker made, which shipped
//! with one consumer and reached seven without a rewrite.

use gpui::{
    AnchoredPositionMode, App, Context, Corner, FocusHandle, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, SharedString, Styled,
    Window, anchored, div, px,
};

use crate::theme::ActiveTheme;

const ROW_HEIGHT: f32 = 24.;
/// Wide enough that the keybinding column doesn't collide with the label on the shortest row.
const MIN_WIDTH: f32 = 200.;

/// One row. The keystroke is resolved when the menu is *built*, not when it's drawn, because
/// reading the keymap needs a `Window` and the render closure has one only by accident of
/// nesting — resolving early also means the string can't disagree with what dispatch will do.
pub struct MenuItem {
    pub label: SharedString,
    /// The dimmed right-hand column. A keystroke read from the live keymap for an action row,
    /// so a rebinding can't leave the menu advertising a dead key; any short string otherwise.
    /// Empty draws as no column rather than as a gap.
    pub detail: SharedString,
    pub command: MenuCommand,
}

/// What choosing a row does.
///
/// The menu performs neither itself — both travel out through `Chose` so the opener can close
/// first. That ordering is load-bearing for `Dispatch` (see `ContextMenuEvent`) and free for
/// `OpenUrl`, and one path is easier to keep right than two.
pub enum MenuCommand {
    Dispatch(Box<dyn gpui::Action>),
    OpenUrl(SharedString),
    /// Close and do nothing.
    ///
    /// Exists for the way *out* of a destructive confirmation. `Escape` already dismisses, but
    /// a menu asking "delete this file?" with no visible answer other than the destructive one
    /// reads as having no way back — and a row that says so costs nothing.
    Dismiss,
}

impl Clone for MenuCommand {
    fn clone(&self) -> Self {
        match self {
            Self::Dispatch(action) => Self::Dispatch(action.boxed_clone()),
            Self::OpenUrl(url) => Self::OpenUrl(url.clone()),
            Self::Dismiss => Self::Dismiss,
        }
    }
}

/// A row, or the rule between two groups of them.
pub enum MenuRow {
    Item(MenuItem),
    Separator,
}

impl MenuItem {
    /// An action row, labelled with the keystroke it has **in `focus`'s context**.
    ///
    /// The handle is not decoration. A menu's verbs belong to a pane, and most of their bindings
    /// are scoped to that pane's key context — `ctrl-c` in `ResponsePane`, `f2` in
    /// `CollectionPanel`. `keybinding_label` asks the *rendered frame*, whose context stack is
    /// empty once the frame is built, so it can only ever find globally-bound actions: every
    /// row here rendered a blank column from the day the menu shipped, in a primitive whose
    /// stated purpose is to teach the keystroke. Pass the handle the menu restores focus to —
    /// it is the pane these verbs act on, which is the same question.
    pub fn new(
        label: impl Into<SharedString>,
        action: impl gpui::Action,
        focus: &FocusHandle,
        window: &Window,
    ) -> Self {
        let detail = crate::workspace::keybinding_label_in(&action, focus, window);
        Self {
            label: label.into(),
            detail: SharedString::from(detail),
            command: MenuCommand::Dispatch(action.boxed_clone()),
        }
    }

    /// A row that opens a link. `detail` is free text — the About row puts the version there,
    /// which is why the column is not called `keystroke` any more.
    pub fn url(
        label: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        url: impl Into<SharedString>,
    ) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            command: MenuCommand::OpenUrl(url.into()),
        }
    }

    /// A row that just closes the menu. See `MenuCommand::Dismiss`.
    pub fn dismiss(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            detail: SharedString::new_static(""),
            command: MenuCommand::Dismiss,
        }
    }

    /// A row spelling out a consequence, with no keystroke of its own.
    ///
    /// Confirmation rows are reached by choosing the row above them, never by a shortcut, so
    /// `MenuItem::new`'s keymap lookup would draw an empty column and imply one exists.
    pub fn plain(label: impl Into<SharedString>, action: impl gpui::Action) -> Self {
        Self {
            label: label.into(),
            detail: SharedString::new_static(""),
            command: MenuCommand::Dispatch(action.boxed_clone()),
        }
    }
}

impl From<MenuItem> for MenuRow {
    fn from(item: MenuItem) -> Self {
        MenuRow::Item(item)
    }
}

impl ContextMenu {
    /// The rows as drawn, with separators as `"---"`, for tests.
    ///
    /// Reads real state rather than painted bounds: `cx.debug_bounds` reports the previous
    /// frame, so it can confirm a menu is *there* and never that a particular row is.
    #[cfg(test)]
    pub(crate) fn row_labels(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| match row {
                MenuRow::Item(item) => item.label.to_string(),
                MenuRow::Separator => "---".to_string(),
            })
            .collect()
    }

    /// The rows as `(label, detail)`, for asserting which ones name a keystroke.
    #[cfg(test)]
    pub(crate) fn row_details(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                MenuRow::Item(item) => Some((item.label.to_string(), item.detail.to_string())),
                MenuRow::Separator => None,
            })
            .collect()
    }
}

pub enum ContextMenuEvent {
    Dismissed,
    /// Chosen, carrying what to do. The menu never performs it — the opener closes first and
    /// then acts, so focus is back where it belongs before an action resolves.
    Chose(MenuCommand),
}

impl gpui::EventEmitter<ContextMenuEvent> for ContextMenu {}

pub struct ContextMenu {
    rows: Vec<MenuRow>,
    /// Always indexes an `Item`; `select` steps over separators and the constructor starts on
    /// the first one, so nothing has to re-check before acting.
    selected: usize,
    /// Window coordinates, straight from the `MouseDownEvent` that opened it.
    at: Point<Pixels>,
    focus_handle: FocusHandle,
    /// Where focus was, so dismissing puts it back. Without it the keymap goes dead on close:
    /// focus would sit on a handle belonging to a dropped entity and no key context would
    /// match — the same failure `Picker` and `SettingsPanel` each guard against.
    restore_focus: Option<FocusHandle>,
}

impl ContextMenu {
    pub fn new(
        rows: Vec<MenuRow>,
        at: Point<Pixels>,
        restore_focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected = rows
            .iter()
            .position(|row| matches!(row, MenuRow::Item(_)))
            .unwrap_or(0);
        Self {
            rows,
            selected,
            at,
            // Left at gpui's default `tab_stop: false`: there is nothing to tab *to* inside a
            // menu, and a tab stop here would let Tab walk focus out through the scrim.
            focus_handle: cx.focus_handle(),
            restore_focus,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn restore_focus(&self) -> Option<FocusHandle> {
        self.restore_focus.clone()
    }

    /// Move the selection, clamped, stepping over separators.
    ///
    /// Clamped rather than wrapping, unlike the picker: a menu is short enough to see whole, so
    /// falling off the end and reappearing at the top is disorienting rather than convenient.
    /// Landing *on* a separator would be worse — a keypress that appears to do nothing — so the
    /// step continues in the same direction until it finds a row or runs out.
    pub fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        let step = delta.signum();
        if step == 0 {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let mut at = self.selected as isize;
        loop {
            let next = at + step;
            if next < 0 || next > last {
                break;
            }
            at = next;
            if matches!(self.rows[at as usize], MenuRow::Item(_)) {
                self.selected = at as usize;
                cx.notify();
                break;
            }
        }
    }

    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(MenuRow::Item(item)) = self.rows.get(self.selected) {
            cx.emit(ContextMenuEvent::Chose(item.command.clone()));
        }
    }

    fn choose(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix;
        self.confirm(cx);
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = self.selected;

        let rows: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let item = match row {
                    MenuRow::Separator => {
                        return div()
                            .flex_none()
                            .my_1()
                            .h(px(1.))
                            .bg(theme.border)
                            .into_any_element();
                    }
                    MenuRow::Item(item) => item,
                };
                let label = item.label.clone();
                let keystroke = item.detail.clone();

                let mut row = div()
                    .debug_selector(move || format!("menu-row-{ix}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_2();

                if ix == selected {
                    row = row.bg(theme.bg_hover);
                }

                row
                    // **Hover moves the selection rather than adding a second highlight.** A
                    // `hover` style would light the pointed-at row while `selected` still lit
                    // another, and `Enter` would fire the one you are not pointing at. Moving
                    // the selection keeps a single highlight that always says what Enter does.
                    .on_mouse_move(cx.listener(move |menu, _: &gpui::MouseMoveEvent, _, cx| {
                        if menu.selected != ix {
                            menu.selected = ix;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |menu, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            menu.choose(ix, cx);
                        }),
                    )
                    .child(div().flex_none().text_color(theme.text).child(label))
                    // `text_faint`, never `theme.border`: in the dark theme `border` equals
                    // `bg_hover`, so on the selected row this column would vanish — the exact
                    // bug the palette's keybinding column shipped with.
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_faint)
                            .child(keystroke),
                    )
                    .into_any_element()
            })
            .collect();

        // A transparent full-window catcher, not the picker's dimming scrim: a menu is a small
        // local choice and darkening the app for it would read as a modal dialog. It still has
        // to exist, because click-outside is the one dismissal a menu must have.
        div()
            .absolute()
            .inset_0()
            // A modal owns the mouse, not just the keyboard — see `picker.rs` for why this is
            // `occlude` and why it has no test.
            .occlude()
            .track_focus(&self.focus_handle)
            .key_context("ContextMenu")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.emit(ContextMenuEvent::Dismissed);
                }),
            )
            // Right-clicking elsewhere should move the menu, not leave two ideas of "here" —
            // dismissing lets the new click's own handler open a fresh one.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.emit(ContextMenuEvent::Dismissed);
                }),
            )
            .child(
                anchored()
                    .position(self.at)
                    .anchor(Corner::TopLeft)
                    // Window, not Local: `self.at` came from a `MouseDownEvent`, whose
                    // `position` is already in window coordinates. Reading it as local would
                    // add the scrim's origin twice.
                    .position_mode(AnchoredPositionMode::Window)
                    .child(
                        div()
                            .id("context-menu")
                            .debug_selector(|| "context-menu".to_string())
                            .flex()
                            .flex_col()
                            .min_w(px(MIN_WIDTH))
                            .bg(theme.bg_elevated)
                            .border_1()
                            .border_color(theme.border)
                            // Swallow clicks, or choosing a row also hits the catcher behind
                            // it and the menu dismisses before the choice is read.
                            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                            })
                            .children(rows),
                    ),
            )
    }
}

impl gpui::Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
