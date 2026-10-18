//! Toasts: an error, or work that finished in the background, raised above the status bar.
//!
//! **Three levels, and only two of them are toasts.** The status bar's message line is one grey
//! line at the bottom edge, truncated, and gone on the next tab switch — right for "Copied" and
//! "Saved", which answer a key you just pressed while you were looking, and wrong for "Could not
//! save" or an import that finished while you were somewhere else. Toasting every confirmation
//! instead would teach people to ignore toasts, so:
//!
//! - **Info** stays in the status bar (`Workspace::set_status`).
//! - **Done** — an import, an export, a clone — is a toast that fades after `FADE_AFTER`, and
//!   waits while the pointer rests on it.
//! - **Error** is a toast that stays until it is dismissed, with the full message one click away,
//!   because the reason something failed is exactly the part a truncated line cut off.
//!
//! Window-level rather than per tab, unlike the status message: a result that arrives after you
//! have switched tabs still lands where you are looking.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, Action, ClipboardItem, Context, MouseButton, MouseDownEvent, SharedString,
};

use crate::theme::Theme;
use crate::ui::{glyph, Icon};
use crate::workspace::Workspace;

/// How long a Done toast stays before it fades, once the pointer is off it.
pub const FADE_AFTER: Duration = Duration::from_secs(4);

/// Most toasts kept at once. Past it the oldest Done goes first, so an error is never pushed
/// out by a later success.
const MAX: usize = 5;

/// A detail longer than this collapses behind Details rather than growing the toast.
const SHORT_DETAIL: usize = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Done,
    Error,
}

pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub title: SharedString,
    pub detail: Option<SharedString>,
    /// One follow-up a button can take, as the action its keybinding would dispatch.
    pub action: Option<(SharedString, Box<dyn Action>)>,
    pub expanded: bool,
    pub hovered: bool,
}

#[derive(Default)]
pub struct Toasts {
    pub list: Vec<Toast>,
    next_id: u64,
}

impl Toasts {
    /// Add one, making room if needed. Returns its id, for the fade to find it again.
    pub fn push(
        &mut self,
        kind: ToastKind,
        title: impl Into<SharedString>,
        detail: Option<String>,
        action: Option<(SharedString, Box<dyn Action>)>,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        if self.list.len() >= MAX {
            let oldest = self
                .list
                .iter()
                .position(|toast| toast.kind == ToastKind::Done)
                .unwrap_or(0);
            self.list.remove(oldest);
        }
        self.list.push(Toast {
            id,
            kind,
            title: title.into(),
            detail: detail.filter(|detail| !detail.trim().is_empty()).map(SharedString::from),
            action,
            expanded: false,
            hovered: false,
        });
        id
    }

