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
    ListHorizontalSizingBehavior, ParentElement, Pixels, SharedString, StatefulInteractiveElement,
    Styled, UniformListScrollHandle,
    Window, div, px, uniform_list,
};
use zuno_core::{
    EngineError, Header, JsonOutline, LineIndex, ResponseData, ResponseDiff, Row, RowKind,
    ScalarKind, StatusClass,
};

use crate::actions::{
    CancelRequest, CopyResponse, FindInResponse, FoldAll, OpenRowMenu, SaveResponse, SendRequest,
    ShowHistory, ToggleFold, UnfoldAll,
};
use crate::ui::{HScrollIndicator, Icon, icon_button, text_action};
use gpui::Action as _;
use crate::body_view::{BodyKind, BodyNotice, BodyView, is_folded_at};
use crate::request_view::{InFlight, RequestView, ResponseSearch, ResponseView};
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
                .children(historical_notice(view.viewing(), theme, window))
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
                    // Above the body rather than floating over it: an overlay would cover the
                    // first rows, which are exactly where a match near the top of the document
                    // is about to be scrolled to.
                    .children(view.search.as_ref().map(|search| {
                        find_bar(search, view.body_view.as_ref().is_some_and(BodyView::is_json), theme, cx)
                    }))
                    .child(body_region(view, theme, window, cx)),
                ResponseView::Headers => {
                    pane.child(headers_region(&response.headers, &view.headers_scroll, theme))
                }
            }
        }
        None => pane.child(empty_state(theme, window)),
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
        // Pushes the action row to the far right; the tabs stay left where the eye starts.
        .child(div().flex_1())
        .child(response_actions(theme))
}

