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

use crate::actions::{
    AddFormField, AddHeader, AddMultipartField, AddQuery, CancelRequest, ChooseBodyFile,
    CopyAsCurl, ImportCurl, OpenBodyType, OpenSettings, SaveRequest, SendRequest,
};
use crate::ui::{Icon, icon_button};
use crate::request_view::{BodyType, KeyValueRow, MultipartRow, RequestView, RowKind};
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
        .child(rows_table(&view.headers, RowKind::Header, theme, window, cx))
        .child(section_header(
            "Query",
            query_detail,
            RowKind::Query,
            theme,
            cx,
        ))
        .child(rows_table(&view.query, RowKind::Query, theme, window, cx))
        .child(body_header(view, body_lines, theme))
        .child(body_region(view, theme, body_focused, window, cx))
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
        .child(method_chip(view, theme))
        .child(url_bar(view, theme, url_focused))
        .child(send_button(theme, view.is_sending(), cx))
        .child(request_actions(theme))
}

/// The verbs that act on the request, as icon buttons beside Send.
///
/// Save and Settings act on this request; Import and Copy-as-curl move it in and out of the app.
/// All four were keyboard-only, which for Copy-as-curl meant a feature shipped the same week with
/// no way to find it.
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

fn url_bar(view: &RequestView, theme: &Theme, focused: bool) -> Div {
    div()
        .flex_1()
        .min_w(px(0.))
        // `truncate()` styles text overflow; it does not clip a custom-painted element.
        // Without this the shaped URL paints straight over the Send button.
        .overflow_hidden()
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
        RowKind::Form => "add-form-field",
        RowKind::Multipart => "add-part",
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
        .rounded_md()
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

