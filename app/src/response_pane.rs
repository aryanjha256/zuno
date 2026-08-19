//! The response half of a buffer: status, timing, headers, body.
//!
//! Four states, and the in-flight one is the reason the engine reports a stream rather
//! than a single future: status and headers paint at TTFB, and a downloading body shows
//! bytes arriving instead of a frozen pane.
//!
//! The body renders as plain monospace lines. M1.3 replaces that with `JsonOutline` +
//! `uniform_list` so a 50MB payload stays at 60fps; a throwaway renderer beats a
//! half-built tokenizer in the meantime.

use std::time::Duration;

use gpui::{
    Div, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use zuno_core::{EngineError, Header, ResponseData, StatusClass};

use crate::request_view::{InFlight, RequestView};
use crate::theme::Theme;

pub fn render(view: &RequestView, theme: &Theme, window: &Window) -> impl IntoElement {
    let focused = view.response_focus.is_focused(window);

    let pane = div()
        .id("response-pane")
        .key_context("ResponsePane")
        .track_focus(&view.response_focus)
        .flex_1()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(theme.bg)
        .border_l_1()
        .border_color(theme.focus_border(focused));

    // Order matters: an in-flight request outranks the previous response, and an error
    // outranks a stale success.
    if let Some(inflight) = &view.inflight {
        return pane.child(in_flight(inflight, theme));
    }
    if let Some(error) = &view.error {
        return pane.child(failure(error, theme));
    }
    match &view.response {
        Some(response) => pane
            .child(status_line(response, theme))
            .child(section_header("Headers", theme))
            .child(headers_table(&response.headers, theme))
            .child(section_header("Body", theme))
            .child(body_region(response, theme)),
        None => pane.child(empty_state(theme)),
    }
}

// ---------------------------------------------------------------------------
// In flight
// ---------------------------------------------------------------------------

fn in_flight(inflight: &InFlight, theme: &Theme) -> Div {
    let headline = match &inflight.status {
        // Status known already — the response head arrived, the body is still coming.
        Some((status, text)) => SharedString::from(format!("{status} {text}")),
        None => SharedString::from("Waiting for response…"),
    };
    let status_color = inflight
        .status
        .as_ref()
        .map(|(status, _)| theme.status_color(StatusClass::of(*status)))
        .unwrap_or(theme.text_muted);

    let progress = match (inflight.received, inflight.total) {
        (0, _) => SharedString::from("Ctrl+C or Escape to cancel"),
        (received, Some(total)) => SharedString::from(format!(
            "{} of {} ({:.0}%)",
            format_bytes(received as u64),
            format_bytes(total as u64),
            (received as f64 / total.max(1) as f64) * 100.0
        )),
        (received, None) => SharedString::from(format!("{} received", format_bytes(received as u64))),
    };

    let mut column = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(status_color)
                .child(headline),
        )
        .child(div().text_xs().text_color(theme.text_muted).child(progress));

    if let Some(ttfb) = inflight.ttfb {
        column = column.child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child(format!("TTFB {}", format_duration(ttfb))),
        );
    }

    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .p_4()
        .child(column)
}

// ---------------------------------------------------------------------------
// Failure
// ---------------------------------------------------------------------------

fn failure(error: &EngineError, theme: &Theme) -> Div {
    // A local failure means nothing left the machine — worth saying, because it tells
    // the user this is their request to fix, not the network's.
    let (label, label_color) = if error.is_local() {
        ("Request not sent", theme.status_client_error)
    } else {
        ("Request failed", theme.status_server_error)
    };

    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .p_4()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(label_color)
                .child(label.to_string()),
        )
        .child(
            div()
                .max_w(px(460.))
                .text_xs()
                .text_color(theme.text)
                .child(error.to_string()),
        )
}

// ---------------------------------------------------------------------------
// Completed response
// ---------------------------------------------------------------------------

fn status_line(response: &ResponseData, theme: &Theme) -> Div {
    let status_color = theme.status_color(response.status_class());

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.bg_elevated)
                .border_1()
                .border_color(status_color)
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(status_color)
                .child(format!("{} {}", response.status, response.status_text)),
        )
        .child(meta(response.version.as_str().to_string(), theme))
        .child(meta(format_duration(response.timing.total), theme))
        .child(meta(
            format!("TTFB {}", format_duration(response.timing.ttfb)),
            theme,
        ))
        .child(meta(size_label(response), theme))
}

/// Wire size is only interesting when it differs from decoded — that difference is how
/// you see compression happened. reqwest drops Content-Length once it decompresses, in
/// which case there is nothing to compare.
fn size_label(response: &ResponseData) -> String {
    if response.size.wire == response.size.decoded {
        format_bytes(response.size.decoded)
    } else {
        format!(
            "{} on the wire · {} decoded",
            format_bytes(response.size.wire),
            format_bytes(response.size.decoded)
        )
    }
}

fn meta(text: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .flex_none()
        .text_xs()
        .text_color(theme.text_muted)
        .child(text.into())
}

fn section_header(title: &str, theme: &Theme) -> Div {
    div()
        .px_3()
        .py_1()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child(title.to_string())
}

fn headers_table(headers: &[Header], theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .children(headers.iter().map(|header| header_row(header, theme)))
}

fn header_row(header: &Header, theme: &Theme) -> Div {
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
                .w(px(180.))
                .truncate()
                .text_color(theme.text)
                .child(header.name.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(theme.text_muted)
                .child(header.value.clone()),
        )
}

fn body_region(response: &ResponseData, theme: &Theme) -> impl IntoElement {
    let lines: Vec<String> = match response.body_as_str() {
        Some("") => vec!["(empty body)".to_string()],
        Some(text) => text.lines().map(str::to_string).collect(),
        // A non-UTF-8 body is a normal outcome, not an error. M1.3 gives it a real hex
        // view.
        None => vec![format!(
            "{} of binary data ({})",
            format_bytes(response.body.len() as u64),
            response.content_type().unwrap_or("unknown content type")
        )],
    };

    div()
        .id("response-body")
        .flex_1()
        .min_h(px(0.))
        .p_2()
        .overflow_y_scroll()
        .font_family(theme.mono.clone())
        .text_xs()
        .text_color(theme.text)
        .child(div().flex().flex_col().children(lines))
}

fn empty_state(theme: &Theme) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .text_sm()
        .text_color(theme.text_muted)
        .child("No response yet".to_string())
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child("Ctrl+Enter to send".to_string()),
        )
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Sub-millisecond responses are real (localhost), so don't round them to "0 ms".
fn format_duration(duration: Duration) -> String {
    let ms = duration.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{ms:.2} ms")
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let bytes = bytes as f64;
    if bytes < KB {
        format!("{bytes:.0} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes / KB)
    } else if bytes < GB {
        format!("{:.1} MB", bytes / MB)
    } else {
        format!("{:.2} GB", bytes / GB)
    }
}