/// The verbs that act on a response, as icon buttons.
///
/// **These were keyboard-only until an audit counted.** Find, copy, save and history were reachable
/// by shortcut or by the command palette and by nothing you could see — which for a new user means
/// they did not exist. Each button dispatches its action and carries a tooltip naming its key, so
/// the mouse path teaches the keyboard one instead of competing with it.
fn response_actions(theme: &Theme) -> Div {
    // **The row verbs deliberately do not live here.** `value` and `path` labels sat in this
    // row for one slice, rendered only once a row was selected — which made the mouse path
    // findable only by someone who already knew the keyboard path, a smaller copy of the very
    // gap the discoverability audit was about. They also made this row shift as the selection
    // changed. Right-clicking a row is the discoverable gesture, and it needs no standing
    // control; these four are whole-response verbs and are always applicable.
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .flex_none()
        .child(icon_button(
            "action-find",
            Icon::Search,
            "Find in response",
            FindInResponse,
            theme,
        ))
        .child(icon_button(
            "action-copy-body",
            Icon::Copy,
            "Copy response body",
            CopyResponse,
            theme,
        ))
        .child(icon_button(
            "action-save-body",
            Icon::Download,
            "Save response body to a file",
            SaveResponse,
            theme,
        ))
        .child(icon_button(
            "action-history",
            Icon::History,
            "Show response history",
            ShowHistory,
            theme,
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

// ---------------------------------------------------------------------------
// Find in response
// ---------------------------------------------------------------------------

/// The find bar: query, position, and every way the count can mislead.
///
/// Three notices, and each exists because the honest count and the useful count differ:
///
/// - **`first N`** when the scan stopped at `search::MAX_MATCHES`. Without it, "5000" reads as
///   the total when it means "at least".
/// - **`past the 4KB line limit`** when the current match sits beyond where the raw view cuts
///   its line. The row is on screen and the match isn't, which otherwise looks like a bug in
///   the search rather than a limit on the display.
/// - **`searching the raw bytes`** on a JSON body, because the count is over the source and not
///   over what's drawn: structural whitespace is searchable, a key includes its quotes, and a
///   folded container's `{ … 3 items }` summary is not in the bytes at all.
fn find_bar(
    search: &ResponseSearch,
    is_json: bool,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> Div {
    let query_is_empty = search.query.read(cx).text().is_empty();

    let (status, status_color) = match search.position() {
        Some((at, total)) => (
            SharedString::from(format!("{at} of {total}")),
            theme.text_muted,
        ),
        None if query_is_empty => (SharedString::from(""), theme.text_muted),
        None => (SharedString::from("no matches"), theme.status_client_error),
    };

    let mut notes: Vec<SharedString> = Vec::new();
    if search.truncated {
        notes.push(SharedString::from(format!(
            "first {} only",
            zuno_core::search::MAX_MATCHES
        )));
    }
    if search.current_clipped {
        notes.push(SharedString::from("past this line's display limit"));
    }
    if is_json && !query_is_empty {
        notes.push(SharedString::from("matching raw bytes"));
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .flex_none()
        .px_3()
        .py_1()
        .bg(theme.bg_elevated)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.bg)
                .border_1()
                .border_color(theme.border)
                .font_family(theme.mono.clone())
                .text_color(theme.text)
                .child(search.query.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_color(status_color)
                .child(status),
        )
        .children(notes.into_iter().map(|note| {
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(note)
        }))
        // Both step through the same action a keystroke does, so the buttons and Enter can't
        // drift. Rendered as text rather than icons for the same reason the rest of this pane
        // is: there is no icon set.
        .child(step_button("find-prev", "‹", crate::actions::FindPrev, theme, cx))
        .child(step_button("find-next", "›", crate::actions::FindNext, theme, cx))
        .child(step_button("find-close", "×", crate::actions::CloseFind, theme, cx))
}

/// `use<A>` rather than `use<>`: the return has to mention every type parameter in scope, and
/// `A` is genuinely captured by the click closure. The empty form is only for helpers that
/// borrow nothing *and* are non-generic.
fn step_button<A: gpui::Action + Clone + 'static>(
    id: &'static str,
    glyph: &'static str,
    action: A,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> impl IntoElement + use<A> {
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex_none()
        .px_1()
        .rounded_sm()
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover).text_color(theme.text))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                window.dispatch_action(Box::new(action.clone()), cx);
            }),
        )
        .child(glyph.to_string())
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
fn headers_region(
    headers: &[Header],
    headers_scroll: &gpui::ScrollHandle,
    theme: &Theme,
) -> AnyElement {
    if headers.is_empty() {
        return centered_note("(no headers)", theme).into_any_element();
    }

    div()
        .id("response-headers")
        .debug_selector(|| "response-headers".to_string())
        .track_scroll(headers_scroll)
        .flex_1()
        .min_h(px(0.))
        // **Vertical only, on purpose — long values wrap instead of scrolling sideways.** This
        // tab did scroll horizontally for one revision, by making the container a flex row so
        // the table could exceed it, and that was the wrong answer twice over: a short table no
        // longer filled the pane, and scrolling sideways carried the *name* column off the left
        // edge, so you lost track of which header you were reading.
        //
        // Wrapping is what §6 said this tab wanted all along. It is not virtualized and header
        // counts are in the tens, so a variable row height costs nothing here — which is exactly
        // why the body, which is virtualized on fixed-height rows, cannot do the same.
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
        .gap_2()
        .flex_none()
        // Matches the address bar exactly, so the two panes' first rows line up and every row
        // below them does too. A fixed height rather than `py_2`, which is what made it ~34px
        // against the bar's 30 — see `ui::BAR_HEIGHT`.
        .h(px(crate::ui::BAR_HEIGHT))
        .px_3()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        // The status was a filled, bordered pill. It is now plain bold text in the status
        // colour, divided from the rest by the same rule the titlebar uses — one separated
        // list rather than a badge followed by a cloud of grey values.
        .child(
            div()
                .flex_none()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(status_color)
                .child(format!("{} {}", response.status, response.status_text)),
        )
        .child(crate::ui::separator(theme))
        .child(meta(response.version.as_str().to_string(), theme))
        .child(crate::ui::separator(theme))
        .child(meta(format_duration(response.timing.total), theme))
        .child(crate::ui::separator(theme))
        .child(meta(
            format!("TTFB {}", format_duration(response.timing.ttfb)),
            theme,
        ))
        .child(crate::ui::separator(theme))
        .child(meta(size_label(response), theme))
}

