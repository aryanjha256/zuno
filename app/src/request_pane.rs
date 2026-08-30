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
    SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::actions::{
    AddFormField, AddHeader, AddMultipartField, AddQuery, BodyFindNext, BodyFindPrev,
    CancelRequest, ChooseBodyFile, CloseBodyFind, CopyAsCurl, ImportCurl, OpenBodyType,
    OpenSettings, ReplaceAll, ReplaceNext, SaveRequest, SendRequest, ShowBodyTab, ShowHeadersTab,
    ShowParamsTab,
};
use crate::ui::{Icon, icon_button};
use crate::request_view::{BodyType, KeyValueRow, MultipartRow, RequestTab, RequestView, RowKind};
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
    let body_focused = view.body_region_focused(window, cx);
    let body_lines = view.body_editor.read(cx).line_count();

    let header_detail = count_label(
        view.headers.iter().filter(|row| row.enabled).count(),
        view.headers.len(),
    );
    let query_detail = count_label(
        view.query.iter().filter(|row| row.enabled).count(),
        view.query.len(),
    );

    let pane = div()
        .flex_1()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(theme.bg)
        .child(toolbar(view, theme, url_focused, cx))
        .child(section_tabs(view, theme, cx));

    match view.request_tab {
        RequestTab::Headers => pane
            .child(section_header("Headers", header_detail, RowKind::Header, theme))
            .child(rows_table(&view.headers, RowKind::Header, theme, window, cx)),
        RequestTab::Query => pane
            .child(section_header("Params", query_detail, RowKind::Query, theme))
            .child(rows_table(&view.query, RowKind::Query, theme, window, cx)),
        RequestTab::Body => pane
            .child(body_header(view, body_lines, theme))
            // Above the editor, matching where the response pane puts its own bar — and above
            // rather than below so it does not move as the body grows.
            .children(
                view.body_search
                    .as_ref()
                    .map(|search| body_find_bar(search, theme, cx)),
            )
            .child(body_region(view, theme, body_focused, window, cx)),
    }
}

/// The tab bar over the request's three sections, with the request-level verbs at its far end.
///
/// **Three tabs, so each needs its own action** — the response pane's two-tab trick of one
/// cycling action plus an inert active tab cannot work here, since clicking Body while on
/// Headers is two steps rather than one. See `response_pane::view_tabs`.
///
/// Counts ride on the labels for the reason the response pane's `Headers 24` does: what a
/// hidden section costs you is knowing there's anything in it. Zero is omitted rather than
/// shown, since `Headers 0` is noise where `Headers 3` is information.
fn section_tabs(view: &RequestView, theme: &Theme, cx: &mut gpui::Context<RequestView>) -> Div {
    let active = view.request_tab;
    let body_label = match view.body_type {
        BodyType::Empty => "Body".to_string(),
        _ => format!("Body {}", view.body_label()),
    };

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
        .child(section_tab(
            "request-tab-headers",
            count_suffix("Headers", view.headers.len()),
            active == RequestTab::Headers,
            ShowHeadersTab,
            theme,
            cx,
        ))
        .child(section_tab(
            "request-tab-params",
            count_suffix("Params", view.query.len()),
            active == RequestTab::Query,
            ShowParamsTab,
            theme,
            cx,
        ))
        .child(section_tab(
            "request-tab-body",
            body_label,
            active == RequestTab::Body,
            ShowBodyTab,
            theme,
            cx,
        ))
        .child(div().flex_1())
        .child(request_actions(theme))
}

fn count_suffix(label: &str, count: usize) -> String {
    if count == 0 {
        label.to_string()
    } else {
        format!("{label} {count}")
    }
}

