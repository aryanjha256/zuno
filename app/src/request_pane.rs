//! The request half of a buffer: method, URL, headers, body.
//!
//! Everything here is read-only in M1.0 — the point of this milestone is that
//! layout, theming, and focus are correct before any text editing exists. The URL
//! bar and body region are real focus targets with real key contexts; they just
//! don't accept keystrokes yet. `TextInput` lands in M1.1.

use gpui::{
    Div, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use zuno_core::{Header, QueryParam, RequestSpec};

use crate::request_view::RequestView;
use crate::theme::Theme;

pub fn render(view: &RequestView, theme: &Theme, window: &Window) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(theme.bg)
        .child(toolbar(view, theme, window))
        .child(section_header("Headers", header_count_label(&view.spec), theme))
        .child(headers_table(&view.spec, theme))
        .child(section_header("Query", query_count_label(&view.spec), theme))
        .child(query_table(&view.spec, theme))
        .child(section_header(
            "Body",
            SharedString::from(view.spec.body.label()),
            theme,
        ))
        .child(body_region(view, theme, window))
}

fn toolbar(view: &RequestView, theme: &Theme, window: &Window) -> Div {
    let url_focused = view.url_focus.is_focused(window);

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
        .child(method_chip(view, theme))
        .child(url_bar(view, theme, url_focused))
        .child(send_button(theme))
}

fn method_chip(view: &RequestView, theme: &Theme) -> Div {
    div()
        .flex_none()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(theme.method_color(&view.spec.method))
        .child(view.spec.method.as_str().to_string())
}

fn url_bar(view: &RequestView, theme: &Theme, focused: bool) -> impl IntoElement {
    let url = if view.spec.url.is_empty() {
        SharedString::from("Enter a URL…")
    } else {
        SharedString::from(view.spec.url.clone())
    };
    let color = if view.spec.url.is_empty() { theme.text_muted } else { theme.text };

    div()
        .id("url-bar")
        // A key context is what lets `enter` mean "send" here and "newline" in
        // the body editor. Set from the start; unfixable later if every binding
        // is registered globally. See architecture.md §5.
        .key_context("UrlBar")
        .track_focus(&view.url_focus)
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
        .text_color(color)
        .truncate()
        .child(url)
}

fn send_button(theme: &Theme) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(theme.accent)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_on_accent)
        .child("Send".to_string())
}

fn section_header(title: &str, detail: SharedString, theme: &Theme) -> Div {
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
        .child(detail)
}

fn header_count_label(spec: &RequestSpec) -> SharedString {
    SharedString::from(format!(
        "{} of {} enabled",
        spec.enabled_headers().count(),
        spec.headers.len()
    ))
}

fn query_count_label(spec: &RequestSpec) -> SharedString {
    SharedString::from(format!(
        "{} of {} enabled",
        spec.enabled_query().count(),
        spec.query.len()
    ))
}

fn headers_table(spec: &RequestSpec, theme: &Theme) -> Div {
    if spec.headers.is_empty() {
        return empty_row("No headers", theme);
    }

    div()
        .flex()
        .flex_col()
        .children(spec.headers.iter().map(|header| header_row(header, theme)))
}

fn header_row(header: &Header, theme: &Theme) -> Div {
    key_value_row(header.enabled, &header.name, &header.value, theme)
}

fn query_table(spec: &RequestSpec, theme: &Theme) -> Div {
    if spec.query.is_empty() {
        return empty_row("No query parameters", theme);
    }

    div()
        .flex()
        .flex_col()
        .children(spec.query.iter().map(|param| query_row(param, theme)))
}

fn query_row(param: &QueryParam, theme: &Theme) -> Div {
    key_value_row(param.enabled, &param.name, &param.value, theme)
}

/// One row of the headers/query tables. A disabled row stays visible and legible
/// but visibly muted — muting a row without deleting it is a core interaction.
fn key_value_row(enabled: bool, name: &str, value: &str, theme: &Theme) -> Div {
    let text_color = if enabled { theme.text } else { theme.text_muted };
    let marker_color = if enabled { theme.accent } else { theme.border };

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
        .child(
            div()
                .flex_none()
                .w(px(6.))
                .h(px(6.))
                .rounded_full()
                .bg(marker_color),
        )
        .child(
            div()
                .flex_none()
                .w(px(180.))
                .truncate()
                .text_color(text_color)
                .child(name.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(theme.text_muted)
                .child(value.to_string()),
        )
}

fn empty_row(label: &str, theme: &Theme) -> Div {
    div()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child(label.to_string())
}

fn body_region(view: &RequestView, theme: &Theme, window: &Window) -> impl IntoElement {
    let focused = view.body_focus.is_focused(window);
    let lines: Vec<String> = match view.spec.body.as_text() {
        Some(text) => text.lines().map(str::to_string).collect(),
        None => vec!["(no body)".to_string()],
    };

    div()
        .id("body-editor")
        .key_context("BodyEditor")
        .track_focus(&view.body_focus)
        .flex_1()
        .min_h(px(0.))
        .m_2()
        .p_2()
        .rounded_md()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.focus_border(focused))
        .overflow_y_scroll()
        .font_family(theme.mono.clone())
        .text_xs()
        .text_color(theme.text)
        .child(div().flex().flex_col().children(lines))
}
