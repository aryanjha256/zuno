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
    AddAssertion, AddCapture, OpenSettings, ReplaceAll, ReplaceNext, SaveRequest, SendRequest,
    ShowAssertTab, ShowBodyTab, ShowCaptureTab, ShowHeadersTab, ShowParamsTab,
};
use crate::ui::{Icon, icon_button};
use crate::kinds::{GraphQlEditor, KindEditor};
use crate::request_view::{
    BodyType, KeyValueRow, MultipartRow, RequestTab, RequestView, RowKind,
};
use crate::theme::Theme;

pub fn render(
    view: &RequestView,
    theme: &Theme,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    // Read focus state before any `&mut cx` use below — the immutable borrow from
    // `read` has to end first.
    let body_focused = view.body_region_focused(window, cx);
    let body_lines = view
        .primary_editor()
        .map_or(0, |editor| editor.read(cx).line_count());

    let header_detail = count_label(
        view.headers.iter().filter(|row| row.enabled).count(),
        view.headers.len(),
    );
    let query_rows: &[KeyValueRow] = view.http().map_or(&[], |http| &http.query);
    let query_detail = count_label(
        query_rows.iter().filter(|row| row.enabled).count(),
        query_rows.len(),
    );

    let pane = div()
        .flex_1()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(theme.bg)
        .child(section_tabs(view, theme, cx));

    match view.request_tab {
        RequestTab::Headers => pane
            .child(section_header("Headers", header_detail, RowKind::Header, theme))
            .child(rows_table(&view.headers, RowKind::Header, theme, window, cx)),
        // **The kind's own tabs.** Which content a slot holds is the kind's business, not this
        // match's — that is what keeps adding gRPC to one module instead of to every site that
        // renders a tab.
        RequestTab::Kind(slot) => match (&view.kind, slot) {
            (KindEditor::Http(_), 0) => pane
                .child(section_header("Params", query_detail, RowKind::Query, theme))
                .child(rows_table(query_rows, RowKind::Query, theme, window, cx)),
            (KindEditor::Http(_), _) => pane
                .child(body_header(view, body_lines, theme))
                // Above the editor, matching where the response pane puts its own bar — and
                // above rather than below so it does not move as the body grows.
                .children(
                    view.body_search
                        .as_ref()
                        .map(|search| body_find_bar(search, theme, cx)),
                )
                .child(body_region(view, theme, body_focused, window, cx)),
            (KindEditor::GraphQl(graphql), 0) => pane
                .child(graphql_query_header(graphql, theme))
                .children(
                    view.body_search
                        .as_ref()
                        .map(|search| body_find_bar(search, theme, cx)),
                )
                .child(
                    editor_region(theme, focused_editor(&graphql.query, window, cx))
                        .child(graphql.query.clone()),
                ),
            (KindEditor::GraphQl(graphql), _) => pane
                // **Not `section_header`.** That one is for row tables and draws an add
                // control unconditionally — pointed here at `RowKind::Query`, it rendered an
                // "add" button on a text editor whose only effect was to jump you to the
                // document tab, because `AddQuery` shows slot 0 and then adds a row to a table
                // a GraphQL request does not have.
                .child(editor_header("Variables", "", theme))
                .child(
                    editor_region(theme, focused_editor(&graphql.variables, window, cx))
                        .child(graphql.variables.clone()),
                ),
        },
        RequestTab::Capture => pane
            .child(section_header(
                "Capture",
                count_label(
                    view.captures.iter().filter(|row| row.enabled).count(),
                    view.captures.len(),
                ),
                RowKind::Capture,
                theme,
            ))
            .child(capture_table(view, theme, window, cx)),
        RequestTab::Assert => pane
            .child(assert_header(view, theme))
            .child(assert_table(view, theme, window, cx)),
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
/// The request pane's tab strip — **composed from the kind, not hardcoded.**
///
/// Spine tabs bracket the kind's own: Headers, then whatever this kind contributes, then
/// Capture and Assert. For HTTP that reproduces `Headers · Params · Body · Capture · Assert`
/// exactly, so nothing moved for existing requests; for GraphQL it reads
/// `Headers · Query · Variables · Capture · Assert`.
///
/// It used to be five hand-written `section_tab` calls with `"Params"` and `"Body"` baked in, so
/// a GraphQL request drew two tabs whose labels described a kind it wasn't — the document behind
/// one called "Params", the variables behind one called "Body". `a_kinds_tab_strip_is_named_by_
/// the_kind` is what keeps that from coming back.
fn section_tabs(view: &RequestView, theme: &Theme, cx: &mut gpui::Context<RequestView>) -> Div {
    let active = view.request_tab;

    let mut strip = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .flex_none()
        .px_2()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border);

    for tab in RequestTab::for_kind(&view.kind) {
        let (id, label, action): (&'static str, SharedString, Box<dyn gpui::Action>) = match tab {
            RequestTab::Headers => (
                "request-tab-headers",
                SharedString::from(count_suffix("Headers", view.headers.len())),
                Box::new(ShowHeadersTab),
            ),
            // **Two slot actions, because no kind has a third tab yet.** A kind that declares
            // one would have no way to reach it, which `every_kinds_tabs_have_an_action` fails
            // on rather than leaving as a dead control — that is the point to add a
            // parameterised `ShowKindTab(u8)`, not before.
            RequestTab::Kind(slot) => (
                if slot == 0 { "request-tab-kind-0" } else { "request-tab-kind-1" },
                view.kind.tab_label(slot),
                if slot == 0 { Box::new(ShowParamsTab) } else { Box::new(ShowBodyTab) },
            ),
            RequestTab::Capture => (
                "request-tab-capture",
                SharedString::from(count_suffix("Capture", view.captures.len())),
                Box::new(ShowCaptureTab),
            ),
            RequestTab::Assert => (
                "request-tab-assert",
                SharedString::from(count_suffix("Assert", view.assertions.len())),
                Box::new(ShowAssertTab),
            ),
        };

        strip = strip.child(section_tab_boxed(id, label, active == tab, action, theme, cx));
    }

    strip.child(div().flex_1()).child(request_actions(theme))
}

fn count_suffix(label: &str, count: usize) -> String {
    if count == 0 {
        label.to_string()
    } else {
        format!("{label} {count}")
    }
}

/// One tab in the strip.
///
/// Takes a **boxed** action rather than a generic one, because the strip is now built by
/// iterating a heterogeneous list — `Headers`, the kind's own, `Capture`, `Assert` — and those
/// carry different action types. `Action::boxed_clone` is the trait's own answer to
/// `Box<dyn Action>` not being `Clone`.
fn section_tab_boxed(
    id: &'static str,
    label: SharedString,
    active: bool,
    action: Box<dyn gpui::Action>,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> impl IntoElement + use<> {
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
                window.dispatch_action(action.boxed_clone(), cx);
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
                .children(match view.http().map(|http| http.body_type) {
                    Some(BodyType::Form) => Some(add_control(RowKind::Form, theme)),
                    Some(BodyType::Multipart) => Some(add_control(RowKind::Multipart, theme)),
                    _ => None,
                })
                // Offered only where it applies, rather than shown greyed out: the verb is
                // JSON-only, and a control that is present-but-dead teaches nothing.
                .children(
                    (view.http().map(|http| http.body_type) == Some(BodyType::Raw)
                        && view.body_kind() == zuno_core::RawKind::Json)
                        .then(|| {
                            crate::ui::text_action(
                                "action-format-body",
                                "Format".into(),
                                "Format the body as JSON",
                                crate::actions::FormatBody,
                                theme,
                            )
                        }),
                )
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
/// The method, the URL and Send — **spanning the whole window, not just the request pane.**
///
/// Emitted by `RequestView::render` above the request/response split rather than inside the
/// request pane, because the endpoint is the one thing that describes the *whole* exchange:
/// confined to the left half it was cramped by the pane divider while a long URL had nowhere to
/// go, and the response beside it is an answer from that same URL.
pub(crate) fn toolbar(
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
        .child(kind_chip(view, theme))
        .child(div().w(px(1.)).h(px(16.)).flex_none().bg(theme.border))
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
/// Which kind of request this is — HTTP, GraphQL, and whatever comes after.
///
/// **Left of the method, in the toolbar**, because the kind is the request's identity: it
/// decides which tabs exist, whether there is a verb at all, and what goes on the wire. It is
/// deliberately *not* a row in the body-type picker — that picker answers "what body am I
/// sending", and gRPC and MQTT can never be bodies.
fn kind_chip(view: &RequestView, theme: &Theme) -> impl IntoElement {
    div()
        .id("kind-chip")
        .flex()
        .items_center()
        .flex_none()
        .h_full()
        .px(px(10.))
        .text_xs()
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        // Dispatches rather than mutating the view, so the chip and the palette run one path.
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(crate::actions::OpenRequestKind), cx);
        })
        .child(view.kind.choice().label())
}

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
        .text_color(view.method().map_or(theme.text_muted, |m| theme.method_color(m)))
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
        .child(view.method().map_or_else(String::new, |m| m.as_str().to_string()))
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
        RowKind::Assert => crate::ui::icon_text_action(
            "add-assertion",
            Icon::Plus,
            "Add".into(),
            "Add an assertion",
            AddAssertion,
            theme.accent,
            theme,
        )
        .into_any_element(),
        RowKind::Capture => crate::ui::icon_text_action(
            "add-capture",
            Icon::Plus,
            "Add".into(),
            "Capture a value from the response",
            AddCapture,
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
/// A header for a text surface — a title and a note, and **no add control**.
///
/// `section_header` is for row tables and always draws one; using it over an editor puts a
/// button there that adds a row to a table that isn't on screen.
fn editor_header(title: &str, note: &str, theme: &Theme) -> Div {
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
        .child(div().text_color(theme.text_faint).child(note.to_string()))
}

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
        // Never reached: multipart goes through `multipart_table` and captures through
        // `capture_table`, each of which labels its own rows.
        RowKind::Multipart => "prt",
        RowKind::Capture => "cap",
        RowKind::Assert => "asr",
    };

    div().flex().flex_col().children(
        rows.iter()
            .enumerate()
            .map(|(ix, row)| render_row(row, kind, prefix, ix, None, theme, cx)),
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
        RowKind::Assert => hint_row(
            "assertions",
            &[(&AddAssertion, "to add one")],
            theme,
            window,
        ),
        RowKind::Capture => hint_row(
            "captures",
            &[(&AddCapture, "to add one")],
            theme,
            window,
        ),
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

/// The Assert tab's header: the expected status, then the usual add control.
///
/// **The status sits here rather than in the table**, because it is the one check every request
/// wants and a row for it would be on every request in the collection saying the obvious. It is
/// a text box rather than a stepper for the same reason the URL is: you type `404` in three
/// keystrokes, and a box that rejects `4` on the way to `404` is a box nobody can type in.
fn assert_header(view: &RequestView, theme: &Theme) -> Div {
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
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child("Expect status")
                .child(
                    div()
                        .w(px(64.))
                        .overflow_hidden()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(theme.border)
                        .font_family(theme.mono.clone())
                        .text_color(theme.text)
                        .child(view.expect_status.clone()),
                ),
        )
        .child(add_control(RowKind::Assert, theme))
}

fn assert_table(
    view: &RequestView,
    theme: &Theme,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    if view.assertions.is_empty() {
        return empty_table(RowKind::Assert, theme, window);
    }

    div().flex().flex_col().children(
        view.assertions
            .iter()
            .enumerate()
            .map(|(ix, row)| assert_row(row, ix, theme, cx)),
    )
}

fn assert_row(
    row: &crate::request_view::AssertionRow,
    ix: usize,
    theme: &Theme,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    let marker_color = if row.enabled { theme.accent } else { theme.border };
    let text_color = if row.enabled { theme.text } else { theme.text_muted };
    // `exists` takes no value, so showing an empty box beside it invites typing into something
    // that is ignored.
    let takes_value = row.op != zuno_core::assertion::Op::Exists;

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
        .child(
            div()
                .id(SharedString::from(format!("asr-toggle-{ix}")))
                .flex_none()
                .w(px(10.))
                .h(px(10.))
                .rounded_full()
                .bg(marker_color)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.toggle_row_at(RowKind::Assert, ix, cx)
                    }),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .child(row.path.clone()),
        )
        // Three operators, so a click cycles rather than opening a picker for a choice of three.
        .child(
            div()
                .id(SharedString::from(format!("asr-op-{ix}")))
                .debug_selector(move || format!("asr-op-{ix}"))
                .flex_none()
                .w(px(64.))
                .px_1()
                .rounded_sm()
                .bg(theme.bg_panel)
                .text_color(theme.text_muted)
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover).text_color(theme.text))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        view.cycle_assert_op(ix, cx)
                    }),
                )
                .child(row.op.label()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_color(theme.text_muted)
                .children(takes_value.then(|| row.value.clone())),
        )
        .child(
            div()
                .id(SharedString::from(format!("asr-remove-{ix}")))
                .debug_selector(move || format!("asr-remove-{ix}"))
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.remove_row_at(RowKind::Assert, ix, cx)
                    }),
                )
                .child(crate::ui::glyph(Icon::Close, theme.text_muted, theme.text, 12.)),
        )
}