fn section_tab<A: gpui::Action + Clone + 'static>(
    id: &'static str,
    label: String,
    active: bool,
    action: A,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> impl IntoElement + use<A> {
    let tab = div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex_none()
        .px_2()
        .py_1()
        // The inactive tab keeps the border width in the panel's own colour, or switching
        // would shift every label by 2px.
        .border_b_2()
        .border_color(if active { theme.accent } else { theme.bg_panel })
        .text_xs()
        .text_color(if active { theme.text } else { theme.text_muted })
        .child(label);

    if active {
        // No pointer cursor either — it would advertise a click that changes nothing.
        return tab;
    }

    tab.cursor_pointer()
        .hover(|style| style.text_color(theme.text))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                window.dispatch_action(Box::new(action.clone()), cx);
            }),
        )
}

/// The Body header, with a chip that opens the body-type picker — the same action
/// `Ctrl+Shift+B` dispatches.
///
/// **It used to cycle `RawKind` in place**, which was wrong three ways. It could never reach
/// Form, Binary, or Multipart — the picker exists precisely because cycling couldn't. When the
/// body *was* one of those three, the label read "Form"/"Binary"/"Multipart" while the click
/// mutated `body_kind` underneath it, so clicking appeared to do nothing and silently changed
/// what a later switch back to Raw would produce. And it called into the view directly rather
/// than dispatching, which is the thing the "actions, not direct calls" convention exists to
/// prevent: the chip and the keybinding could drift, and had.
fn body_header(view: &RequestView, lines: usize, theme: &Theme) -> Div {
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
                // Form and multipart bodies are row tables, and until this landed the *only* way
                // to add a row to one was `Ctrl+Shift+F` / `Ctrl+Shift+M` — no button anywhere,
                // because the Body tab draws this header rather than `section_header`. The other
                // body types have nothing to add to.
                .children(match view.body_type {
                    BodyType::Form => Some(add_control(RowKind::Form, theme)),
                    BodyType::Multipart => Some(add_control(RowKind::Multipart, theme)),
                    _ => None,
                })
                .child(
                    div()
                        .id("body-kind")
                        // A no-op outside test builds (gpui cfg-gates the body away); it's what
                        // lets a test click this chip rather than only assert about the action
                        // it dispatches.
                        .debug_selector(|| "body-kind-chip".to_string())
                        .px_1()
                        .rounded_sm()
                        .text_color(theme.accent)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.bg_hover))
                        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
                            window.dispatch_action(Box::new(crate::actions::OpenBodyType), cx);
                        })
                        .child(view.body_label()),
                ),
        )
}

/// The address bar: method, URL and Send as one segmented control, edge to edge.
///
/// **One control, no frames.** These were three separately bordered, rounded boxes on a padded
/// row, which read as three unrelated widgets and made Send look like one button among several.
/// Now they share a single fill and are divided by a 1px rule, so the row reads as one thing you
/// act on. Nothing here is rounded, and the row has no padding of its own — the segment *is* the
/// row.
///
/// **The 2px bottom border is always 2px**, and only its colour changes on focus. Growing it from
/// 1px to 2px would shift every row below by a pixel each time focus arrived; `border` is nearly
/// invisible against the panel anyway, so the resting weight costs nothing.
///
/// The fill is `bg_elevated` rather than `bg_hover` so that `bg_hover` stays available as the
/// method segment's hover — with the field itself painted `bg_hover` there would be nowhere for a
/// hover to go and the method would look dead. Worth knowing: in the light theme `bg_elevated`
/// sits at 1.04:1 against `bg_panel`, so there the bottom border does most of the work of saying
/// the field is a field. A dedicated `bg_field` token is the fix whenever that starts to grate.
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
        .flex_none()
        .h(px(crate::ui::BAR_HEIGHT))
        .bg(theme.bg_elevated)
        .border_b_2()
        .border_color(if url_focused { theme.accent } else { theme.border })
        .child(method_chip(view, theme))
        .child(segment_divider(theme))
        .child(url_bar(view, theme))
        .child(send_button(theme, view.is_sending(), cx))
}

/// The rule between two segments. A filled 1px child rather than a border on either neighbour,
/// because a div carries one `border_color` for all four sides and the row's already spoken for.
fn segment_divider(theme: &Theme) -> impl IntoElement + use<> {
    div().flex_none().w(px(1.)).h_full().bg(theme.border)
}

