//! The request settings panel: a modal over the active buffer's `RequestSettings`.
//!
//! **What this is really for.** `architecture.md` §11 lists nine engine capabilities that
//! are honoured on every request with no way to see or change them. This surfaces five of
//! them — cookie jar, timeout, redirect following and its hop limit, TLS verification, and
//! content encodings — which makes it the best capability-per-line work left (ROADMAP
//! principle 3). ROADMAP claims six; counting them, it's five.
//!
//! **Per-request, deliberately.** `RequestSettings` lives on `RequestSpec`, so it's already
//! per-request and already persisted per collection file. A global-defaults layer would need
//! a scope model (global → environment → request), which is the *same* problem environments
//! has to solve in M3 — building a second one here would mean throwing one away. So: this
//! edits the active buffer only, and the panel says so.
//!
//! **Cookies are the reason this can't be pure UI.** `cookie_store` is part of the engine's
//! `ClientKey`, so toggling it off routes through a different cached client rather than
//! emptying a jar, and toggling it back returns the original client with its cookies intact.
//! A toggle on its own would create the confusion it exists to remove, so `Engine::
//! clear_cookies` landed alongside it and the panel offers it as an explicit action.

use std::time::Duration;

use gpui::{
    Context, FocusHandle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, SharedString, Styled, Window, div, px,
};
use zuno_core::RequestSettings;

use crate::theme::{ActiveTheme, Theme};

/// How much a `+`/`-` press moves the timeout.
const TIMEOUT_STEP_SECS: u64 = 5;
/// Bounds for the timeout, in seconds. Zero would mean "fail instantly"; the upper bound is
/// where a stuck request stops being a request you're waiting for.
const TIMEOUT_MIN_SECS: u64 = 1;
const TIMEOUT_MAX_SECS: u64 = 600;
/// Redirect hops. reqwest takes a `u8`, and beyond this a loop is the likelier explanation.
const MAX_REDIRECTS_CEILING: u8 = 50;

/// Which row the keyboard is on.
///
/// An enum rather than an index into a `Vec` of rows, so adding a setting can't silently
/// shift what a row does — and so `adjust` can be exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    CookieStore,
    VerifyTls,
    FollowRedirects,
    MaxRedirects,
    AcceptEncodings,
    Timeout,
    ClearCookies,
}

impl Row {
    /// Top to bottom, cookies first because it's the one that silently changes behaviour.
    pub const ALL: [Row; 7] = [
        Row::CookieStore,
        Row::VerifyTls,
        Row::FollowRedirects,
        Row::MaxRedirects,
        Row::AcceptEncodings,
        Row::Timeout,
        Row::ClearCookies,
    ];

    fn label(self) -> &'static str {
        match self {
            Row::CookieStore => "Store and replay cookies",
            Row::VerifyTls => "Verify TLS certificates",
            Row::FollowRedirects => "Follow redirects",
            Row::MaxRedirects => "Maximum redirect hops",
            Row::AcceptEncodings => "Accept compressed responses",
            Row::Timeout => "Timeout",
            Row::ClearCookies => "Clear stored cookies now",
        }
    }

    /// The consequence, not a restatement of the label. A settings row that only repeats
    /// its own name teaches nothing.
    fn hint(self) -> &'static str {
        match self {
            Row::CookieStore => "off makes each request independent",
            Row::VerifyTls => "off allows self-signed certificates",
            Row::FollowRedirects => "off returns the 3xx itself",
            Row::MaxRedirects => "guards against redirect loops",
            Row::AcceptEncodings => "gzip, brotli, deflate, zstd",
            Row::Timeout => "how long to wait for a response",
            Row::ClearCookies => "ends the session for every request",
        }
    }

    /// Whether this row is an action rather than a value. Actions have no on/off state and
    /// respond to Enter, not to left/right.
    fn is_action(self) -> bool {
        matches!(self, Row::ClearCookies)
    }
}

