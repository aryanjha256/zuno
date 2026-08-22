//! The response half of a buffer: status, timing, headers, body.
//!
//! Four states, and the in-flight one is the reason the engine reports a stream rather
//! than a single future: status and headers paint at TTFB, and a downloading body shows
//! bytes arriving instead of a frozen pane.
//!
//! The body is rendered through `uniform_list` over a pre-built index, so only the
//! visible rows become elements — that's what keeps a 10MB payload scrolling. Both the
//! JSON outline and the raw-text fallback are virtualized, because a 10MB text body has
//! just as many rows as a 10MB JSON one.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Div, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px, uniform_list,
};
use zuno_core::{
    EngineError, Header, JsonOutline, LineIndex, ResponseData, ResponseDiff, Row, RowKind,
    ScalarKind, StatusClass,
};

use crate::actions::CancelRequest;
use crate::body_view::{BodyKind, BodyNotice, BodyView, is_folded_at};
use crate::request_view::{InFlight, RequestView, ResponseView};
use crate::theme::Theme;

/// Fixed row height. `uniform_list` measures one item and assumes the rest match, so
/// every row must agree — that constraint is exactly what buys O(visible) rendering.
const ROW_HEIGHT: f32 = 18.0;
const INDENT: f32 = 13.0;

pub fn render(
    view: &RequestView,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<RequestView>,
) -> impl IntoElement {
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
        // Read from the keymap rather than written into the copy, so the hint cannot outlive the
        // binding it names — the same reason the command palette does it.
        let cancel = crate::workspace::keybinding_hint(&CancelRequest, window);
        return pane.child(in_flight(inflight, &cancel, theme));
    }
    if let Some(error) = &view.error {
        return pane.child(failure(error, theme));
    }
    match view.displayed() {
        Some(response) => {
            // The status line, the historical notice and the diff bar all describe the
            // response as a whole, so they sit *above* the tabs rather than inside one. The
            // notice especially: it exists to stop the pane being mistaken for the live run,
            // and hiding it behind a tab would reintroduce exactly that.
            let pane = pane
                .child(status_line(response, theme))
                .children(historical_notice(view.viewing(), theme))
                // The diff describes live-vs-previous, so it's meaningless — and wrong —
                // beside an older run.
                .children(
                    (view.viewing() == 0)
                        .then(|| view.diff.as_ref().map(|diff| diff_bar(diff, theme)))
                        .flatten(),
                )
                .child(view_tabs(view, response.headers.len(), theme, cx));

            match view.response_view {
                ResponseView::Body => pane
                    .child(body_header(view, theme, cx))
                    .child(body_region(view, theme, cx)),
                ResponseView::Headers => pane.child(headers_region(&response.headers, theme)),
            }
        }
        None => pane.child(empty_state(theme)),
    }
}

// ---------------------------------------------------------------------------
// Body / Headers tabs
// ---------------------------------------------------------------------------

/// The tab bar over the response detail.
///
/// A tab dispatches `ToggleResponseView` rather than setting the view directly, so the click
/// and `Alt+R` run one path — the "actions, not direct calls" convention.
///
/// **Only the inactive tab is clickable, and that is load-bearing rather than cosmetic.** The
/// action *cycles*, so a handler on both tabs would make clicking the tab you are already on
/// switch away from it — a control that does the opposite of what its label says. Leaving the
/// active tab inert makes "click a tab, land on that tab" true, and it works only because
/// there are exactly two: cycling from the one inactive tab always arrives at it. A third tab
/// would have to split this into per-tab actions.
fn view_tabs(
    view: &RequestView,
    header_count: usize,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> Div {
    let active = view.response_view;

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .flex_none()
        .px_2()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .child(view_tab(
            "response-tab-body",
            "Body".to_string(),
            active == ResponseView::Body,
            theme,
            cx,
        ))
        // The count is on the label because it's the one thing hiding the headers costs you:
        // without it there's no way to tell a response with two headers from one with thirty
        // without switching.
        .child(view_tab(
            "response-tab-headers",
            format!("Headers {header_count}"),
            active == ResponseView::Headers,
            theme,
            cx,
        ))
}

fn view_tab(
    id: &'static str,
    label: String,
    active: bool,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> impl IntoElement + use<> {
    let tab = div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex_none()
        .px_2()
        .py_1()
        // Underlined rather than filled, and the inactive tab keeps the width in the panel's
        // own colour — otherwise switching would shift both labels by 2px.
        .border_b_2()
        .border_color(if active { theme.accent } else { theme.bg_panel })
        .text_xs()
        .text_color(if active { theme.text } else { theme.text_muted })
        .child(label);

    if active {
        // Inert on purpose — see `view_tabs`. No pointer cursor either, or it would advertise
        // a click that does nothing.
        return tab;
    }

    tab.cursor_pointer()
        .hover(|style| style.text_color(theme.text))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _: &MouseDownEvent, window, cx| {
                window.dispatch_action(Box::new(crate::actions::ToggleResponseView), cx);
            }),
        )
}