/// The verbs that act on the request, at the far end of the section tabs.
///
/// They sat beside Send until the tabs landed, where four grey icons touching the one accent
/// button made Send read as button 1 of 5. They belong with the request's sections, not with
/// the thing that sends it.
fn request_actions(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .flex_none()
        .child(icon_button(
            "action-save-request",
            Icon::Save,
            "Save request to collection",
            SaveRequest,
            theme,
        ))
        .child(icon_button(
            "action-import-curl",
            Icon::Clipboard,
            "Import request from curl on the clipboard",
            ImportCurl,
            theme,
        ))
        .child(icon_button(
            "action-copy-curl",
            Icon::Terminal,
            "Copy request as a curl command",
            CopyAsCurl,
            theme,
        ))
        .child(icon_button(
            "action-settings",
            Icon::Settings,
            "Request settings",
            OpenSettings,
            theme,
        ))
}

/// Clicking opens the method picker, same as `Ctrl+M`.
///
/// It cycled until M4, and the note here used to say a real dropdown "needs an anchored
/// popover". It doesn't: the picker is a centred modal, and reusing it means one selection
/// interaction instead of two — plus a filter input, which is how a custom verb becomes
/// reachable at all.
fn method_chip(view: &RequestView, theme: &Theme) -> impl IntoElement {
    div()
        .id("method-chip")
        .flex()
        .items_center()
        .flex_none()
        .h_full()
        .px(px(10.))
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(theme.method_color(&view.method))
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        // Dispatches rather than mutating the view directly, so the button and Ctrl+M run
        // the same path — the convention in CLAUDE.md. Right-click used to cycle backwards;
        // with a filtered list there is no "backwards" to go.
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, window, cx| {
                window.dispatch_action(Box::new(crate::actions::OpenMethod), cx);
            },
        )
        .child(view.method.as_str().to_string())
}

fn url_bar(view: &RequestView, theme: &Theme) -> Div {
    div()
        .flex_1()
        .min_w(px(0.))
        // `truncate()` styles text overflow; it does not clip a custom-painted element.
        // Without this the shaped URL paints straight over the Send button.
        .overflow_hidden()
        .px_2()
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
        .flex()
        .items_center()
        .flex_none()
        .h_full()
        .px_4()
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
/// The `+ Add` control for a row table.
///
/// Dispatches the action its keystroke does rather than calling `add_row` — the convention the
/// body-kind chip and the fold-all buttons were both caught breaking. Four action types, so the
/// arms erase to `AnyElement`.
fn add_control(kind: RowKind, theme: &Theme) -> gpui::AnyElement {
    match kind {
        RowKind::Header => crate::ui::icon_text_action(
            "add-header",
            Icon::Plus,
            "Add".into(),
            "Add header",
            AddHeader,
            theme.accent,
            theme,
        )
        .into_any_element(),
        RowKind::Query => crate::ui::icon_text_action(
            "add-query",
            Icon::Plus,
            "Add".into(),
            "Add query parameter",
            AddQuery,
            theme.accent,
            theme,
        )
        .into_any_element(),
        RowKind::Form => crate::ui::icon_text_action(
            "add-form-field",
            Icon::Plus,
            "Add".into(),
            "Add form field",
            AddFormField,
            theme.accent,
            theme,
        )
        .into_any_element(),
        RowKind::Multipart => crate::ui::icon_text_action(
            "add-part",
            Icon::Plus,
            "Add".into(),
            "Add multipart field",
            AddMultipartField,
            theme.accent,
            theme,
        )
        .into_any_element(),
    }
}

/// No `cx`: the add control dispatches an action rather than calling `add_row` through a
/// listener, so nothing here needs the view.
fn section_header(title: &str, detail: SharedString, kind: RowKind, theme: &Theme) -> Div {

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
                .child(add_control(kind, theme)),
        )
}