/// The capture table. Separate from `rows_table` for a stronger reason than multipart's: the
/// third column is a *lock*, not a value, and the two text columns mean path-then-name rather
/// than the name-then-value every other table has.
fn capture_table(
    view: &RequestView,
    theme: &Theme,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> Div {
    if view.captures.is_empty() {
        return empty_table(RowKind::Capture, theme, window);
    }

    div()
        .flex()
        .flex_col()
        .children(
            view.captures
                .iter()
                .enumerate()
                .map(|(ix, row)| capture_row(row, ix, theme, cx)),
        )
}

fn capture_row(
    row: &crate::request_view::CaptureRow,
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
    let (icon, hint) = if row.secret {
        (Icon::Lock, "Secret — written to the gitignored file")
    } else {
        (Icon::LockOpen, "Written to the committed file")
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
        .child(
            div()
                .id(SharedString::from(format!("cap-toggle-{ix}")))
                .flex_none()
                .w(px(10.))
                .h(px(10.))
                .rounded_full()
                .bg(marker_color)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.toggle_row_at(RowKind::Capture, ix, cx)
                    }),
                ),
        )
        // Path first, because that is the half you copy out of the response and the half that
        // can be wrong. The variable name is short and usually follows from it.
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .child(row.path.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(24.))
                .text_color(theme.text_faint)
                .child("→"),
        )
        .child(
            div()
                .flex_none()
                .w(px(140.))
                .overflow_hidden()
                .text_color(theme.text_muted)
                .child(row.name.clone()),
        )
        .child(
            div()
                .id(SharedString::from(format!("cap-secret-{ix}")))
                .debug_selector(move || format!("cap-secret-{ix}"))
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                .tooltip({
                    let hint = hint.to_string();
                    move |_, cx| crate::ui::Tooltip::text(hint.clone(), cx)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.toggle_capture_secret(ix, cx)
                    }),
                )
                .child(crate::ui::glyph(
                    icon,
                    if row.secret { theme.accent } else { theme.text_muted },
                    theme.text,
                    12.,
                )),
        )
        .child(
            div()
                .id(SharedString::from(format!("cap-remove-{ix}")))
                .debug_selector(move || format!("cap-remove-{ix}"))
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                        view.remove_row_at(RowKind::Capture, ix, cx)
                    }),
                )
                .child(crate::ui::glyph(Icon::Close, theme.text_muted, theme.text, 12.)),
        )
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
        render_row(&part.row, RowKind::Multipart, prefix, ix, Some(part.is_file), theme, cx)
    }))
}

