//! The unsaved-changes prompt.
//!
//! `Ctrl+W` used to discard edits silently — the one data-loss path in Zuno, since quitting
//! preserves every buffer through the session envelope and only closing a *tab* does not.
//!
//! A centred modal rather than the anchored menu that confirms a delete: a delete is answered
//! where you right-clicked, and a close can arrive from a keystroke with no anchor at all.

use gpui::{
    App, EntityId, FocusHandle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, div, px,
};

use crate::actions::ConfirmClose;
use crate::theme::Theme;
use crate::workspace::Workspace;

const WIDTH: f32 = 420.;

/// Which button is selected. Ordered as drawn, so `left`/`right` are `prev`/`next`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Save,
    Discard,
    Cancel,
}

impl Choice {
    const ORDER: [Choice; 3] = [Choice::Save, Choice::Discard, Choice::Cancel];

    fn label(self, many: bool) -> &'static str {
        match (self, many) {
            (Choice::Save, false) => "Save",
            (Choice::Save, true) => "Save all",
            (Choice::Discard, _) => "Don't save",
            (Choice::Cancel, _) => "Cancel",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Choice::Save => "close-save",
            Choice::Discard => "close-discard",
            Choice::Cancel => "close-cancel",
        }
    }

    fn step(self, delta: isize) -> Choice {
        let at = Self::ORDER.iter().position(|c| *c == self).unwrap_or(0) as isize;
        let next = (at + delta).rem_euclid(Self::ORDER.len() as isize) as usize;
        Self::ORDER[next]
    }
}

pub struct CloseConfirm {
    /// Every buffer this prompt will close, by entity id.
    ///
    /// **Ids rather than indices**, and it is the same reason `close_many` uses them: each close
    /// renumbers `views`, so a stored index would aim at whatever slid into that slot. An id
    /// that is no longer in `views` is simply skipped, which is also the guard the old single
    /// index needed a separate check for.
    pub targets: Vec<EntityId>,
    /// How many of `targets` have unsaved changes. Drives the wording and the button labels.
    pub unsaved: usize,
    /// Named only when exactly one is unsaved — past that a count reads better than a list.
    pub label: SharedString,
    pub choice: Choice,
    /// Where focus was, so dismissing puts it back. Same guard the picker, settings and import
    /// panels each carry.
    pub restore_focus: Option<FocusHandle>,
    pub focus_handle: FocusHandle,
}

impl CloseConfirm {
    pub fn new(
        targets: Vec<EntityId>,
        unsaved: usize,
        label: SharedString,
        restore_focus: Option<FocusHandle>,
        cx: &mut App,
    ) -> Self {
        Self {
            targets,
            unsaved,
            label,
            // Cancel, not Save: the safe default is the one that changes nothing, and Enter is
            // the key most likely to be pressed reflexively.
            choice: Choice::Cancel,
            restore_focus,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn step(&mut self, delta: isize) {
        self.choice = self.choice.step(delta);
    }
}

pub fn render(state: &CloseConfirm, theme: &Theme, cx: &mut gpui::Context<Workspace>) -> impl IntoElement {
    let selected = state.choice;
    let many = state.unsaved > 1;

    div()
        .id("close-confirm-scrim")
        // A scrim that catches clicks does not stop the wheel: scroll handlers gate on the hit
        // test, not on propagation. Every overlay in this app needs this.
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .track_focus(&state.focus_handle)
                .key_context("CloseConfirm")
                .w(px(WIDTH))
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg_elevated)
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.text)
                        .child(SharedString::from(if state.unsaved == 1 {
                            format!("{} has unsaved changes.", state.label)
                        } else {
                            format!("{} requests have unsaved changes.", state.unsaved)
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child("Closing the tab discards them — there is no undo."),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        // Spelled out rather than mapped over `ORDER`: `array::map` wants an
                        // `FnMut` and each button borrows `cx` mutably to build its listener.
                        .child(button(Choice::Save, selected, many, theme, cx))
                        .child(button(Choice::Discard, selected, many, theme, cx))
                        .child(button(Choice::Cancel, selected, many, theme, cx)),
                ),
        )
}

/// `use<>` because nothing in the returned element borrows `theme` or `cx` — `cx.listener`
/// hands back an owned closure — and without it each button holds a mutable borrow of `cx` for
/// its whole life, so three of them cannot coexist. See `settings_panel::setting_row`.
fn button(
    choice: Choice,
    selected: Choice,
    many: bool,
    theme: &Theme,
    cx: &mut gpui::Context<Workspace>,
) -> impl IntoElement + use<> {
    let selected = choice == selected;
    let danger = matches!(choice, Choice::Discard);

    div()
        .id(choice.id())
        .debug_selector(move || choice.id().to_string())
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .border_1()
        .border_color(if selected { theme.accent } else { theme.border })
        .text_color(if danger { theme.status_server_error } else { theme.text })
        .bg(if selected { theme.bg_hover } else { theme.bg })
        .hover(|style| style.bg(theme.bg_hover))
        // A click both chooses and confirms; the selection exists for the keyboard.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                workspace.set_close_choice(choice);
                window.dispatch_action(Box::new(ConfirmClose), cx);
            }),
        )
        .child(choice.label(many))
}