/// What the panel asks `Workspace` to do. The panel edits settings itself; only the things
/// it cannot reach on its own come back as events.
pub enum SettingsEvent {
    Dismissed,
    /// Cookies survive in the engine, not in `RequestSettings`, so only the workspace can
    /// do this.
    ClearCookies,
}

impl gpui::EventEmitter<SettingsEvent> for SettingsPanel {}

pub struct SettingsPanel {
    focus_handle: FocusHandle,
    settings: RequestSettings,
    selected: usize,
    /// Where focus was when the panel opened, so dismissing puts it back. Same failure as
    /// the picker's: without it, focus is left on a dropped handle, no key context matches,
    /// and every binding silently stops working.
    restore_focus: Option<FocusHandle>,
    /// Set once cookies have been cleared, so the row can confirm it happened. A silent
    /// action leaves you wondering whether you pressed it.
    cleared: bool,
}

impl SettingsPanel {
    pub fn new(
        settings: RequestSettings,
        restore_focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            // Its own handle rather than a text input's: every control here is a key press, so
            // there's nothing to type into. Left at gpui's default of `tab_stop: false` — Tab is
            // not how you move within a modal, up/down is. (This comment used to claim a
            // `tab_stop(false)` call that was never here. The default is *why* Tab used to escape
            // the panel entirely; `Workspace::modal_open` is what stops it now.)
            focus_handle: cx.focus_handle(),
            settings,
            selected: 0,
            restore_focus,
            cleared: false,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn restore_focus(&self) -> Option<FocusHandle> {
        self.restore_focus.clone()
    }

    /// The edited settings, for the workspace to write back onto the buffer.
    pub fn settings(&self) -> &RequestSettings {
        &self.settings
    }

    fn row(&self) -> Row {
        Row::ALL[self.selected]
    }

    pub fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = Row::ALL.len() as isize;
        // `rem_euclid` so up from the first row wraps instead of underflowing.
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
        cx.notify();
    }

    /// Toggle a boolean, or step a number. `delta` is -1 or +1.
    ///
    /// Returns whether anything changed, so the workspace only writes back when it must.
    pub fn adjust(&mut self, delta: i64, cx: &mut Context<Self>) -> bool {
        let changed = match self.row() {
            // A toggle ignores direction: left and right both flip it, which is what every
            // keyboard-driven settings list does.
            Row::CookieStore => flip(&mut self.settings.cookie_store),
            Row::VerifyTls => flip(&mut self.settings.verify_tls),
            Row::FollowRedirects => flip(&mut self.settings.follow_redirects),
            Row::AcceptEncodings => flip(&mut self.settings.accept_encodings),
            Row::MaxRedirects => {
                let next = (self.settings.max_redirects as i64 + delta)
                    .clamp(0, MAX_REDIRECTS_CEILING as i64) as u8;
                std::mem::replace(&mut self.settings.max_redirects, next) != next
            }
            Row::Timeout => {
                let current = self
                    .settings
                    .timeout
                    .map(|timeout| timeout.as_secs())
                    .unwrap_or(0);
                let next = (current as i64 + delta * TIMEOUT_STEP_SECS as i64)
                    .clamp(TIMEOUT_MIN_SECS as i64, TIMEOUT_MAX_SECS as i64) as u64;
                let next = Some(Duration::from_secs(next));
                std::mem::replace(&mut self.settings.timeout, next) != next
            }
            // Not a value; Enter is its verb.
            Row::ClearCookies => false,
        };

        if changed {
            cx.notify();
        }
        changed
    }

    /// Enter. Actions fire; values toggle, so Enter on a checkbox does the obvious thing.
    pub fn confirm(&mut self, cx: &mut Context<Self>) -> bool {
        if self.row().is_action() {
            self.cleared = true;
            cx.emit(SettingsEvent::ClearCookies);
            cx.notify();
            return false;
        }
        self.adjust(1, cx)
    }