    pub fn dismiss(&mut self, id: u64) {
        self.list.retain(|toast| toast.id != id);
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Toast> {
        self.list.iter_mut().find(|toast| toast.id == id)
    }
}

/// `Could not save: permission denied` → (`Could not save`, `permission denied`).
///
/// Most errors reach the workspace as one formatted line, and that shape is nearly universal:
/// what failed, a colon, why. Split at the first `: ` so the title reads as a sentence and the
/// reason — often an OS or parser message — sits underneath.
pub fn split(message: &str) -> (String, Option<String>) {
    match message.split_once(": ") {
        Some((title, detail)) if !title.is_empty() && title.len() <= 80 => {
            (title.to_string(), Some(detail.to_string()))
        }
        _ => (message.to_string(), None),
    }
}

/// The stack, bottom-right above the status bar, or nothing when it is empty.
pub fn render(toasts: &Toasts, theme: &Theme, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    if toasts.list.is_empty() {
        return None;
    }
    let stack = div()
        .absolute()
        .right(px(16.))
        // The status bar's height plus a gap, so a toast never covers its badges.
        .bottom(px(36.))
        .flex()
        .flex_col()
        .items_end()
        .gap_2()
        .children(toasts.list.iter().map(|toast| one(toast, theme, cx)));
    Some(stack.into_any_element())
}

fn one(toast: &Toast, theme: &Theme, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let id = toast.id;
    let (icon, color) = match toast.kind {
        ToastKind::Done => (Icon::CircleCheck, theme.status_success),
        ToastKind::Error => (Icon::CircleAlert, theme.status_server_error),
    };
    let long = toast
        .detail
        .as_ref()
        .is_some_and(|detail| detail.len() > SHORT_DETAIL || detail.contains('\n'));

    let detail = toast.detail.as_ref().map(|detail| {
        let shown: SharedString = if long && !toast.expanded {
            let first = detail.lines().next().unwrap_or_default();
            let cut: String = first.chars().take(SHORT_DETAIL).collect();
            SharedString::from(format!("{cut}…"))
        } else {
            detail.clone()
        };
        let block = div().mt_1().text_xs().text_color(theme.text_muted);
        if long && toast.expanded {
            block
                .mt_2()
                .p_2()
                .rounded_sm()
                .bg(theme.bg)
                .border_1()
                .border_color(theme.border)
                .font_family(theme.mono.clone())
                .text_color(theme.text)
                .child(shown)
        } else {
            block.child(shown)
        }
    });

    let button = |label: SharedString, element_id: SharedString, primary: bool| {
        div()
            .id(element_id)
            .h(px(24.))
            .px_2()
            .flex()
            .items_center()
            .rounded_sm()
            .text_xs()
            .cursor_pointer()
            .when(primary, |button| {
                button.bg(theme.bg_hover).border_1().border_color(theme.border).text_color(theme.text)
            })
            .when(!primary, |button| button.text_color(theme.text_muted))
            .hover(|style| style.text_color(theme.text))
            .child(label)
    };

    let mut buttons = Vec::new();
    if let Some((label, action)) = &toast.action {
        let action = action.boxed_clone();
        buttons.push(
            button(label.clone(), format!("toast-{id}-action").into(), true)
                .debug_selector(move || format!("toast-{id}-action"))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.dismiss_toast(id, cx);
                        window.dispatch_action(action.boxed_clone(), cx);
                    }),
                )
                .into_any_element(),
        );
    }
    if long {
        let label = if toast.expanded { "Hide details" } else { "Details" };
        buttons.push(
            button(label.into(), format!("toast-{id}-details").into(), false)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        if let Some(toast) = this.toasts.get_mut(id) {
                            toast.expanded = !toast.expanded;
                        }
                        cx.notify();
                    }),
                )
                .into_any_element(),
        );
    }
    if toast.kind == ToastKind::Error {
        let text = match &toast.detail {
            Some(detail) => format!("{}: {detail}", toast.title),
            None => toast.title.to_string(),
        };
        buttons.push(
            button("Copy".into(), format!("toast-{id}-copy").into(), false)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    }),
                )
                .into_any_element(),
        );
    }

    div()
        .id(SharedString::from(format!("toast-{id}")))
        .debug_selector(move || format!("toast-{id}"))
        .w(px(380.))
        .flex()
        .flex_row()
        .gap_3()
        .p_3()
        .rounded_md()
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        // A click on a toast is for the toast, never for the pane underneath it.
        .occlude()
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if let Some(toast) = this.toasts.get_mut(id) {
                toast.hovered = *hovered;
            }
            cx.notify();
        }))
        .child(div().flex_none().pt(px(1.)).child(glyph(icon, color, color, 16.)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(div().text_sm().text_color(theme.text).child(toast.title.clone()))
                .children(detail)
                .when(!buttons.is_empty(), |body| {
                    body.child(div().mt_2().flex().flex_row().gap_1().children(buttons))
                }),
        )
        .child(
            div()
                .id(SharedString::from(format!("toast-{id}-close")))
                .debug_selector(move || format!("toast-{id}-close"))
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_pointer()
                .group(crate::ui::ICON_GROUP)
                .hover(|style| style.bg(theme.bg_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.dismiss_toast(id, cx);
                    }),
                )
                .child(glyph(Icon::Close, theme.text_faint, theme.text, 12.)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_splits_into_what_failed_and_why() {
        assert_eq!(
            split("Could not save: permission denied"),
            ("Could not save".to_string(), Some("permission denied".to_string()))
        );
        assert_eq!(split("Cancelled"), ("Cancelled".to_string(), None));
    }

    #[test]
    fn a_full_stack_drops_a_success_before_an_error() {
        let mut toasts = Toasts::default();
        let error = toasts.push(ToastKind::Error, "e", None, None);
        for _ in 0..MAX {
            toasts.push(ToastKind::Done, "d", None, None);
        }
        assert_eq!(toasts.list.len(), MAX);
        assert!(toasts.list.iter().any(|toast| toast.id == error), "the error must survive");
    }
}