/// The headers, in their own scrollable region.
///
/// **The scroll is the point of this tab existing.** The table was previously rendered
/// inline above the body with no bound and no scroll, inside a pane that clips — so a
/// response with two dozen headers pushed the body past the bottom edge and left it
/// unreachable. A plain scroll rather than a `uniform_list`: header counts are tens, and
/// virtualizing tens of rows buys nothing while costing the fixed row height that headers,
/// which wrap in principle, shouldn't be forced into.
///
/// `AnyElement` rather than `Div` because `.id()` yields `Stateful<Div>`, so the two branches
/// have different concrete types.
fn headers_region(headers: &[Header], theme: &Theme) -> AnyElement {
    if headers.is_empty() {
        return centered_note("(no headers)", theme).into_any_element();
    }

    div()
        .id("response-headers")
        .flex_1()
        .min_h(px(0.))
        // `overflow_y_scroll` is on `StatefulInteractiveElement`, hence the `.id()` above.
        .overflow_y_scroll()
        .child(headers_table(headers, theme))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// In flight
// ---------------------------------------------------------------------------

/// `cancel` is the keystroke currently bound to `CancelRequest`, or empty if nothing is.
fn in_flight(inflight: &InFlight, cancel: &str, theme: &Theme) -> Div {
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
        // Nothing has arrived yet, so the only useful thing to say is how to give up. If the action
        // is unbound there is no instruction to give, and inventing one would be the original bug.
        (0, _) if cancel.is_empty() => SharedString::from("waiting for the first byte"),
        (0, _) => SharedString::from(format!("{cancel} to cancel")),
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

/// A banner marking the pane as showing a retained run rather than the live one.
fn historical_notice(viewing: usize, theme: &Theme) -> Option<Div> {
    if viewing == 0 {
        return None;
    }
    let label = if viewing == 1 {
        "Showing the previous run".to_string()
    } else {
        format!("Showing the run from {viewing} sends ago")
    };

    Some(
        div()
            .flex_none()
            .px_2()
            .py_1()
            .bg(theme.bg_elevated)
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.accent)
            .child(format!("{label} · Ctrl+H to pick another · send to return to live")),
    )
}

/// What arrived, and the server's declared length when it disagrees.
///
/// **Not a compression indicator, though it reads like one.** The declaration is `Content-Length`,
/// and `tower-http` removes that header along with `Content-Encoding` whenever it decompresses — so
/// on a compressed response there is nothing to compare and the ratio can never be shown. See
/// `SizeInfo`. What does reach the second arm is a `HEAD` or `304`: a length declared with no body
/// behind it, worth showing precisely because it is surprising.
fn size_label(response: &ResponseData) -> String {
    match response.size.declared {
        Some(declared) if declared != response.size.decoded => format!(
            "{} received · {} declared",
            format_bytes(response.size.decoded),
            format_bytes(declared)
        ),
        _ => format_bytes(response.size.decoded),
    }
}

// ---------------------------------------------------------------------------
// Diff against the previous run
// ---------------------------------------------------------------------------

/// One line answering "did my change do anything?".
///
/// Deliberately terse. Timing and size always wobble between runs, so they're shown as
/// context but never make the bar claim something changed — that's `is_quiet`'s job.
fn diff_bar(diff: &ResponseDiff, theme: &Theme) -> Div {
    let (accent, headline) = if diff.is_quiet() {
        (
            theme.text_muted,
            SharedString::from("same as last run"),
        )
    } else if let Some((before, after)) = diff.status {
        (
            theme.status_color(StatusClass::of(after)),
            SharedString::from(format!("status {before} → {after}")),
        )
    } else {
        (theme.accent, SharedString::from("changed since last run"))
    };

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .flex_none()
        .px_3()
        .py_1()
        .bg(theme.bg_elevated)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .text_color(accent)
                .child(headline),
        );

    if diff.body_changed {
        let lines = match diff.line_delta {
            0 => "body changed".to_string(),
            delta => format!("body {delta:+} lines"),
        };
        row = row.child(meta(lines, theme));
    }

    if diff.header_change_count() > 0 {
        let mut parts = Vec::new();
        if !diff.headers_added.is_empty() {
            parts.push(format!("+{}", diff.headers_added.len()));
        }
        if !diff.headers_removed.is_empty() {
            parts.push(format!("-{}", diff.headers_removed.len()));
        }
        if !diff.headers_changed.is_empty() {
            parts.push(format!("~{}", diff.headers_changed.len()));
        }
        row = row.child(meta(format!("headers {}", parts.join(" ")), theme));
    }

    if diff.size_delta != 0 {
        row = row.child(meta(format!("{:+} bytes", diff.size_delta), theme));
    }
    if diff.duration_delta_ms != 0 {
        row = row.child(meta(format!("{:+} ms", diff.duration_delta_ms), theme));
    }

    row
}