    /// How the current value reads on screen.
    fn value(&self, row: Row) -> SharedString {
        let on_off = |on: bool| if on { "on" } else { "off" };
        match row {
            Row::CookieStore => on_off(self.settings.cookie_store).into(),
            Row::VerifyTls => on_off(self.settings.verify_tls).into(),
            Row::FollowRedirects => on_off(self.settings.follow_redirects).into(),
            Row::AcceptEncodings => on_off(self.settings.accept_encodings).into(),
            Row::MaxRedirects => SharedString::from(self.settings.max_redirects.to_string()),
            Row::Timeout => match self.settings.timeout {
                Some(timeout) => SharedString::from(format!("{}s", timeout.as_secs())),
                // Not currently reachable — `adjust` clamps above zero — but the model
                // allows it and rendering "0s" would be a lie.
                None => "none".into(),
            },
            Row::ClearCookies => {
                if self.cleared {
                    "cleared".into()
                } else {
                    "press Enter".into()
                }
            }
        }
    }

    #[cfg(test)]
    pub fn rows_for_test(&self) -> Vec<String> {
        Row::ALL
            .iter()
            .map(|row| format!("{} = {}", row.label(), self.value(*row)))
            .collect()
    }

    #[cfg(test)]
    pub fn selection(&self) -> usize {
        self.selected
    }
}

fn flip(value: &mut bool) -> bool {
    *value = !*value;
    true
}

impl Render for SettingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = self.selected;

        // Collected eagerly: `.children()` stores the iterator, so a closure capturing
        // `cx` would have to outlive this call and the borrow checker refuses. Same shape
        // as the picker's row list.
        let mut rows = Vec::with_capacity(Row::ALL.len());
        for (ix, row) in Row::ALL.iter().enumerate() {
            let value = self.value(*row);
            rows.push(setting_row(*row, value, ix == selected, &theme, cx));
        }

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .bg(gpui::hsla(0., 0., 0., 0.4))
            .track_focus(&self.focus_handle)
            .key_context("SettingsPanel")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.emit(SettingsEvent::Dismissed);
                }),
            )
            .child(
                div()
                    .mt(px(96.))
                    .w(px(560.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border)
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(header(&theme))
                    .children(rows)
                    .child(footer(&theme)),
            )
    }
}

fn header(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.text)
                .child("Request settings"),
        )
        // Says out loud that this is per-request. Without it, someone reasonably assumes
        // they've just changed a global default — and only finds out otherwise later.
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child("Applies to this request only"),
        )
}

/// `use<>` is load-bearing. Under edition 2024 an `impl Trait` return captures every
/// in-scope lifetime, so without it the returned element borrows `cx` — and collecting a
/// row per setting into a `Vec` would then be several simultaneous mutable borrows of it.
/// Nothing here actually needs the borrow: `cx.listener` hands back an owned closure.
fn setting_row(
    row: Row,
    value: SharedString,
    active: bool,
    theme: &Theme,
    cx: &mut Context<SettingsPanel>,
) -> impl IntoElement + use<> {
    let index = Row::ALL.iter().position(|candidate| *candidate == row).unwrap_or(0);

    div()
        .id(("setting", index))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .flex_none()
        .px_3()
        .py_1()
        .overflow_hidden()
        .cursor_pointer()
        .bg(if active { theme.bg_hover } else { theme.bg_elevated })
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, _: &MouseDownEvent, _, cx| {
                // Clicking selects *that* row and acts on it, rather than acting on
                // whatever the keyboard had selected.
                panel.selected = index;
                panel.confirm(cx);
            }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .min_w(px(0.))
                .overflow_hidden()
                .child(
                    div()
                        .text_xs()
                        .text_color(if active { theme.text } else { theme.text_muted })
                        .child(row.label()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.border)
                        .child(row.hint()),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(if row.is_action() {
                    theme.accent
                } else {
                    theme.text
                })
                .child(value),
        )
}

fn footer(theme: &Theme) -> impl IntoElement {
    div()
        .flex_none()
        .px_3()
        .py_1()
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child("↑↓ move · ←→ or Enter change · Esc close")
}