/// A banner marking the pane as showing a retained run rather than the live one.
fn historical_notice(viewing: usize, theme: &Theme, window: &Window) -> Option<Div> {
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
            .child(match crate::workspace::keybinding_label(&ShowHistory, window) {
                // Unbound: drop the clause rather than name a key that does nothing. The
                // "send to return to live" half needs no key, so it always stays.
                key if key.is_empty() => format!("{label} · send to return to live"),
                key => format!("{label} · {key} to pick another · send to return to live"),
            }),
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
        .debug_selector(|| "header-row".to_string())
        .flex()
        .flex_row()
        // `items_start`, not `items_center`: a wrapped value is several lines tall and its name
        // should sit beside the first of them, not float in the middle of the block.
        .items_start()
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
        // No `truncate`: that is what clipped a JWT or a CSP header to the pane's width with no
        // way to reach the rest. Without it the value wraps onto as many lines as it needs.
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
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
        // **Dispatch, not a direct call.** These two were calling `set_all_folded` straight on the
        // view, so the buttons and `Alt+F`/`Alt+E` were separate code paths that could drift —
        // exactly what the body-kind chip was caught doing, and what "actions, not direct calls"
        // exists to prevent. `text_action` also gives them the tooltip the icons have.
        actions = actions
            .child(text_action(
                "fold-all",
                "fold all".into(),
                "Fold all",
                FoldAll,
                theme,
            ))
            .child(text_action(
                "unfold-all",
                "expand".into(),
                "Unfold all",
                UnfoldAll,
                theme,
            ));
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

/// The width the widest row needs, in the font the rows are actually drawn in.
///
/// One `shape_line` of a ten-character sample per frame — negligible, and the only way to get an
/// advance that matches what the viewer paints. See `BodyView::widest_extent` for why measuring
/// this ourselves beats letting `uniform_list` measure a row.
fn content_width(view: &RequestView, theme: &Theme, window: &Window) -> Pixels {
    let Some(body) = &view.body_view else {
        return px(0.);
    };
    let (depth, chars) = body.widest_extent();
    if chars == 0 {
        return px(0.);
    }

    const SAMPLE: &str = "0123456789";
    let font_size = window.rem_size() * 0.75;
    let font = gpui::font(theme.mono.clone());
    let runs = [gpui::TextRun {
        len: SAMPLE.len(),
        font,
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    }];
    let advance = window
        .text_system()
        .shape_line(SAMPLE.into(), font_size, &runs, None)
        .width
        / SAMPLE.len() as f32;

    // Indent, fold marker, the text itself, and a little slack so rounding never clips the last
    // glyph — over-reaching by a few pixels is invisible, falling short is the bug.
    px(4.) + px(depth as f32 * INDENT) + px(12.) + advance * chars as f32 + px(8.)
}

fn body_region(
    view: &RequestView,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<RequestView>,
) -> Div {
    // `relative` so the scroll indicator can sit over the bottom edge of the list rather than
    // taking a row of layout from it.
    let container = div().flex_1().flex().flex_col().min_h(px(0.)).relative();

    let Some(body) = &view.body_view else {
        return container.child(centered_note("Indexing response…", theme));
    };

    let notice = body.notice.as_ref().map(|notice| notice_bar(notice, theme));

    // The row holding the current match, and the row the reader has selected. Both are
    // plain `Option<u32>` and therefore `Copy`, so the render closures capture them without
    // borrowing the view. They are separate on purpose: a match is where the *search* is, a
    // selection is where *you* are, and a row is routinely both.
    let hit = view.current_match_row();
    let matched = view.current_match_bytes(cx);
    let selected = view.selected_body_row();
    let widest = body.widest_visible_ix();
    let content = content_width(view, theme, window);
    let scroll = view.body_scroll.clone();

    let list = match &body.kind {
        BodyKind::Empty => centered_note("(empty body)", theme).into_any_element(),
        BodyKind::Binary { len } => centered_note(
            &format!("{} of binary data", format_bytes(*len as u64)),
            theme,
        )
        .into_any_element(),
        BodyKind::Json(outline) => json_list(
            outline.clone(),
            body.visible(),
            hit,
            matched.clone(),
            selected,
            widest,
            content,
            scroll,
            theme,
            cx,
        )
        .into_any_element(),
        BodyKind::Text(lines) => {
            text_list(
                lines.clone(),
                hit,
                matched,
                selected,
                widest,
                content,
                body.raw_is_json(),
                scroll,
                theme,
                cx,
            ).into_any_element()
        }
    };

    container.children(notice).child(list)
}

/// The virtualized JSON view.
///
/// `uniform_list` only builds elements for the visible range, so this stays O(visible)
/// no matter how many rows exist. The closure captures two `Arc`s and the theme — never
/// the fold flags, which would be a megabyte-scale clone per frame (see `is_folded_at`).
#[allow(clippy::too_many_arguments)]
fn json_list(
    outline: Arc<JsonOutline>,
    visible: Arc<Vec<u32>>,
    hit: Option<u32>,
    matched: Option<std::ops::Range<u32>>,
    selected: Option<u32>,
    widest: usize,
    content: Pixels,
    scroll: UniformListScrollHandle,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> impl IntoElement {
    let row_theme = theme.clone();
    let mono = theme.mono.clone();
    let view = cx.entity().downgrade();
    let indicator_scroll = scroll.clone();
    let count = visible.len();

    uniform_list("json-body", count, move |range, _window, _cx| {
        range
            .map(|visible_ix| {
                let row_ix = visible[visible_ix] as usize;
                let Some(row) = outline.row(row_ix).copied() else {
                    return div().w_full().h(px(ROW_HEIGHT));
                };
                let folded = row.kind.is_open() && is_folded_at(&visible, visible_ix, row_ix);
                json_row(
                    &outline,
                    row,
                    row_ix,
                    visible_ix,
                    content,
                    folded,
                    hit == Some(row_ix as u32),
                    (hit == Some(row_ix as u32)).then(|| matched.clone()).flatten(),
                    selected == Some(row_ix as u32),
                    &row_theme,
                    view.clone(),
                )
            })
            .collect()
    })
    // Without this the list cannot be scrolled programmatically, so jumping to a match would
    // silently do nothing — the handle is the only way in.
    .track_scroll(scroll)
    // Horizontal scrolling, and the second call is not optional. `Unconstrained` sizes the
    // content to the widest item — but "widest" means *one sampled row*, `with_width_from_item`,
    // which defaults to index 0. Row 0 of a JSON document is `{`, so the default sizes the region
    // to nothing and the switch above appears not to work at all.
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(widest))
    .with_decoration(HScrollIndicator {
        scroll: indicator_scroll,
        color: theme.text_faint,
    })
    // The reference frame for `a_response_row_spans_the_full_width_of_the_list`. A row's own
    // bounds agree with the width bug, so the container is the only honest thing to measure a
    // click against.
    .debug_selector(|| "response-body".to_string())
    .flex_1()
    .px_2()
    .font_family(mono)
    .text_xs()
}

#[allow(clippy::too_many_arguments)]
fn json_row(
    outline: &JsonOutline,
    row: Row,
    row_ix: usize,
    visible_ix: usize,
    content: Pixels,
    folded: bool,
    is_hit: bool,
    matched: Option<std::ops::Range<u32>>,
    is_selected: bool,
    theme: &Theme,
    view: gpui::WeakEntity<RequestView>,
) -> Div {
    let opening = view.clone();
    // The original moves into `selecting`; nothing below needs a third handle, since the fold
    // chevron dispatches an action rather than reaching into the view.
    let selecting = view;

    let mut line = div()
        .debug_selector(move || format!("response-row-{visible_ix}"))
        .flex()
        // **`w_full` is load-bearing, and its absence looks like nothing.** `uniform_list`
        // lays each item out as a taffy *root* and hands it the list's width as definite
        // available space, which reads like a stretch instruction. It isn't: taffy stretches a
        // root to its available width only for `display: block` (`compute_root_layout`'s
        // `style.is_block()` gate), and `.flex()` above takes the other branch and sizes to
        // content. Without this the row is as wide as its own text — so the highlights below
        // end mid-row, and, worse, the click target covers only the text and the rest of the
        // row silently swallows clicks. This is the picker's bug (see `picker.rs`) in the
        // surface that has a million rows.
        //
        // **Horizontal scrolling needed no change here**, which is worth recording because a
        // minimum width looks like the obvious answer and is not. Once the list scrolls
        // sideways it lays each row out with `available_width = viewport + |scroll_x|` and
        // shifts the row origin by `scroll_x`, so a 100% row already spans exactly the visible
        // region at every offset. Swapping in `min_w(100%)` was tried and changed nothing any
        // test or eye could see.
        .w_full()
        // The scroll region's real size. `uniform_list` would otherwise take it from measuring
        // one row *before* the list's text style is in effect (see `BodyView::widest_extent`),
        // which shapes it in the wrong font and can fall short of the longest line.
        .min_w(content)
        // And the row carries its own text style rather than inheriting the list's, so anything
        // that does measure it measures the font it is drawn in.
        .font_family(theme.mono.clone())
        .text_xs()
        .flex_row()
        .items_center()
        .h(px(ROW_HEIGHT))
        .pl(px(4.0 + row.depth as f32 * INDENT))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, window, cx| {
            let _ = selecting.update(cx, |view, cx| {
                view.select_body_row_at(visible_ix, cx);
            });
            // Double-click folds, the file-tree convention — and it means the 12px chevron is
            // no longer the only fold target. Dispatched rather than called, because three
            // surfaces now reach this verb.
            if event.click_count == 2 {
                window.dispatch_action(ToggleFold.boxed_clone(), cx);
            }
        })
        .on_mouse_down(MouseButton::Right, move |event: &MouseDownEvent, window, cx| {
            // Select first: "Copy value" has to be unambiguous about which row it means.
            let _ = opening.update(cx, |view, cx| {
                view.select_body_row_at(visible_ix, cx);
                view.set_menu_anchor(event.position);
            });
            window.dispatch_action(OpenRowMenu.boxed_clone(), cx);
        });

    if is_hit {
        // Whole-row, because the match is a byte range in the source and the row is built from
        // separately styled key/punctuation/value elements — highlighting the exact characters
        // means splitting a shaped run, which is the syntax-highlighting problem (principle 4)
        // and not this slice's. The row is enough to find it with your eye.
        line = line.bg(theme.bg_hover).border_l_2().border_color(theme.accent);
    }

    // After the hit, so a row that is both keeps the accent bar saying "this is a match" while
    // the fill says "you are here". One `border_color` paints every side, so the selection
    // deliberately adds no border of its own — two colours would need two boxes.
    if is_selected {
        line = line.bg(theme.selection);
    }

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
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    // **Deliberately does *not* stop propagation**, which is the opposite of
                    // what a clickable nested in a clickable usually needs. Two reasons, and
                    // the second is the one that bites. The row's own effect is wanted here —
                    // the row handler selects, which is what lets this dispatch a verb that
                    // takes no row index. And `track_focus` transfers focus by registering an
                    // ordinary Bubble-phase mouse listener (`div.rs`, `Interactivity::paint`),
                    // so stopping propagation silently suppresses *that* too: with it, clicking
                    // a chevron left the pane unfocused and the next arrow key did nothing.
                    window.dispatch_action(ToggleFold.boxed_clone(), cx);
                })
                .child(marker.to_string()),
        );
    } else {
        line = line.child(div().flex_none().w(px(12.)));
    }

    // **One `StyledText` rather than a div per token**, which is what lets a search highlight
    // the matched *bytes* instead of tinting the whole row: `with_highlights` layers a colour
    // and a background onto ranges of one shaped string, where separately styled elements can
    // only be coloured whole. It also collapses up to five elements per row into one, on the
    // surface that has 1.31M of them.
    let mut painted = PaintedRow::default();

    if !row.key.is_none() {
        painted.push_from(outline.text(row.key), theme.syntax.key, row.key.start);
        painted.push(": ", theme.syntax.punct);
    }

    match row.kind {
        RowKind::Scalar(kind) => painted.push_from(
            outline.text(row.value),
            match kind {
                ScalarKind::String => theme.syntax.string,
                ScalarKind::Number => theme.syntax.number,
                ScalarKind::Bool | ScalarKind::Null => theme.syntax.literal,
            },
            row.value.start,
        ),
        RowKind::ObjectOpen if folded => {
            painted.push(&format!("{{ … {} }}", plural(row.child_count)), theme.syntax.punct)
        }
        RowKind::ArrayOpen if folded => {
            painted.push(&format!("[ … {} ]", plural(row.child_count)), theme.syntax.punct)
        }
        RowKind::ObjectOpen => painted.push("{", theme.syntax.punct),
        RowKind::ArrayOpen => painted.push("[", theme.syntax.punct),
        RowKind::ObjectClose => painted.push("}", theme.syntax.punct),
        RowKind::ArrayClose => painted.push("]", theme.syntax.punct),
    }

    if row.trailing_comma {
        painted.push(",", theme.syntax.punct);
    }

    line.child(div().flex_none().child(painted.into_element(matched, theme)))
}

