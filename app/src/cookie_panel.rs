//! The cookie viewer: what the jar holds, and a way to forget one.
//!
//! A plain state struct rendered by a function, `cert_panel`'s shape and for its reason — it owns
//! no text input, so its whole state is which row is selected. **The cookies are read from the
//! engine at render time**, never copied in: a response that lands while the panel is open adds
//! its cookie to the list, and one that expires leaves it, with nothing to keep in step.
//!
//! Grouped by domain, because that is how someone scans for one site's session — and because the
//! heading is where `host_only` belongs: whether a cookie reaches subdomains is a fact about the
//! domain it was set for, not about its name.

use gpui::{
    App, FocusHandle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};
use zuno_core::engine::StoredCookie;

use crate::actions::{ClearCookies, CookiesRemove};
use crate::theme::Theme;
use crate::workspace::Workspace;

const WIDTH: f32 = 620.;
/// Tall enough for a working set of sessions, short enough to stay a panel over the panes.
const LIST_HEIGHT: f32 = 360.;

pub struct CookiePanel {
    pub selected: usize,
    pub restore_focus: Option<FocusHandle>,
    pub focus_handle: FocusHandle,
}

impl CookiePanel {
    pub fn new(restore_focus: Option<FocusHandle>, cx: &mut App) -> Self {
        Self {
            selected: 0,
            restore_focus,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn step(&mut self, delta: isize, len: usize) {
        self.selected = stepped(self.selected, delta, len);
    }

    /// Keep the selection on a row that exists — the list shrinks when a cookie is removed or
    /// expires, and grows when a response sets one.
    pub fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

/// One step through `len` rows, wrapping at both ends.
fn stepped(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as isize + delta).rem_euclid(len as isize) as usize
}

/// When a cookie stops being sent, relative to now, in the largest unit that is at least one.
///
/// **Relative, not a date**, because the question is "will this still be here when I send the
/// next request", and `2026-09-24T11:03:00Z` makes you do the subtraction.
pub fn expiry_label(expires: Option<i64>, now: i64) -> String {
    let Some(at) = expires else {
        return "session".to_string();
    };
    let left = at - now;
    if left <= 0 {
        return "expired".to_string();
    }
    let (amount, unit) = match left {
        secs if secs < 60 => (secs, "s"),
        secs if secs < 3_600 => (secs / 60, "m"),
        secs if secs < 86_400 => (secs / 3_600, "h"),
        secs if secs < 86_400 * 365 => (secs / 86_400, "d"),
        secs => (secs / (86_400 * 365), "y"),
    };
    format!("in {amount}{unit}")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

pub fn render(
    state: &CookiePanel,
    cookies: &[StoredCookie],
    theme: &Theme,
    cx: &mut gpui::Context<Workspace>,
) -> impl IntoElement + use<> {
    let now = now();
    let mut list = div()
        .id("cookie-list")
        .flex()
        .flex_col()
        .max_h(px(LIST_HEIGHT))
        .overflow_y_scroll();

    let mut last_domain: Option<(&str, bool)> = None;
    for (ix, cookie) in cookies.iter().enumerate() {
        let domain = (cookie.domain.as_str(), cookie.host_only);
        if last_domain != Some(domain) {
            list = list.child(heading(cookie, theme));
            last_domain = Some(domain);
        }
        list = list.child(entry(ix, cookie, ix == state.selected, now, theme, cx));
    }
    if cookies.is_empty() {
        list = list.child(
            div()
                .px_3()
                .py_3()
                .text_xs()
                .text_color(theme.text_faint)
                .child("No cookies stored. A response's Set-Cookie header lands here."),
        );
    }

    let count = match cookies.len() {
        1 => "1 cookie".to_string(),
        n => format!("{n} cookies"),
    };

    div()
        .id("cookie-panel-scrim")
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
                .key_context("CookiePanel")
                .w(px(WIDTH))
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg_elevated)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().text_sm().text_color(theme.text).child("Cookies"))
                        .child(div().text_xs().text_color(theme.text_faint).child(count))
                        .child(div().flex_1())
                        .children((!cookies.is_empty()).then(|| {
                            crate::ui::text_action(
                                "cookies-clear",
                                "Clear all".into(),
                                "Forget every stored cookie",
                                ClearCookies,
                                theme,
                            )
                        })),
                )
                .child(list)
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_size(px(10.))
                        .text_color(theme.text_faint)
                        .child("delete removes · shift-delete clears all · escape closes"),
                ),
        )
}