/// `file_part` is `Some(is_file)` only for a multipart row — the other three tables hold text,
/// and a type chip or a file picker there would be a control that cannot do anything.
#[allow(clippy::too_many_arguments)]
fn render_row(
    row: &KeyValueRow,
    kind: RowKind,
    prefix: &'static str,
    ix: usize,
    file_part: Option<bool>,
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
                .id(SharedString::from(format!("{prefix}-name-{ix}")))
                .debug_selector(move || format!("{prefix}-name-{ix}"))
                .flex_none()
                .w(px(160.))
                .overflow_hidden()
                .child(row.name.clone()),
        )
        // **Says what the row sends, and is how you change it.** The state was previously
        // visible nowhere — the `txt`/`fil` prefix reached element ids and nothing on screen —
        // and a part could only *become* a file by having one chosen, with no way back. A
        // form-data body routinely mixes text fields and uploads, so it has to be a per-row
        // choice that can be made before there is any file to point at.
        //
        // A menu rather than a click-to-toggle: with two states a toggle is ambiguous about
        // which one you are in versus which one you would get, and this label has to answer the
        // first question. The menu shows both with the current one ticked.
        //
        // Shaped like `ui::menu_button` — word, trailing chevron, `GLYPH_INLINE` so the glyph
        // matches the word's height — but not *using* it, for two reasons it cannot serve:
        // its id is a `&'static str` where this needs one per row, and the row index has to be
        // parked before the action is dispatched, since an `Action` carries no payload.
        // `chrome.rs`'s app-name button is hand-rolled off the same primitive for its own
        // reason, so this is the second of two rather than a new pattern.
        .children(file_part.map(|is_file| {
            let (label, colour) = if is_file {
                ("file", theme.accent)
            } else {
                ("text", theme.text_muted)
            };
            div()
                .id(SharedString::from(format!("part-kind-{ix}")))
                .debug_selector(move || format!("part-kind-{ix}"))
                .group(crate::ui::ICON_GROUP)
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .text_color(colour)
                .hover(|style| style.bg(theme.bg_hover))
                .tooltip(move |_, cx| {
                    crate::ui::Tooltip::text(
                        if is_file {
                            "This part sends a file — click to change"
                        } else {
                            "This part sends text — click to change"
                        },
                        cx,
                    )
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                        // Same rule as the file picker beside it: `Workspace`'s root carries
                        // `track_focus`, so letting this bubble hands focus to the root.
                        cx.stop_propagation();
                        view.set_part_kind_menu(ix, event.position);
                        window.dispatch_action(
                            Box::new(crate::actions::OpenPartKindMenu),
                            cx,
                        );
                    }),
                )
                .child(label)
                .child(crate::ui::glyph(
                    crate::ui::Icon::ChevronDown,
                    colour,
                    theme.text,
                    crate::ui::GLYPH_INLINE,
                ))
        }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_color(theme.text_muted)
                .child(row.value.clone()),
        )
        // Only on a part that sends a file: on a text part it would be a control with nothing
        // to do. Placed before Remove so the destructive button stays last, as in every row.
        .children(file_part.unwrap_or(false).then(|| {
            div()
                // `part-`, not `{prefix}-`, and deliberately: the row prefix flips between
                // `txt` and `fil` the moment a part is given a file, so a selector built from it
                // would rename the control the first time it is used. Every other cell in the
                // row is prefixed because headers, query and form share `render_row`; this one
                // exists only for multipart, so it has nothing to disambiguate against.
                .id(SharedString::from(format!("part-file-{ix}")))
                .debug_selector(move || format!("part-file-{ix}"))
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                .tooltip(move |window, cx| {
                    crate::ui::Tooltip::for_action(
                        "Choose a file",
                        &crate::actions::ChooseBodyFile,
                        window,
                        cx,
                    )
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, window, cx| {
                        // Focus first: the action resolves its target row from focus, and this
                        // button is a sibling of the cell rather than inside it, so clicking it
                        // moves nothing on its own.
                        // **`stop_propagation` is load-bearing, not tidiness.** `Workspace`'s
                        // root carries `track_focus`, whose focus-on-click is an ordinary
                        // bubble listener — so without this the click sets the row's focus
                        // here and the root takes it straight back, `focused_multipart_row`
                        // reads `None`, and `ChooseBodyFile` falls through to its other
                        // meaning and replaces the whole body with a binary one. Measured:
                        // the click left focus at `None` until this line existed.
                        cx.stop_propagation();
                        view.focus_multipart_value(ix, window, cx);
                        window.dispatch_action(Box::new(crate::actions::ChooseBodyFile), cx);
                    }),
                )
                .child(crate::ui::glyph(
                    crate::ui::Icon::File,
                    theme.text_muted,
                    theme.accent,
                    crate::ui::GLYPH,
                ))
        }))
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
/// Whether this particular editor holds focus.
///
/// Per editor, not per pane: a GraphQL request has two on screen at different times and the
/// ring has to say which one you are in.
fn focused_editor(
    editor: &gpui::Entity<crate::input::Editor>,
    window: &Window,
    cx: &gpui::App,
) -> bool {
    use gpui::Focusable;
    editor.read(cx).focus_handle(cx).is_focused(window)
}

