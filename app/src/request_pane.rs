//! The request half of a buffer: method, URL, headers, query, body.
//!
//! Fully editable: the URL and every table cell is a `TextInput`, the body is a
//! multi-line `Editor`, and rows can be added, muted, and removed by keyboard or mouse.
//!
//! These are functions rather than an entity, but they take `&mut Context<RequestView>`
//! so they can build `cx.listener` click handlers. `&RequestView` and
//! `&mut Context<RequestView>` are independent borrows in GPUI, so passing both is
//! fine.

use gpui::{
    Div, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Styled, Window, div, px,
};

use crate::actions::{CancelRequest, SendRequest};
use crate::request_view::{KeyValueRow, RequestView, RowKind};
use crate::theme::Theme;

pub fn render(
    view: &RequestView,
    theme: &Theme,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    // Read focus state before any `&mut cx` use below — the immutable borrow from
    // `read` has to end first.
    let url_focused = view.url_focus(cx).is_focused(window);
    let body_focused = view.body_focus(cx).is_focused(window);
    let body_lines = view.body_editor.read(cx).line_count();

    let header_detail = count_label(
        view.headers.iter().filter(|row| row.enabled).count(),
        view.headers.len(),
    );
    let query_detail = count_label(
        view.query.iter().filter(|row| row.enabled).count(),
        view.query.len(),
    );

    div()
        .flex_1()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(theme.bg)
        .child(toolbar(view, theme, url_focused, cx))
        .child(section_header(
            "Headers",
            header_detail,
            RowKind::Header,
            theme,
            cx,
        ))
        .child(rows_table(&view.headers, RowKind::Header, theme, cx))
        .child(section_header(
            "Query",
            query_detail,
            RowKind::Query,
            theme,
            cx,
        ))
        .child(rows_table(&view.query, RowKind::Query, theme, cx))
        .child(body_header(view, body_lines, theme, cx))
        .child(body_region(view, theme, body_focused))
}

/// The Body header, with a clickable body-kind chip (`Ctrl+Shift+B` does the same).
fn body_header(
    view: &RequestView,
    lines: usize,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .px_3()
        .py_1()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child("Body".to_string())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(format!("{lines} lines"))
                .child(
                    div()
                        .id("body-kind")
                        .px_1()
                        .rounded_sm()
                        .text_color(theme.accent)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.bg_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _: &MouseDownEvent, _, cx| {
                                view.cycle_body_kind(cx)
                            }),
                        )
                        .child(view.body_label()),
                ),
        )
}

fn toolbar(
    view: &RequestView,
    theme: &Theme,
    url_focused: bool,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .child(method_chip(view, theme, cx))
        .child(url_bar(view, theme, url_focused))
        .child(send_button(theme, view.is_sending(), cx))
}

/// Clicking cycles the method; right-click cycles back. A real dropdown needs an
/// anchored popover, which isn't worth building before the send loop works —
/// `Ctrl+M` / `Ctrl+Shift+M` do the same thing from the keyboard.
fn method_chip(
    view: &RequestView,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> impl IntoElement {
    div()
        .id("method-chip")
        .flex_none()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(theme.method_color(&view.method))
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _: &MouseDownEvent, _, cx| view.cycle_method(true, cx)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|view, _: &MouseDownEvent, _, cx| view.cycle_method(false, cx)),
        )
        .child(view.method.as_str().to_string())
}

fn url_bar(view: &RequestView, theme: &Theme, focused: bool) -> Div {
    div()
        .flex_1()
        .min_w(px(0.))
        .px_2()
        .py_1()
        .rounded_md()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.focus_border(focused))
        .font_family(theme.mono.clone())
        .text_sm()
        .text_color(theme.text)
        .child(view.url.clone())
}