fn heading(cookie: &StoredCookie, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap_2()
        .px_3()
        .pt_2()
        .pb_1()
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme.text_muted)
                .child(cookie.domain.clone()),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                // What the one-word difference in `Set-Cookie` actually changes.
                .child(if cookie.host_only {
                    "this host only"
                } else {
                    "and its subdomains"
                }),
        )
}

fn entry(
    ix: usize,
    cookie: &StoredCookie,
    selected: bool,
    now: i64,
    theme: &Theme,
    cx: &mut gpui::Context<Workspace>,
) -> impl IntoElement + use<> {
    let mut detail: Vec<String> = Vec::new();
    // `/` is nearly every cookie's path and says nothing; anything narrower is worth knowing.
    if cookie.path != "/" {
        detail.push(cookie.path.clone());
    }
    if cookie.secure {
        detail.push("secure".to_string());
    }
    if cookie.http_only {
        detail.push("httponly".to_string());
    }
    detail.push(expiry_label(cookie.expires, now));

    div()
        .id(("cookie-row", ix))
        .debug_selector(move || format!("cookie-row-{ix}"))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .text_xs()
        .cursor_pointer()
        .bg(if selected { theme.bg_hover } else { theme.bg_elevated })
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |workspace, _: &MouseDownEvent, _, cx| {
                workspace.select_cookie_row(ix, cx);
            }),
        )
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_color(if selected { theme.text } else { theme.text_muted })
                .font_family(theme.mono.clone())
                .child(cookie.name.clone()),
        )
        // **Shortened in Rust, not truncated by layout** — `truncate()` is not dependable here
        // (CLAUDE.md, traps), and a session token is exactly the long string that needs it.
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(theme.text_faint)
                .font_family(theme.mono.clone())
                .child(zuno_core::request::elide(&cookie.value, 48).into_owned()),
        )
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .child(detail.join(" · ")),
        )
        .child(
            div()
                .id(("cookie-remove", ix))
                .debug_selector(move || format!("cookie-remove-{ix}"))
                .flex_none()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .hover(|style| style.text_color(theme.status_server_error))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                        // A clickable inside a clickable: the row's own handler would run too.
                        cx.stop_propagation();
                        workspace.select_cookie_row(ix, cx);
                        window.dispatch_action(Box::new(CookiesRemove), cx);
                    }),
                )
                .child("remove"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expiry_reads_as_the_time_left() {
        let now = 1_000_000;
        assert_eq!(expiry_label(None, now), "session");
        assert_eq!(expiry_label(Some(now + 30), now), "in 30s");
        assert_eq!(expiry_label(Some(now + 90), now), "in 1m");
        assert_eq!(expiry_label(Some(now + 3 * 3_600 + 5), now), "in 3h");
        assert_eq!(expiry_label(Some(now + 2 * 86_400), now), "in 2d");
        assert_eq!(expiry_label(Some(now + 400 * 86_400), now), "in 1y");
        assert_eq!(expiry_label(Some(now - 1), now), "expired");
    }

    #[test]
    fn the_selection_wraps_at_both_ends() {
        assert_eq!(stepped(0, -1, 3), 2, "up from the top wraps to the bottom");
        assert_eq!(stepped(2, 1, 3), 0, "down from the bottom wraps to the top");
        assert_eq!(stepped(0, 1, 0), 0, "an empty list has nothing to step through");
    }
}