/// The box a multi-line editor sits in — margin, padding, background, and a focus ring.
///
/// **One definition, used by every editor surface.** The HTTP body had all of this and the
/// GraphQL editors were hand-rolled without it: no padding, no background, and no focus border,
/// so nothing on screen said which of the two you were typing into.
fn editor_region(theme: &Theme, focused: bool) -> Div {
    div()
        .flex_1()
        .min_h(px(0.))
        .m_2()
        .p_2()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.focus_border(focused))
        .font_family(theme.mono.clone())
        .text_xs()
        .text_color(theme.text)
}

fn body_region(
    view: &RequestView,
    theme: &Theme,
    focused: bool,
    window: &Window,
    cx: &mut gpui::Context<RequestView>,
) -> impl IntoElement + use<> {
    let region = editor_region(theme, focused);

    // A form, multipart, or binary body can be *held* but not yet edited. Showing the empty
    // editor here would be a lie in the worst way: it looks like the request has no body,
    // and it's the state from which a save would overwrite the real one.
    let Some(http) = view.http() else {
        // Only HTTP has a body region; every other kind renders its own tabs.
        return region;
    };

    match http.body_type {
        // A form body is a table, not text — the same widget as headers and query params,
        // because `FormField` has the same shape as `Header`.
        BodyType::Form => region
            .font_family(theme.mono.clone())
            .child(rows_table(&http.form, RowKind::Form, theme, window, cx)),
        BodyType::Multipart => region
            .font_family(theme.mono.clone())
            .child(multipart_table(&http.multipart, theme, window, cx)),
        BodyType::Binary => region.child(binary_body(view, theme, window)),
        // Not the editor: its text is retained so switching back is lossless, but showing it
        // under a body type of "None" would imply it gets sent.
        BodyType::Empty => region.child(
            div()
                .text_color(theme.text_muted)
                .child(crate::workspace::hint_sentence("No body", &[(&OpenBodyType, "to pick a type")], window)),
        ),
        BodyType::Raw => region.child(http.body_editor.clone()),
    }
}

/// The header over the GraphQL query editor: what it is, and which operation to run.
///
/// **`operationName` lives here rather than beside the URL**, because it names one of the
/// operations *in this document* — it is a property of the text below it, not of the endpoint.
/// Blank is the common case: a document with one operation needs no name, and the server works
/// it out.
fn graphql_query_header(
    graphql: &GraphQlEditor,
    theme: &Theme,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child(div().flex_none().child("Query"))
        .child(div().flex_1().min_w(px(0.)))
        .child(
            div()
                .flex_none()
                .text_color(theme.text_faint)
                .child("operation"),
        )
        // Styled like `expect_status`, the other small labelled input in a header row, rather
        // than hand-rolled: **`overflow_hidden` is what keeps a long name inside its box** —
        // without it the text simply paints past the edge — and the border is the only thing
        // that says this is somewhere you can type. A placeholder alone vanishes the moment
        // there is a character in it.
        .child(
            div()
                .flex_none()
                .w(px(140.))
                .overflow_hidden()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(theme.border)
                .font_family(theme.mono.clone())
                .text_color(theme.text)
                .child(graphql.operation.clone()),
        )
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
    let chosen = view.http().and_then(|http| http.binary_path.clone());

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

