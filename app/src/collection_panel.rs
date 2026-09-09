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
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
    uniform_list,
};
use zuno_core::Method;
use zuno_core::collection::{Node, NodeKind};

use crate::actions::{
    CollectionCollapseAll, CollectionExpandAll, NewFolder, NewRequest, OpenCollectionMenu, OpenWorkspaceMenu,
};
use gpui::Action as _;
use crate::theme::Theme;
use crate::ui::{Icon, glyph, icon_button};
use crate::workspace::Workspace;

/// Fixed, as `uniform_list` requires: it measures one item and assumes the rest agree.
const ROW_HEIGHT: f32 = 25.0;
/// One level of nesting. Deliberately small — a collection nested four deep should still
/// leave most of a narrow panel for the name.
const INDENT: f32 = 12.0;
/// The chevron's column, reserved on *every* row including requests, so names at one depth
/// line up whether or not their neighbour is a directory.
const CHEVRON: f32 = 14.0;
/// The glyph column, reserved on both kinds so names line up.
const GLYPH_WIDTH: f32 = 16.0;

/// The method label's column, where the pictograph's used to sit — before the name, so a
/// request and a sibling folder still start their names at the same x. Sized for `PATCH` in
/// mono at `text_xs`: 5 chars at a 0.6em advance, plus slack.
const METHOD_WIDTH: f32 = 38.0;

