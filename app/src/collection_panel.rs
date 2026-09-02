//! The collection panel — the *browser*, as distinct from `Ctrl+P`'s finder.
//!
//! Until this existed, `collection::scan` had exactly one caller in the whole app: the picker.
//! So the only way to look at a saved request was to fuzzy-search for it, which requires
//! already knowing its name. Nothing could answer "what have I got in here" — the question you
//! open a collection to ask after a week away.
//!
//! The two surfaces stay separate on purpose rather than one growing a browse mode. `Ctrl+P`
//! ranks buffers first so it doubles as a tab switcher (see ROADMAP), and mixing a tree into
//! that would make it worse at both jobs. Every editor ships both a file finder and a file
//! tree for the same reason.
//!
//! **Virtualized like every other list here.** A collection is hundreds of rows, not the
//! response viewer's 1.31 million, so `uniform_list` is not strictly needed — but the fixed
//! row height it demands is what the rest of this codebase already assumes, and reusing the
//! pattern costs nothing while an un-virtualized list is what broke the response pane's
//! layout (architecture.md §6).

use gpui::{
    Context, Div, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, Window, div, px,
    uniform_list,
};
use zuno_core::collection::{Node, NodeKind};

use crate::actions::{OpenCollectionMenu, ToggleCollectionPanel};
use gpui::Action as _;
use crate::theme::Theme;
use crate::ui::{Icon, glyph, icon_button};
use crate::workspace::Workspace;

/// Fixed, as `uniform_list` requires: it measures one item and assumes the rest agree.
const ROW_HEIGHT: f32 = 22.0;
/// One level of nesting. Deliberately small — a collection nested four deep should still
/// leave most of a narrow panel for the name.
const INDENT: f32 = 12.0;
/// The chevron's column, reserved on *every* row including requests, so names at one depth
/// line up whether or not their neighbour is a directory.
const CHEVRON: f32 = 14.0;
/// Wide enough for `DELETE` at `text_xs`, so the name column starts at the same x on every
/// row rather than jittering with the verb.
const METHOD_WIDTH: f32 = 46.0;

/// The panel's width.
///
/// Fixed rather than resizable, and that is a deliberate limitation rather than an oversight:
/// a drag handle means a stored width, a minimum, and a pointer mode, to serve a preference
/// nobody has expressed yet. Revisit when someone asks.
pub const WIDTH: f32 = 232.0;

pub fn render(
    workspace: &Workspace,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let focused = workspace.panel_focus.is_focused(window);
    let visible = workspace.tree_visible.clone();
    let nodes = workspace.tree.clone();
    let collapsed = workspace.collapsed.clone();
    let selection = workspace.panel_selection;
    // Resolved once per frame rather than per row: `rename_input_for` would be a lookup on
    // every one of them to answer "no" for all but a single row.
    let renaming = workspace.renaming_row();
    let row_theme = theme.clone();
    let count = visible.len();
    // A `uniform_list` render closure is handed a bare `&mut App`, not a `Context<Workspace>`,
    // so `cx.listener` is unavailable inside it and the entity has to be captured instead —
    // the same shape the response body's rows use.
    let entity = cx.entity();

    let list = uniform_list("collection-tree", count, move |range, _window, _cx| {
        range
            .map(|visible_ix| {
                let row_ix = visible[visible_ix];
                let Some(node) = nodes.get(row_ix) else {
                    return div().w_full().h(px(ROW_HEIGHT));
                };
                let expanded = matches!(node.kind, NodeKind::Directory)
                    && !collapsed.contains(&node.path);
                row(
                    node,
                    row_ix,
                    expanded,
                    selection == Some(row_ix),
                    renaming.as_ref().filter(|(ix, _)| *ix == row_ix).map(|(_, i)| i.clone()),
                    &row_theme,
                    entity.clone(),
                )
            })
            .collect()
    })
    .track_scroll(workspace.panel_scroll.clone())
    // The reference frame for `a_collection_row_spans_the_full_width_of_the_panel`. A row's
    // own bounds agree with the width bug this asserts against, so only the container can
    // tell a full-width row from a label-width one.
    .debug_selector(|| "collection-tree".to_string())
    .flex_1();

    div()
        .id("collection-panel")
        .debug_selector(|| "collection-panel".to_string())
        .key_context("CollectionPanel")
        .track_focus(&workspace.panel_focus)
        .flex()
        .flex_col()
        .flex_none()
        .w(px(WIDTH))
        .h_full()
        .overflow_hidden()
        .bg(theme.bg_panel)
        .border_r_1()
        .border_color(theme.focus_border(focused))
        .child(header(workspace, theme, cx))
        .child(list)
        .children(empty_notice(workspace, theme))
}

