//! `{{variables}}` on screen: the hover that says what one resolves to, and the list that
//! completes a name after `{{`. Shared by `TextInput` and `Editor`, so a variable reads the same
//! in the URL bar as in a body.
//!
//! Both draw through **`deferred`**, which is what lets them escape: every input sits inside
//! `overflow_hidden` ancestors, and an `anchored` child is still masked by them, while a deferred
//! draw is painted after the whole tree with no clip of its own.

use std::ops::Range;

use gpui::{
    App, Context, InteractiveElement, IntoElement, ParentElement, Pixels, Point, SharedString,
    Styled, div, px,
};

use crate::theme::ActiveTheme;

/// What the hovered `{{name}}` resolves to, and from where, with its top-left at `at`.
pub fn popover(name: &str, at: Point<Pixels>, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let info = crate::auth::describe(name, cx);

    // A value runs to a JWT's length; the popover is for recognising it, not reading it all.
    const SHOWN: usize = 120;
    let value = info.value.map(|value| shorten(&value, SHOWN));
    let defined = value.is_some();

    gpui::deferred(
        gpui::anchored()
            .position(at)
            .position_mode(gpui::AnchoredPositionMode::Window)
            .child(
                div()
                    .debug_selector(|| "variable-popover".to_string())
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .max_w(px(420.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .text_xs()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div()
                                    .font_family(theme.mono.clone())
                                    .text_color(theme.text)
                                    .child(name.to_string()),
                            )
                            .child(div().text_color(theme.text_faint).child(info.origin)),
                    )
                    .child(
                        div()
                            .font_family(theme.mono.clone())
                            .text_color(if defined {
                                theme.text_muted
                            } else {
                                theme.status_server_error
                            })
                            // Not "will not send": true of the URL and headers, which are
                            // checked, and false of a form body, which is sent as typed.
                            .child(value.unwrap_or_else(|| "unresolved".to_string())),
                    ),
            ),
    )
}

fn shorten(value: &str, max: usize) -> String {
    if value.is_empty() {
        "(empty)".to_string()
    } else if value.chars().count() > max {
        format!("{}…", value.chars().take(max).collect::<String>())
    } else {
        value.to_string()
    }
}

/// A `{{name` being completed: what a choice replaces, and the names that fit.
#[derive(Clone)]
pub struct Completion {
    pub replace: Range<usize>,
    pub closed: bool,
    pub items: Vec<String>,
}

/// The completion at `cursor`, unless it was dismissed at this same `{{`.
///
/// `dismissed` is the `replace.start` that `Escape` was pressed at — keyed by where the name
/// begins rather than a flag, so typing a *new* `{{` elsewhere opens the list again.
pub fn completion(text: &str, cursor: usize, dismissed: Option<usize>, cx: &App) -> Option<Completion> {
    let prefix = zuno_core::environment::completion_at(text, cursor)?;
    if dismissed == Some(prefix.replace.start) {
        return None;
    }
    let shown = cx.try_global::<crate::auth::ShownResolver>()?;
    let names = shown.resolver.names();
    let items: Vec<String> = zuno_core::environment::rank_names(&names, prefix.typed)
        .into_iter()
        .map(str::to_string)
        .collect();
    // Nothing to offer, or the one match is already typed and closed — a list there is a thing
    // to dismiss rather than to use.
    let finished = prefix.closed && items.len() == 1 && items[0] == prefix.typed;
    (!items.is_empty() && !finished).then(|| Completion {
        replace: prefix.replace,
        closed: prefix.closed,
        items,
    })
}

/// The text a chosen name is written as: the name, and the closing braces unless they are
/// already there.
pub fn inserted(name: &str, closed: bool) -> String {
    if closed {
        name.to_string()
    } else {
        format!("{name}}}}}")
    }
}

/// The list itself, at `at`, each name beside a glimpse of its value.
pub fn completion_list<V, C>(
    at: Point<Pixels>,
    items: &[String],
    highlighted: usize,
    cx: &mut Context<V>,
    on_choose: C,
) -> impl IntoElement + use<V, C>
where
    V: gpui::Render,
    C: Fn(&mut V, usize, &mut gpui::Window, &mut Context<V>) + Clone + 'static,
{
    let theme = cx.theme().clone();
    let rows = items
        .iter()
        .map(|name| crate::ui::SelectRow {
            label: SharedString::from(name.clone()),
            detail: crate::auth::describe(name, cx)
                .value
                .map(|value| SharedString::from(shorten(&value, 40))),
            dimmed: false,
        })
        .collect();
    gpui::deferred(crate::ui::select_list(
        "variable-completion",
        at,
        rows,
        Some(highlighted),
        px(180.),
        &theme,
        cx,
        |_, _, _| {},
        on_choose,
    ))
}