/// The title strip's height. Named because the workspace menu anchors just below it.
pub const HEADER_HEIGHT: f32 = 28.0;

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
    // While the new-folder box is open the list is one row longer than `tree_visible`, and every
    // row at or past the insertion point shifts down by one. Rendering it as a real row rather
    // than splicing a placeholder into `tree_visible` keeps that index — which the selection, the
    // fold walk and `scroll_to_item` all address — meaning exactly one thing.
    let pending = workspace.new_node_row();
    let row_theme = theme.clone();
    let count = visible.len() + usize::from(pending.is_some());
    // A `uniform_list` render closure is handed a bare `&mut App`, not a `Context<Workspace>`,
    // so `cx.listener` is unavailable inside it and the entity has to be captured instead —
    // the same shape the response body's rows use.
    let entity = cx.entity();

    let list = uniform_list("collection-tree", count, move |range, _window, _cx| {
        range
            .map(|visible_ix| {
                if let Some((insert_at, depth, kind, input)) = &pending {
                    if visible_ix == *insert_at {
                        return new_node_cell(*kind, *depth, input.clone(), &row_theme);
                    }
                }
                let shifted = match &pending {
                    Some((insert_at, ..)) if visible_ix > *insert_at => visible_ix - 1,
                    _ => visible_ix,
                };
                let Some(&row_ix) = visible.get(shifted) else {
                    return div().w_full().h(px(ROW_HEIGHT));
                };
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
    // Rows ran flush against the header's rule and the panel's bottom edge, which is most of
    // what read as cramped: the first name touched a border and the tree had no margin of its
    // own inside the panel. On the list rather than on the rows, so `ROW_HEIGHT` still describes
    // exactly one row — `scroll_to_item` and the selection both address rows by index.
    .pt_1()
    .pb_2()
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
        .h(px(HEADER_HEIGHT))
        .px_2()
        .border_b_1()
        .border_color(theme.border)
        // The line that says *which* workspace you are in, so it is also the way to change it —
        // this header's own comment predicted that before switching existed.
        .child(crate::ui::menu_button(
            "workspace-name",
            name,
            "Workspace",
            OpenWorkspaceMenu,
            theme.text_muted,
            theme,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                // No hide button here. It was a `×`, and a control that can only *hide* the
                // panel it lives in takes itself away with it — there was no mouse path back.
                // The toggle sits in the titlebar, where it stays reachable in both states.
                // Before New folder, because it is the commoner verb by far — a collection is
                // mostly requests, and until this landed the only way to make one was a scratch
                // tab that then had to be saved to the root and moved.
                .child(icon_button(
                    "collection-new-request",
                    Icon::FilePlus,
                    "New request",
                    NewRequest,
                    theme,
                ))
                .child(icon_button(
                    "collection-new-folder",
                    Icon::FolderPlus,
                    "New folder",
                    NewFolder,
                    theme,
                ))
                // Two controls rather than one that toggles, matching the response pane's
                // `fold all` / `expand` pair. A single button would have to read the tree's
                // state to decide its meaning, and a half-collapsed tree has no honest answer —
                // you could not expand-all from it without collapsing everything first.
                .child(icon_button(
                    "collection-collapse-all",
                    Icon::ChevronsDownUp,
                    "Collapse all folders",
                    CollectionCollapseAll,
                    theme,
                ))
                .child(icon_button(
                    "collection-expand-all",
                    Icon::ChevronsUpDown,
                    "Expand all folders",
                    CollectionExpandAll,
                    theme,
                )),
        )
}

/// The name box for a folder being created, drawn as a row *in* the tree.
///
/// **Not a strip under the header, which is where it started.** A box that says "billing" beside
/// it describes the destination; a box sitting one indent inside `billing`, as its last child,
/// *is* the destination — which is what every editor does and what a reader already knows how to
/// read. It costs an index translation in the list closure and nothing else.
fn new_node_cell(
    kind: crate::workspace::NewNode,
    depth: u16,
    input: Entity<crate::input::TextInput>,
    theme: &Theme,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(ROW_HEIGHT))
        .pr_2()
        .pl(px(6. + f32::from(depth) * INDENT))
        .gap_1()
        .text_xs()
        // **Both columns, at their real widths.** The first version put the folder glyph in the
        // *chevron* column and shrank the second to compensate, which left the input starting
        // 14px left of where a folder name starts — the box did not line up with the row it was
        // about to become. The chevron slot is reserved and empty: there is nothing to expand
        // yet, and drawing a chevron that toggles nothing is a dead control.
        .child(div().flex_none().w(px(CHEVRON)))
        // **What the row it becomes will carry**, not a fixed glyph: a request row shows its
        // method, so the box shows `GET` — the method a new request starts on. A folder glyph
        // here is what shipped, because generalising *where* the row goes said nothing about
        // what it draws.
        .child(match kind {
            crate::workspace::NewNode::Folder => {
                glyph_cell().child(glyph(Icon::Folder, theme.text_faint, theme.text_faint, 13.))
            }
            crate::workspace::NewNode::Request => {
                method_cell(&zuno_core::Method::default(), theme)
            }
        })
        .child(div().flex_1().min_w(px(0.)).overflow_hidden().child(input))
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
            // Three states, not two. A folder that holds *other* things is not an empty
            // collection, and saying "Nothing saved yet" there described the wrong problem —
            // the same courtesy `Import::skipped` already extends.
            .child(match (workspace.tree_scanned, workspace.tree_skipped) {
                (false, _) => SharedString::from("Reading the collection…"),
                (true, 0) => SharedString::from(
                    "Nothing saved yet. Ctrl+S writes the request you're editing into the \
                     collection.",
                ),
                (true, 1) => SharedString::from("No requests here — 1 other file was skipped."),
                (true, n) => {
                    SharedString::from(format!("No requests here — {n} other files were skipped."))
                }
            }),
    )
}

/// Roughly how many characters of a name fit at `depth`, in the panel's font at `text_xs`.
///
/// Computed rather than measured, the same bet `TAB_LABEL_CHARS` makes: real widths need the
/// shipping font, which the test platform does not have, and a pure function over a string is
/// something a unit test can actually check. Only used to decide whether a name needs a tooltip,
/// so erring low costs a tooltip nobody needed and erring high costs one that was wanted.
///
/// `5.95` is `TAB_LABEL_WIDTH / TAB_LABEL_CHARS` — the same measured advance the tab strip is
/// tuned to, since both draw `text_xs` in the UI font.
pub(crate) fn name_budget(depth: u16, is_directory: bool) -> usize {
    // 6 left pad, the chevron column, two 4px gaps, the kind column, 8 right pad. A folder's
    // kind column is the glyph's own width, so a folder name has more room than a request's —
    // which is the trade for the glyph sitting beside its name instead of a column away.
    let kind = if is_directory { GLYPH_WIDTH } else { METHOD_WIDTH };
    let chrome = 6. + CHEVRON + 4. + kind + 4. + 8. + f32::from(depth) * INDENT;
    (((WIDTH - chrome) / 5.95).max(0.)) as usize
}

/// The method's name. Replaced a pictograph per method, whose worst arm was a trash can for
/// DELETE — the row menu's *Move to trash* glyph, on a request.
///
/// Exhaustive with no catch-all, so a new `Method` is a compile error rather than a blank cell.
pub(crate) fn method_label(method: &Method) -> String {
    match method {
        Method::Get => "GET".to_string(),
        Method::Post => "POST".to_string(),
        Method::Put => "PUT".to_string(),
        Method::Patch => "PATCH".to_string(),
        Method::Delete => "DEL".to_string(),
        Method::Head => "HEAD".to_string(),
        Method::Options => "OPT".to_string(),
        // An empty custom verb is unreachable through the picker, but it would leave the
        // column blank rather than odd-looking.
        Method::Other(verb) if verb.is_empty() => "?".to_string(),
        Method::Other(verb) => verb.chars().take(5).collect::<String>().to_uppercase(),
    }
}

fn method_cell(method: &Method, theme: &Theme) -> Div {
    div()
        .flex_none()
        .w(px(METHOD_WIDTH))
        .font_family(theme.mono.clone())
        .text_color(theme.method_color(method))
        .child(method_label(method))
}

/// The folder glyph's slot: its own narrow width, so the icon sits beside its name. Sharing the
/// method column put 25px of nothing between them.
fn glyph_cell() -> Div {
    div().flex_none().w(px(GLYPH_WIDTH)).flex().items_center()
}

/// One row's name: a single line, clipped, with the full text on hover when it does not fit.
///
/// **`whitespace_nowrap` is the whole fix, and its absence was the bug.** gpui's default is
/// `WhiteSpace::Normal`, so a long name *wrapped* — and the row is a fixed `ROW_HEIGHT`, as
/// `uniform_list` requires, so the second line was sliced in half. It read as a rendering fault
/// rather than as a name too long for the panel, which is why it was misdiagnosed as clipping.
///
/// The tooltip is attached only when the name is over budget. One that repeats a name you can
/// already read in full is noise, and the panel would have one on every row.
fn name_cell(
    name: &str,
    depth: u16,
    is_directory: bool,
    color: gpui::Hsla,
    row_ix: usize,
) -> gpui::Stateful<Div> {
    let full = SharedString::from(name.to_string());
    let overflows = name.chars().count() > name_budget(depth, is_directory);

    let mut cell = div()
        // `tooltip` lives on `StatefulInteractiveElement`, so the cell needs an id — which is
        // also why the tooltip is here rather than on the row: a `.id()` there would make it
        // `Stateful<Div>` and it could no longer share a `Vec` with the list's fallback row.
        .id(("collection-name", row_ix))
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(color)
        .child(full.clone());

    if overflows {
        cell = cell.tooltip(move |_window, cx| crate::ui::Tooltip::text(full.clone(), cx));
    }
    cell
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
                glyph_cell().child(glyph(
                    if expanded {
                        Icon::FolderOpen
                    } else {
                        Icon::Folder
                    },
                    theme.text_muted,
                    theme.text,
                    13.,
                )),
            )
            // **The rename box has to be drawn here too.** It was only in the request arm, so
            // renaming a folder focused a handle whose element was never painted — the box
            // appeared not to open, typing went nowhere, and `Enter` fell through to the panel's
            // own binding. The failure is invisible: `renaming_row()` reported it open.
            .child(match renaming {
                Some(input) => div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(input)
                    .into_any_element(),
                None => name_cell(&node.name, node.depth, true, theme.text, row_ix)
                    .into_any_element(),
            }),
        NodeKind::Request { method, .. } => row
            // The chevron's column is held open on a request row too, so a request and a
            // sibling directory start their names at the same x.
            .child(div().flex_none().w(px(CHEVRON)))
            .child(method_cell(method, theme))
            // The rename box takes the name's place rather than overlaying the row, so the
            // method and the indentation stay put and the name appears to become editable
            // where it already was.
            .child(match renaming {
                Some(input) => div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(input)
                    .into_any_element(),
                None => name_cell(
                    &node.name,
                    node.depth,
                    false,
                    theme.text_muted,
                    row_ix,
                )
                .into_any_element(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_budget_shrinks_with_depth_and_stays_sane() {
        // Decides whether a row gets a hover tooltip, so both failure modes are silent: a budget
        // of zero puts one on every row, and an enormous one puts it on none. A bounded range is
        // the only assertion that catches either — the same shape as the response viewer's
        // `the_widest_row_can_actually_be_reached`.
        let root = name_budget(0, false);
        assert!(
            (20..=32).contains(&root),
            "a root-level name should fit roughly 30 characters, got {root}"
        );

        // Each level of nesting costs `INDENT`, which is about two characters.
        for depth in 1..6u16 {
            let deeper = name_budget(depth, false);
            assert!(
                deeper < name_budget(depth - 1, false),
                "depth {depth} must have less room than depth {}",
                depth - 1
            );
            assert!(deeper > 0, "a name must never be budgeted to nothing");
        }

        // A folder's glyph column is narrower than a request's method column, so its name has
        // more room. Asserted rather than assumed: the two used to be equal, and a reader
        // comparing them is the only way to notice the columns diverged.
        assert!(name_budget(2, true) > name_budget(2, false));
    }

    #[test]
    fn every_method_gets_its_own_label_and_all_of_them_fit_the_column() {
        // A duplicated arm is a copy-paste away and shows on screen only if a reader happens to
        // have both verbs in the tree. HEAD and OPTIONS are the pair that matters: they share
        // `method_other`, so the *label* is the only thing telling them apart — which is the
        // same argument the pictographs were held to, now that the label replaced them.
        let methods = [
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Patch,
            Method::Delete,
            Method::Head,
            Method::Options,
            Method::Other("REPORT".into()),
        ];

        let mut seen: Vec<String> = Vec::new();
        for method in &methods {
            let label = method_label(method);
            assert!(!label.is_empty(), "{method:?} has no label");
            assert!(
                !seen.contains(&label),
                "{method:?} reuses the label of an earlier method"
            );
            // `METHOD_WIDTH` is set by the longest label, so a new arm that overflows it would
            // clip on screen and nowhere else. Mono's advance is 0.6em, and the column draws at
            // `text_xs`.
            let width = label.chars().count() as f32 * 12. * 0.6;
            assert!(
                width <= METHOD_WIDTH,
                "{method:?} label {label:?} needs {width}px of a {METHOD_WIDTH}px column"
            );
            seen.push(label);
        }

        // An empty custom verb is not reachable through the picker, but it must not leave the
        // column blank either.
        assert_eq!(method_label(&Method::Other(String::new())), "?");
    }
}