/// One button, two states. While a request is in flight the only useful thing it can
/// do is abandon it, so it says so rather than offering a second Send.
///
/// Both branches dispatch an action rather than calling the logic directly, so the
/// button and its keybinding can never drift apart.
fn send_button(theme: &Theme, sending: bool, cx: &mut gpui::Context<RequestView>) -> impl IntoElement {
    let base = div()
        .id("send-button")
        .flex_none()
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_on_accent)
        .cursor_pointer()
        .hover(|style| style.opacity(0.85));

    if sending {
        base.bg(theme.status_client_error)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, window, cx| {
                    window.dispatch_action(Box::new(CancelRequest), cx);
                }),
            )
            .child("Cancel".to_string())
    } else {
        base.bg(theme.accent)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, window, cx| {
                    window.dispatch_action(Box::new(SendRequest), cx);
                }),
            )
            .child("Send".to_string())
    }
}

fn count_label(enabled: usize, total: usize) -> SharedString {
    if total == 0 {
        SharedString::from("empty")
    } else {
        SharedString::from(format!("{enabled} of {total} enabled"))
    }
}

/// Section header with an "+ Add" affordance on the right.
fn section_header(
    title: &str,
    detail: SharedString,
    kind: RowKind,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    let add_id: &'static str = match kind {
        RowKind::Header => "add-header",
        RowKind::Query => "add-query",
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_3()
        .py_1()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child(title.to_string())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(detail)
                .child(
                    div()
                        .id(add_id)
                        .px_1()
                        .rounded_sm()
                        .text_color(theme.accent)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.bg_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _: &MouseDownEvent, window, cx| {
                                view.add_row(kind, window, cx)
                            }),
                        )
                        .child("+ add".to_string()),
                ),
        )
}


fn rows_table(
    rows: &[KeyValueRow],
    kind: RowKind,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    if rows.is_empty() {
        return div()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.text_muted)
            .child(match kind {
                RowKind::Header => "No headers — Ctrl+Shift+H to add".to_string(),
                RowKind::Query => "No query parameters — Ctrl+Shift+Y to add".to_string(),
            });
    }

    div().flex().flex_col().children(
        rows.iter()
            .enumerate()
            .map(|(ix, row)| render_row(row, kind, ix, theme, cx)),
    )
}

fn render_row(
    row: &KeyValueRow,
    kind: RowKind,
    ix: usize,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    let marker_color = if row.enabled {
        theme.accent
    } else {
        theme.border
    };
    let text_color = if row.enabled {
        theme.text
    } else {
        theme.text_muted
    };

    let prefix = match kind {
        RowKind::Header => "hdr",
        RowKind::Query => "qry",
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .hover(|style| style.bg(theme.bg_hover))
        .font_family(theme.mono.clone())
        .text_xs()
        .text_color(text_color)
        // The enabled toggle. Clicking mutes the row without disturbing its text.
        .child(
            div()
                .id(SharedString::from(format!("{prefix}-toggle-{ix}")))
                .flex_none()
                .w(px(10.))
                .h(px(10.))
                .rounded_full()
                .bg(marker_color)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.toggle_row_at(kind, ix, cx)
                    }),
                ),
        )
        .child(div().flex_none().w(px(160.)).child(row.name.clone()))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(theme.text_muted)
                .child(row.value.clone()),
        )
        .child(
            div()
                .id(SharedString::from(format!("{prefix}-remove-{ix}")))
                .flex_none()
                .px_1()
                .rounded_sm()
                .text_color(theme.text_muted)
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover).text_color(theme.status_server_error))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.remove_row_at(kind, ix, cx)
                    }),
                )
                .child("×".to_string()),
        )
}

/// The editor entity renders itself; this only supplies the frame, the focus ring, and
/// the inherited text style it shapes with.
fn body_region(view: &RequestView, theme: &Theme, focused: bool) -> impl IntoElement {
    div()
        .flex_1()
        .min_h(px(0.))
        .m_2()
        .p_2()
        .rounded_md()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.focus_border(focused))
        .font_family(theme.mono.clone())
        .text_xs()
        .text_color(theme.text)
        .child(view.body_editor.clone())
}
