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
    /// Read from the live keymap, so a rebinding can't leave the menu advertising a dead key.
    /// Empty when the action has no binding, which draws as no column rather than as a gap.
    pub keystroke: SharedString,
    pub action: Box<dyn gpui::Action>,
}

impl MenuItem {
    pub fn new(label: impl Into<SharedString>, action: impl gpui::Action, window: &Window) -> Self {
        let keystroke = crate::workspace::keybinding_label(&action, window);
        Self {
            label: label.into(),
            keystroke: SharedString::from(keystroke),
            action: action.boxed_clone(),
        }
    }
}

pub enum ContextMenuEvent {
    Dismissed,
    /// Chosen, carrying the action to dispatch. The menu never dispatches it itself — the
    /// opener closes first and then dispatches, so focus is back where it belongs before the
    /// action resolves.
    Chose(Box<dyn gpui::Action>),
}

impl gpui::EventEmitter<ContextMenuEvent> for ContextMenu {}

pub struct ContextMenu {
    items: Vec<MenuItem>,
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
        items: Vec<MenuItem>,
        at: Point<Pixels>,
        restore_focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            items,
            selected: 0,
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

    /// Move the selection, clamped. Unlike the picker this does not wrap: a menu is short
    /// enough to see whole, so falling off the end and reappearing at the top is disorienting
    /// rather than convenient.
    pub fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        cx.notify();
    }

    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.items.get(self.selected) {
            cx.emit(ContextMenuEvent::Chose(item.action.boxed_clone()));
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
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                let label = item.label.clone();
                let keystroke = item.keystroke.clone();

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