/// One line of the raw view, lexed if the body was meant to be JSON.
///
/// `StyledText` rather than a coloured div per token, for the same two reasons as `json_row`: a
/// search can highlight the matched bytes within it, and a minified line stays one element
/// instead of hundreds.
fn raw_line(
    text: &str,
    highlight: bool,
    matched: Option<std::ops::Range<usize>>,
    theme: &Theme,
) -> gpui::StyledText {
    let colours: Vec<_> = if highlight {
        zuno_core::highlight::lex_json(text)
            .into_iter()
            .map(|token| {
                (
                    token.range,
                    crate::ui::syntax_colour(token.kind, &theme.syntax),
                )
            })
            .collect()
    } else {
        Vec::new()
    };

    if colours.is_empty() && matched.is_none() {
        return gpui::StyledText::new(text.to_string());
    }

    let highlights = crate::ui::split_spans(text.len(), &colours, matched)
        .into_iter()
        .map(|(range, colour, hit)| {
            (
                range,
                gpui::HighlightStyle {
                    color: Some(if hit {
                        theme.text_on_accent
                    } else {
                        colour.unwrap_or(theme.text)
                    }),
                    background_color: hit.then_some(theme.accent),
                    ..Default::default()
                },
            )
        });

    gpui::StyledText::new(text.to_string()).with_highlights(highlights)
}