fn rows_table(
    rows: &[KeyValueRow],
    kind: RowKind,
    theme: &Theme,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    if rows.is_empty() {
        return empty_table(kind, theme, window);
    }

    let prefix = match kind {
        RowKind::Header => "hdr",
        RowKind::Query => "qry",
        RowKind::Form => "fld",
        // Never reached: multipart goes through `multipart_table`, which labels each row by
        // whether it is a file.
        RowKind::Multipart => "prt",
    };

    div().flex().flex_col().children(
        rows.iter()
            .enumerate()
            .map(|(ix, row)| render_row(row, kind, prefix, ix, theme, cx)),
    )
}

/// One "nothing here yet — press X to add" line, with X read from the keymap.
///
/// **Every hint in this pane used to write its own keystroke as a literal.** All of them happened
/// to be correct, which is exactly why they were dangerous: a rebinding would have left four
/// confident sentences naming keys that do nothing, with no test and no compiler to notice. The
/// in-flight pane already learned this the hard way — it advertised `Ctrl+C` for several
/// milestones — and `keybinding_hint` was written for that fix and then used at that one site.
///
/// The dropping of an unbound clause lives in `workspace::hint_sentence` rather than here, so the
/// four tables and the two prose hints below cannot disagree about it.
fn hint_row(
    what: &str,
    hints: &[(&dyn gpui::Action, &str)],
    theme: &Theme,
    window: &Window,
) -> Div {
    div()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child(crate::workspace::hint_sentence(
            &format!("No {what}"),
            hints,
            window,
        ))
}

fn empty_table(kind: RowKind, theme: &Theme, window: &Window) -> Div {
    match kind {
        RowKind::Header => hint_row("headers", &[(&AddHeader, "to add")], theme, window),
        RowKind::Query => hint_row("query parameters", &[(&AddQuery, "to add")], theme, window),
        RowKind::Form => hint_row("fields", &[(&AddFormField, "to add")], theme, window),
        RowKind::Multipart => hint_row(
            "parts",
            &[
                (&AddMultipartField, "to add"),
                (&ChooseBodyFile, "to attach a file"),
            ],
            theme,
            window,
        ),
    }
}

/// The multipart table. Separate from `rows_table` because the prefix is per *row* — a part
/// is either text or a file, and that distinction is the whole point of the body type.
fn multipart_table(
    parts: &[MultipartRow],
    theme: &Theme,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    if parts.is_empty() {
        return empty_table(RowKind::Multipart, theme, window);
    }

    div().flex().flex_col().children(parts.iter().enumerate().map(|(ix, part)| {
        let prefix = if part.is_file { "fil" } else { "txt" };
        render_row(&part.row, RowKind::Multipart, prefix, ix, theme, cx)
    }))
}

fn render_row(
    row: &KeyValueRow,
    kind: RowKind,
    prefix: &'static str,
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
        .child(
            div()
                .flex_none()
                .w(px(160.))
                .overflow_hidden()
                .child(row.name.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_color(theme.text_muted)
                .child(row.value.clone()),
        )
        .child(
            div()
                .id(SharedString::from(format!("{prefix}-remove-{ix}")))
                .debug_selector(move || format!("{prefix}-remove-{ix}"))
                // The glyph takes its colour from this group; an `svg()` never inherits hover.
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                // Names the keystroke for the same verb, even though the click carries a row
                // index the action resolves from focus.
                .tooltip(move |window, cx| {
                    crate::ui::Tooltip::for_action(
                        "Remove row",
                        &crate::actions::RemoveRow,
                        window,
                        cx,
                    )
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.remove_row_at(kind, ix, cx)
                    }),
                )
                .child(crate::ui::glyph(
                    crate::ui::Icon::Close,
                    theme.text_muted,
                    theme.status_server_error,
                    crate::ui::GLYPH,
                )),
        )
}