/// The title strip. Names the collection's own directory rather than saying "Collection",
/// because once project switching exists this is the line that says *which* one you are in —
/// and a label that never changes is a label nobody reads.
fn header(
    workspace: &Workspace,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let name = workspace
        .collection_name(cx)
        .unwrap_or_else(|| SharedString::from("No collection"));

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .h(px(28.))
        .px_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .overflow_hidden()
                .child(name),
        )
        .child(icon_button(
            "collection-hide",
            Icon::Close,
            "Hide the collection panel",
            ToggleCollectionPanel,
            theme,
        ))
}

/// Shown instead of an empty list, because a blank panel is indistinguishable from a broken
/// one. Says what to do rather than only what is absent — the same shape as the picker's
/// "press Ctrl+S to save the one you're editing" fallback.
fn empty_notice(workspace: &Workspace, theme: &Theme) -> Option<impl IntoElement + use<>> {
    if !workspace.tree.is_empty() {
        return None;
    }

    Some(
        div()
            .absolute()
            .top(px(40.))
            .left_0()
            .right_0()
            .px_3()
            .text_xs()
            .text_color(theme.text_faint)
            .child(if workspace.tree_scanned {
                "Nothing saved yet. Ctrl+S writes the request you're editing into the collection."
            } else {
                "Reading the collection…"
            }),
    )
}

fn row(
    node: &Node,
    row_ix: usize,
    expanded: bool,
    selected: bool,
    renaming: Option<Entity<crate::input::TextInput>>,
    theme: &Theme,
    workspace: Entity<Workspace>,
) -> Div {
    let indent = f32::from(node.depth) * INDENT;

    // No `.id()`: it would return `Stateful<Div>`, which cannot share a `Vec` with the
    // fallback row below, and nothing here needs identity — `hover` and `on_mouse_down` both
    // live on `InteractiveElement`, which plain `Div` implements.
    let mut row = div()
        .debug_selector(move || format!("collection-row-{row_ix}"))
        .flex()
        .flex_row()
        .items_center()
        // **Not optional.** `uniform_list` hands each item the list's width as definite
        // available space, which reads like a stretch instruction and is not one: taffy only
        // auto-stretches a root node for `display: block`, and a `.flex()` row sizes to its
        // content. Without this the row is as wide as its label inside a 232px panel, the
        // selection highlight stops mid-row, and the rest of the row swallows clicks. It has
        // shipped twice already — the picker (§12) and the response body (§6).
        .w_full()
        .h(px(ROW_HEIGHT))
        .pr_2()
        .pl(px(6. + indent))
        .gap_1()
        .cursor_pointer()
        .group(crate::ui::ICON_GROUP)
        .text_xs()
        // **No `stop_propagation`.** `track_focus` transfers focus through an ordinary
        // Bubble-phase mouse listener on the panel above, so stopping here would leave the
        // panel unfocused after a click and the next arrow key would do nothing — exactly the
        // bug the response body's fold chevron shipped with (architecture.md §6).
        .on_mouse_down(MouseButton::Left, {
            let workspace = workspace.clone();
            move |_: &MouseDownEvent, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.choose_collection_row(row_ix, window, cx);
                });
            }
        })
        // Right-click is a blind reflex, which is what makes it the discoverable path to a verb
        // that has no button of its own. Selecting first is what lets `DeleteRequest` carry no
        // index: "delete" has to be unambiguous about which row it means.
        .on_mouse_down(MouseButton::Right, move |event: &MouseDownEvent, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.select_collection_row(row_ix, cx);
                workspace.set_collection_menu_anchor(event.position);
            });
            window.dispatch_action(OpenCollectionMenu.boxed_clone(), cx);
        });

    if selected {
        row = row.bg(theme.bg_hover);
    } else {
        row = row.hover(|style| style.bg(theme.bg_hover));
    }

    match &node.kind {
        NodeKind::Directory => row
            .child(
                div().flex_none().w(px(CHEVRON)).child(glyph(
                    if expanded {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    },
                    theme.text_muted,
                    theme.text,
                    10.,
                )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_color(theme.text)
                    .child(SharedString::from(node.name.clone())),
            ),
        NodeKind::Request { method, .. } => row
            // The chevron's column is held open on a request row too, so a request and a
            // sibling directory start their names at the same x.
            .child(div().flex_none().w(px(CHEVRON)))
            .child(
                div()
                    .flex_none()
                    .w(px(METHOD_WIDTH))
                    .text_color(theme.method_color(method))
                    .child(SharedString::from(method.as_str().to_string())),
            )
            .child(match renaming {
                // The rename box takes the name's place rather than overlaying the row, so the
                // method and the indentation stay put and the name appears to become editable
                // where it already was.
                Some(input) => div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(input),
                None => div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(node.name.clone())),
            }),
    }
}