/// A row's text and the colours over it, assembled token by token.
///
/// Exists so the caller can keep writing "push this token in this colour" while what comes out
/// is a single string plus highlight ranges — the shape `StyledText` wants and the shape a
/// per-character search highlight needs.
#[derive(Default)]
struct PaintedRow {
    text: String,
    colours: Vec<(std::ops::Range<usize>, gpui::Hsla)>,
    /// Where each piece came from in the response source, for pieces that came from it at all.
    /// A folded container's `{ … 3 items }` summary has no source, so a match can never be
    /// located inside text that isn't the body's.
    sources: Vec<(std::ops::Range<usize>, u32)>,
}

impl PaintedRow {
    fn push(&mut self, piece: &str, colour: gpui::Hsla) {
        let start = self.text.len();
        self.text.push_str(piece);
        self.colours.push((start..self.text.len(), colour));
    }

    /// As `push`, recording the source offset the piece was read from.
    fn push_from(&mut self, piece: &str, colour: gpui::Hsla, source: u32) {
        let start = self.text.len();
        self.push(piece, colour);
        self.sources.push((start..self.text.len(), source));
    }

    /// Translate a match's source range into this row's rendered text.
    ///
    /// `None` when the match falls in punctuation the row synthesises rather than reads — the
    /// `": "` between a key and its value, or a fold summary. Those bytes are not in the source
    /// at all, so there is nothing to point at.
    fn locate(&self, source: std::ops::Range<u32>) -> Option<std::ops::Range<usize>> {
        self.sources.iter().find_map(|(local, from)| {
            let span = *from..*from + local.len() as u32;
            (span.start <= source.start && span.end >= source.end).then(|| {
                let offset = (source.start - span.start) as usize;
                let start = local.start + offset;
                start..(start + source.len()).min(local.end)
            })
        })
    }