/// The editor entity renders itself; this only supplies the frame, the focus ring, and
/// the inherited text style it shapes with.
fn body_region(
    view: &RequestView,
    theme: &Theme,
    focused: bool,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> impl IntoElement + use<> {
    let region = div()
        .flex_1()
        .min_h(px(0.))
        .m_2()
        .p_2()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.focus_border(focused))
        .font_family(theme.mono.clone())
        .text_xs()
        .text_color(theme.text);

    // A form, multipart, or binary body can be *held* but not yet edited. Showing the empty
    // editor here would be a lie in the worst way: it looks like the request has no body,
    // and it's the state from which a save would overwrite the real one.
    match view.body_type {
        // A form body is a table, not text — the same widget as headers and query params,
        // because `FormField` has the same shape as `Header`.
        BodyType::Form => region
            .font_family(theme.mono.clone())
            .child(rows_table(&view.form, RowKind::Form, theme, window, cx)),
        BodyType::Multipart => region
            .font_family(theme.mono.clone())
            .child(multipart_table(&view.multipart, theme, window, cx)),
        BodyType::Binary => region.child(binary_body(view, theme, window)),
        // Not the editor: its text is retained so switching back is lossless, but showing it
        // under a body type of "None" would imply it gets sent.
        BodyType::Empty => region.child(
            div()
                .text_color(theme.text_muted)
                .child(crate::workspace::hint_sentence("No body", &[(&OpenBodyType, "to pick a type")], window)),
        ),
        BodyType::Raw => region.child(view.body_editor.clone()),
    }
}

/// Find and replace over the request body.
///
/// **A second bar rather than the response's made target-aware.** Both can be open at once —
/// hunting for a field in what you are sending and in what came back are different questions —
/// and one bar would have to be told which, then moved between panes to sit beside it. Two bars
/// share `TextSearch` and `step_button`, which is where the duplication would actually have
/// mattered.
fn body_find_bar(
    search: &crate::request_view::TextSearch,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
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

    let field = |input: &gpui::Entity<crate::input::TextInput>| {
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
            .child(input.clone())
    };

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
        .child(field(&search.query))
        .children(search.replace.as_ref().map(field))
        .child(div().flex_none().text_color(status_color).child(status))
        .children(search.truncated.then(|| {
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(SharedString::from(format!(
                    "first {} only",
                    zuno_core::search::MAX_MATCHES
                )))
        }))
        // Every button dispatches the action its keystroke does, so the two cannot drift.
        .child(crate::ui::icon_button(
            "body-find-prev",
            crate::ui::Icon::ChevronLeft,
            "Previous match",
            BodyFindPrev,
            theme,
        ))
        .child(crate::ui::icon_button(
            "body-find-next",
            crate::ui::Icon::ChevronRight,
            "Next match",
            BodyFindNext,
            theme,
        ))
        .child(crate::ui::icon_button(
            "body-replace",
            crate::ui::Icon::Replace,
            "Replace",
            ReplaceNext,
            theme,
        ))
        .child(crate::ui::icon_button(
            "body-replace-all",
            crate::ui::Icon::ReplaceAll,
            "Replace all",
            ReplaceAll,
            theme,
        ))
        .child(crate::ui::icon_button(
            "body-find-close",
            crate::ui::Icon::Close,
            "Close find",
            CloseBodyFind,
            theme,
        ))
}

/// The chosen file, or a prompt to pick one.
///
/// Clicking anywhere here reopens the picker, so the path doubles as the control — there's
/// nothing else in this region to click.
fn binary_body(view: &RequestView, theme: &Theme, window: &Window) -> impl IntoElement + use<> {
    let chosen = view.binary_path.clone();

    let headline = match &chosen {
        Some(path) => path.display().to_string(),
        None => crate::workspace::hint_sentence("No file chosen", &[(&ChooseBodyFile, "to pick one")], window),
    };

    div()
        .id("binary-body")
        .flex()
        .flex_col()
        .gap_1()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(crate::actions::ChooseBodyFile), cx);
        })
        .child(
            div()
                .text_color(if chosen.is_some() {
                    theme.text
                } else {
                    theme.text_muted
                })
                .child(headline),
        )
        // `build.rs` sends no Content-Type for a binary body on purpose, so the request has
        // none at all unless a header supplies one. Servers routinely reject that, and it's
        // invisible otherwise.
        .children(chosen.map(|_| {
            div()
                .text_color(theme.text_muted)
                .child("Read at send · no Content-Type is sent unless you add the header")
        }))
}