fn meta(text: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .flex_none()
        .text_xs()
        .text_color(theme.text_muted)
        .child(text.into())
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

// ---------------------------------------------------------------------------
// Body: virtualized
// ---------------------------------------------------------------------------

/// The Body section header, carrying row count and the fold-all / parse-anyway actions.
fn body_header(view: &RequestView, theme: &Theme, cx: &mut Context<RequestView>) -> Div {
    let detail = match &view.body_view {
        None => SharedString::from("indexing…"),
        Some(body) => match &body.kind {
            BodyKind::Empty => SharedString::from("empty"),
            BodyKind::Binary { len } => {
                SharedString::from(format!("{} binary", format_bytes(*len as u64)))
            }
            BodyKind::Json(outline) => SharedString::from(format!(
                "{} rows · {} shown",
                outline.len(),
                body.row_count()
            )),
            BodyKind::Text(lines) => SharedString::from(format!("{} lines", lines.len())),
        },
    };

    let mut header = div()
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
        .child("Body".to_string());

    let mut actions = div().flex().flex_row().items_center().gap_2().child(detail);

    if view.body_view.as_ref().is_some_and(BodyView::is_json) {
        actions = actions
            .child(text_button("fold-all", "fold all", theme, cx, |view, cx| {
                view.set_all_folded(true, cx)
            }))
            .child(text_button("unfold-all", "expand", theme, cx, |view, cx| {
                view.set_all_folded(false, cx)
            }));
    }

    // The explicit escape hatch for an over-the-cap body. Never parse silently, and
    // never refuse silently either.
    if let Some(BodyNotice::TooLarge { .. }) = view.body_view.as_ref().and_then(|b| b.notice.clone())
    {
        actions = actions.child(text_button(
            "parse-anyway",
            "parse as JSON anyway",
            theme,
            cx,
            |view, cx| view.force_parse_body(cx),
        ));
    }

    header = header.child(actions);
    header
}

fn text_button(
    id: &'static str,
    label: &'static str,
    theme: &Theme,
    cx: &mut Context<RequestView>,
    action: impl Fn(&mut RequestView, &mut Context<RequestView>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_1()
        .rounded_sm()
        .text_color(theme.accent)
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, _: &MouseDownEvent, _, cx| action(view, cx)),
        )
        .child(label.to_string())
}

fn body_region(view: &RequestView, theme: &Theme, cx: &mut Context<RequestView>) -> Div {
    let container = div().flex_1().flex().flex_col().min_h(px(0.));

    let Some(body) = &view.body_view else {
        return container.child(centered_note("Indexing response…", theme));
    };

    let notice = body.notice.as_ref().map(|notice| notice_bar(notice, theme));

    let list = match &body.kind {
        BodyKind::Empty => centered_note("(empty body)", theme).into_any_element(),
        BodyKind::Binary { len } => centered_note(
            &format!("{} of binary data", format_bytes(*len as u64)),
            theme,
        )
        .into_any_element(),
        BodyKind::Json(outline) => {
            json_list(outline.clone(), body.visible(), theme, cx).into_any_element()
        }
        BodyKind::Text(lines) => text_list(lines.clone(), theme).into_any_element(),
    };

    container.children(notice).child(list)
}