    fn into_element(self, matched: Option<std::ops::Range<u32>>, theme: &Theme) -> gpui::StyledText {
        let matched = matched.and_then(|range| self.locate(range));
        let highlights = crate::ui::split_spans(self.text.len(), &self.colours, matched)
            .into_iter()
            .map(|(range, colour, hit)| {
                (
                    range,
                    gpui::HighlightStyle {
                        // The match wins the foreground too. A background alone against a
                        // syntax colour is a contrast lottery this palette has no reason to run.
                        color: Some(if hit { theme.text_on_accent } else { colour.unwrap_or(theme.text) }),
                        background_color: hit.then_some(theme.accent),
                        ..Default::default()
                    },
                )
            });

        gpui::StyledText::new(self.text).with_highlights(highlights)
    }
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
#[allow(clippy::too_many_arguments)]
fn text_list(
    lines: Arc<LineIndex>,
    hit: Option<u32>,
    matched: Option<std::ops::Range<u32>>,
    selected: Option<u32>,
    widest: usize,
    content: Pixels,
    highlight: bool,
    scroll: UniformListScrollHandle,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> impl IntoElement {
    let row_theme = theme.clone();
    let mono = theme.mono.clone();
    let view = cx.entity().downgrade();
    let indicator_scroll = scroll.clone();
    let count = lines.len();

    uniform_list("text-body", count, move |range, _window, _cx| {
        range
            .map(|ix| {
                let (text, truncated) = lines.line(ix);
                let selecting = view.clone();
                let opening = view.clone();
                let mut row = div()
                    .debug_selector(move || format!("response-row-{ix}"))
                    .flex()
                    // See `json_row` — a `.flex()` row inside a `uniform_list` sizes to its
                    // content, not to the list, so without this the click target and both
                    // highlights stop at the end of the line's text. Still `w_full` after
                    // horizontal scrolling landed — see `json_row` for why a minimum isn't
                    // needed.
                    .w_full()
                    .min_w(content)
                    .font_family(row_theme.mono.clone())
                    .text_xs()
                    .flex_row()
                    .items_center()
                    .h(px(ROW_HEIGHT))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                        let _ = selecting.update(cx, |view, cx| {
                            view.select_body_row_at(ix, cx);
                        });
                    })
                    // Same gesture as the JSON view. No double-click here: raw lines have no
                    // structure to fold, so binding it would be a keystroke that does nothing.
                    .on_mouse_down(
                        MouseButton::Right,
                        move |event: &MouseDownEvent, window, cx| {
                            let _ = opening.update(cx, |view, cx| {
                                view.select_body_row_at(ix, cx);
                                view.set_menu_anchor(event.position);
                            });
                            window.dispatch_action(OpenRowMenu.boxed_clone(), cx);
                        },
                    )
                    .child(div().flex_none().child(raw_line(
                        text,
                        highlight,
                        // The match is a *source* offset; a line's text starts partway into the
                        // body, so it has to be rebased before it means anything here. Clamped
                        // to the drawn text, since a line past `MAX_DISPLAY_LINE` is cut and a
                        // match beyond the cut has no character on screen to mark — the find
                        // bar already says so.
                        (hit == Some(ix as u32))
                            .then(|| matched.clone())
                            .flatten()
                            .and_then(|range| {
                                let start = lines.line_start(ix)?;
                                let from = range.start.checked_sub(start)? as usize;
                                (from < text.len())
                                    .then(|| from..(from + range.len()).min(text.len()))
                            }),
                        &row_theme,
                    )));

                if hit == Some(ix as u32) {
                    row = row
                        .bg(row_theme.bg_hover)
                        .border_l_2()
                        .border_color(row_theme.accent);
                }

                if selected == Some(ix as u32) {
                    row = row.bg(row_theme.selection);
                }

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
    .track_scroll(scroll)
    // See `json_list`: the sampled-row index is what makes this do anything.
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(widest))
    .with_decoration(HScrollIndicator {
        scroll: indicator_scroll,
        color: theme.text_faint,
    })
    .debug_selector(|| "response-body".to_string())
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

fn empty_state(theme: &Theme, window: &Window) -> Div {
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
                .child(match crate::workspace::keybinding_label(&SendRequest, window) {
                    key if key.is_empty() => "no send key is bound".to_string(),
                    key => format!("{key} to send"),
                }),
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