/// The virtualized JSON view.
///
/// `uniform_list` only builds elements for the visible range, so this stays O(visible)
/// no matter how many rows exist. The closure captures two `Arc`s and the theme — never
/// the fold flags, which would be a megabyte-scale clone per frame (see `is_folded_at`).
fn json_list(
    outline: Arc<JsonOutline>,
    visible: Arc<Vec<u32>>,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> impl IntoElement {
    let row_theme = theme.clone();
    let mono = theme.mono.clone();
    let view = cx.entity().downgrade();
    let count = visible.len();

    uniform_list("json-body", count, move |range, _window, _cx| {
        range
            .map(|visible_ix| {
                let row_ix = visible[visible_ix] as usize;
                let Some(row) = outline.row(row_ix).copied() else {
                    return div().h(px(ROW_HEIGHT));
                };
                let folded = row.kind.is_open() && is_folded_at(&visible, visible_ix, row_ix);
                json_row(&outline, row, row_ix, folded, &row_theme, view.clone())
            })
            .collect()
    })
    .flex_1()
    .px_2()
    .font_family(mono)
    .text_xs()
}

fn json_row(
    outline: &JsonOutline,
    row: Row,
    row_ix: usize,
    folded: bool,
    theme: &Theme,
    view: gpui::WeakEntity<RequestView>,
) -> Div {
    let mut line = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(ROW_HEIGHT))
        .pl(px(4.0 + row.depth as f32 * INDENT));

    // Fold affordance. Only containers get one, and clicking anywhere on the marker
    // toggles — a 1px chevron would be unusable.
    if row.kind.is_open() {
        let marker = if folded { "▸" } else { "▾" };
        line = line.child(
            div()
                .id(("fold", row_ix))
                .flex_none()
                .w(px(12.))
                .text_color(theme.text_muted)
                .cursor_pointer()
                .hover(|style| style.text_color(theme.accent))
                .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    let _ = view.update(cx, |view, cx| view.toggle_fold(row_ix, cx));
                })
                .child(marker.to_string()),
        );
    } else {
        line = line.child(div().flex_none().w(px(12.)));
    }

    if !row.key.is_none() {
        line = line
            .child(
                div()
                    .flex_none()
                    .text_color(theme.syntax.key)
                    .child(outline.text(row.key).to_string()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.syntax.punct)
                    .child(": ".to_string()),
            );
    }

    let (text, color) = match row.kind {
        RowKind::Scalar(kind) => (
            outline.text(row.value).to_string(),
            match kind {
                ScalarKind::String => theme.syntax.string,
                ScalarKind::Number => theme.syntax.number,
                ScalarKind::Bool | ScalarKind::Null => theme.syntax.literal,
            },
        ),
        RowKind::ObjectOpen if folded => (
            format!("{{ … {} }}", plural(row.child_count)),
            theme.syntax.punct,
        ),
        RowKind::ArrayOpen if folded => (
            format!("[ … {} ]", plural(row.child_count)),
            theme.syntax.punct,
        ),
        RowKind::ObjectOpen => ("{".to_string(), theme.syntax.punct),
        RowKind::ArrayOpen => ("[".to_string(), theme.syntax.punct),
        RowKind::ObjectClose => ("}".to_string(), theme.syntax.punct),
        RowKind::ArrayClose => ("]".to_string(), theme.syntax.punct),
    };

    line = line.child(div().flex_none().text_color(color).child(text));

    if row.trailing_comma {
        line = line.child(
            div()
                .flex_none()
                .text_color(theme.syntax.punct)
                .child(",".to_string()),
        );
    }

    line
}

fn plural(count: u32) -> String {
    if count == 1 {
        "1 item".to_string()
    } else {
        format!("{count} items")
    }
}

/// The virtualized raw-text view — the fallback for non-JSON, over-cap, and
/// failed-to-parse bodies. Also virtualized: a 10MB text body has just as many rows.
fn text_list(lines: Arc<LineIndex>, theme: &Theme) -> impl IntoElement {
    let row_theme = theme.clone();
    let mono = theme.mono.clone();
    let count = lines.len();

    uniform_list("text-body", count, move |range, _window, _cx| {
        range
            .map(|ix| {
                let (text, truncated) = lines.line(ix);
                let mut row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(ROW_HEIGHT))
                    .child(
                        div()
                            .flex_none()
                            .text_color(row_theme.text)
                            .child(text.to_string()),
                    );

                if truncated {
                    row = row.child(
                        div()
                            .flex_none()
                            .pl(px(6.))
                            .text_color(row_theme.text_muted)
                            .child("… line truncated for display".to_string()),
                    );
                }
                row
            })
            .collect()
    })
    .flex_1()
    .px_2()
    .font_family(mono)
    .text_xs()
}

fn notice_bar(notice: &BodyNotice, theme: &Theme) -> Div {
    let (color, message) = match notice {
        BodyNotice::TooLarge { len } => (
            theme.status_client_error,
            format!(
                "{} is over the {} auto-parse limit — showing raw text",
                format_bytes(*len as u64),
                format_bytes(crate::body_view::MAX_AUTO_PARSE as u64)
            ),
        ),
        BodyNotice::ParseFailed { message } => (
            theme.status_server_error,
            format!("not valid JSON: {message} — showing raw text"),
        ),
    };

    div()
        .flex_none()
        .px_3()
        .py_1()
        .bg(theme.bg_elevated)
        .border_b_1()
        .border_color(color)
        .text_xs()
        .text_color(theme.text)
        .child(message)
}

fn centered_note(message: &str, theme: &Theme) -> Div {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(theme.text_muted)
        .child(message.to_string())
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
