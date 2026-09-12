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
    AppContext as _, ClickEvent, Context, Div, DragMoveEvent, Empty, Entity, InteractiveElement,
    IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Window, div, px, uniform_list,
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

/// The panel's width when nothing has resized it, and where a double-click on the handle
/// returns it to. Also what a pre-v5 session adopts — `session.rs` imports this one rather
/// than restating the number.
pub const DEFAULT_WIDTH: f32 = 232.0;

/// The narrowest useful panel.
///
/// Not arbitrary: `CHEVRON + GLYPH_WIDTH + METHOD_WIDTH` is 68px of fixed columns before a
/// name starts, plus the list's own padding, so much below this and every row is chrome with
/// an ellipsis after it. A resize that can reach a useless width is a resize you can get
/// stuck in.
const MIN_WIDTH: f32 = 180.0;

/// The widest the panel may get, whatever the window.
///
/// Two ceilings, and the *lower* wins. The absolute one is because past this the panel stops
/// being a sidebar; the proportional one is because a fixed maximum on a narrow window still
/// leaves the request pane unusable.
const MAX_WIDTH: f32 = 600.0;
const MAX_FRACTION: f32 = 0.5;

/// The grab strip, wider than the seam it sits on so it can actually be hit with a pointer.
///
/// Straddles the border rather than sitting beside it, which is why it costs no layout: it is
/// absolutely positioned over the boundary, ~2px into the panel and ~2px into the pane. A
/// gutter that consumed its own width would take those pixels from one side or the other.
const HANDLE_WIDTH: f32 = 5.0;

/// Clamp a desired panel width into what the window can actually accommodate.
///
/// **Applied at render, not only while dragging**, and that is the whole reason it is a
/// function rather than two `min` calls inside the drag handler. A width is stored unclamped
/// and the ceiling moves with the window, so a 600px panel restored onto a 500px screen — or
/// a window dragged narrow after the fact — has to be reined in on the frame that draws it.
/// Clamping only on input leaves the panel eating the request pane with no way to notice.
///
/// A viewport narrow enough that `MIN_WIDTH` alone exceeds the fraction resolves to
/// `MIN_WIDTH`: at that size something has to overflow, and the panel being unreadable is
/// worse than the panes being cramped.
pub fn clamp_width(desired: f32, viewport_width: f32) -> f32 {
    let ceiling = MAX_WIDTH.min(viewport_width * MAX_FRACTION);
    desired.clamp(MIN_WIDTH, ceiling.max(MIN_WIDTH))
}

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
    // Captured for the rows rather than read inside the closure: a `uniform_list` render closure
    // gets a bare `&mut App` and cannot reach the entity's own state.
    let width = workspace.clamped_panel_width(window);

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
                    width,
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
        .w(px(width))
        .h_full()
        .overflow_hidden()
        .bg(theme.bg_panel)
        .border_r_1()
        .border_color(theme.focus_border(focused))
        .child(header(workspace, theme, cx))
        .child(list)
        .children(empty_notice(workspace, theme))
}

/// The panel width a pointer at `pointer_x` is asking for.
///
/// **Absolute, not a delta, and that distinction is a bug fix rather than a preference.** The
/// first version added `pointer_x - handle_center_x` to the panel's *current* width, which
/// reads as self-correcting and is not: `handle_center_x` comes from `hitbox.bounds`, written
/// during the last frame's prepaint, while the current width has already been updated by every
/// earlier event in the same batch. A mouse reporting at 500Hz against a 60Hz window delivers
/// six or seven moves per frame, and each one added the *whole* travel again, measured from a
/// reference that had not moved. The frame then painted the overshoot, the next batch measured
/// back from there and yanked it in — an oscillation, which is why it presented as the panel
/// showing two widths at once rather than as a wrong number.
///
/// Pairing `handle_center_x` with `painted_width` — the width those same bounds were laid out
/// at — recovers the row's left edge, which does not move during a drag. Every event in a
/// batch then computes the same answer from the same pointer, so the function is **idempotent**,
/// and that is what `applying_the_same_drag_event_twice_does_not_move_the_panel_twice` pins.
///
/// **Not testable end-to-end**: `VisualTestContext::simulate_event` calls `run_until_parked`
/// after every event, so the harness repaints between moves and the stale-bounds condition
/// cannot occur; `test_window`, the only way in below that, is `pub(crate)` to gpui. A burst
/// test was written and passed against the bug, exactly as CLAUDE.md's Lessons section warns.
fn width_from_drag(pointer_x: f32, handle_center_x: f32, painted_width: f32) -> f32 {
    // The handle is centred on the panel's right edge, so this is the panel's left edge.
    let panel_left = handle_center_x - painted_width;
    pointer_x - panel_left
}

/// The payload that marks a resize in flight.
///
/// A type rather than a `bool` on `Workspace` because `on_drag_move` dispatches on the
/// dragged value's `TypeId`, so this *is* the drag's identity — and gpui clears
/// `active_drag` itself on mouse-up, which means there is no end-of-drag flag to forget to
/// reset.
struct ResizePanel;

/// The seam between the panel and the panes, as a grab handle.
///
/// **Absolutely positioned rather than a sibling in the flex row**, so it costs no layout:
/// it straddles the boundary, half over the panel's own border and half over the pane, and
/// neither side gives up width for it. A gutter wide enough to hit would otherwise have to
/// take its pixels from one of them.
///
/// **Emitted last in the row**, because paint order is what decides hit-testing between
/// overlapping siblings — the same reason `chrome.rs` paints its resize corners last.
///
/// The drag itself goes through `on_drag`/`on_drag_move` rather than the obvious
/// `on_mouse_down` + `on_mouse_move` pair, and that is not a style preference: a `div`'s
/// move listener is gated on `hitbox.is_hovered`, so the drag would die the moment the
/// pointer outran this 5px strip — which a fast drag does within one frame. `on_drag_move`
/// fires in the capture phase for every move anywhere in the window while a `ResizePanel`
/// drag is live, which is what gpui's own doc comment recommends it for.
pub fn resize_handle(
    workspace: &Workspace,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let width = workspace.clamped_panel_width(window);
    // Nothing else in the app calls `on_drag`, so an active drag is *this* drag. Read rather
    // than stored, which is what keeps it honest: the flag cannot outlive the gesture.
    let dragging = cx.has_active_drag();

    // Invisible at rest — the panel's own border is what shows through, focus colour and all.
    // Hover and drag override it, because hover is transient and only appears while the
    // pointer is on the seam, while focus stays legible on the panel's other three edges.
    let line = if dragging { theme.accent } else { gpui::transparent_black() };

    div()
        .id("collection-resize-handle")
        .debug_selector(|| "collection-resize-handle".to_string())
        .group("panel-resize")
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(width - HANDLE_WIDTH / 2.0))
        .w(px(HANDLE_WIDTH))
        .flex()
        .justify_center()
        .cursor_col_resize()
        .on_drag(ResizePanel, |_, _, _, cx| cx.new(|_| Empty))
        .on_drag_move(cx.listener(
            move |workspace, event: &DragMoveEvent<ResizePanel>, _window, cx| {
                // `width` is captured, not re-read, and that pairing is the entire fix — see
                // `width_from_drag`. These bounds were painted at that width, and reading a
                // fresher one here is what made the panel flicker.
                let desired = width_from_drag(
                    f32::from(event.event.position.x),
                    f32::from(event.bounds.center().x),
                    width,
                );
                workspace.set_panel_width(desired, cx);
            },
        ))
        .on_click(cx.listener(|workspace, event: &ClickEvent, _window, cx| {
            if event.click_count() == 2 {
                workspace.set_panel_width(DEFAULT_WIDTH, cx);
            }
        }))
        .child(
            div()
                .w(px(2.0))
                .h_full()
                .bg(line)
                .group_hover("panel-resize", |style| style.bg(theme.accent)),
        )
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

    // A row being named is not an empty panel. The notice is absolutely positioned *over* the
    // list, so with a draft open its text renders straight through the name box.
    //
    // **Cosmetic only — it does not steal the click**, which is worth stating because the
    // overlap looks exactly like the dead-hitbox bugs elsewhere in this codebase and would
    // otherwise send the next reader hunting for one. `Interactivity::should_insert_hitbox`
    // (`div.rs`) is a disjunction over cursor, group, scroll offset, focus handle, hover style,
    // listeners and tooltip, and a plain styled `div` has none of them, so this element inserts
    // no hitbox at all and nothing below it is occluded.
    //
    // Its advice is wrong here besides: it explains `Ctrl+S`, which is the other way to create
    // the thing already being created.
    if workspace.new_node_row().is_some() {
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
///
/// Takes the panel's width rather than reading `DEFAULT_WIDTH`, because the panel is resizable:
/// a budget pinned to the default would keep putting a tooltip on names that plainly fit once
/// someone widened the panel, which is the exact noise the tooltip guard exists to avoid.
pub(crate) fn name_budget(panel_width: f32, depth: u16, is_directory: bool) -> usize {
    // 6 left pad, the chevron column, two 4px gaps, the kind column, 8 right pad. A folder's
    // kind column is the glyph's own width, so a folder name has more room than a request's —
    // which is the trade for the glyph sitting beside its name instead of a column away.
    let kind = if is_directory { GLYPH_WIDTH } else { METHOD_WIDTH };
    let chrome = 6. + CHEVRON + 4. + kind + 4. + 8. + f32::from(depth) * INDENT;
    (((panel_width - chrome) / 5.95).max(0.)) as usize
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
    panel_width: f32,
    depth: u16,
    is_directory: bool,
    color: gpui::Hsla,
    row_ix: usize,
) -> gpui::Stateful<Div> {
    let full = SharedString::from(name.to_string());
    let overflows = name.chars().count() > name_budget(panel_width, depth, is_directory);

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
    panel_width: f32,
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
                None => name_cell(&node.name, panel_width, node.depth, true, theme.text, row_ix)
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
                    panel_width,
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

    /// A wide window, so `MAX_FRACTION` is not the binding constraint.
    const WIDE: f32 = 1600.0;

    /// Pins the mapping, **not** the flicker.
    ///
    /// Worth being exact, because the honest version of this comment is less flattering than
    /// the one first written here: the shipped bug lived in *which width the handler read*, not
    /// in this arithmetic, so extracting the function removed it by construction and no
    /// assertion below can fail against it. What these two do catch is a wrong formula — losing
    /// `painted_width`, or measuring from the seam instead of the panel's left edge. The
    /// `DEFAULT_WIDTH`-instead-of-`painted_width` variant escapes them both, and is caught
    /// end-to-end by the *second* drag in `dragging_the_seam_widens_the_collection_panel`.
    #[test]
    fn applying_the_same_drag_event_twice_does_not_move_the_panel_twice() {
        // A panel painted 232 wide whose seam is therefore at x=340: the row starts at 108.
        let (center, painted) = (340.0, DEFAULT_WIDTH);
        let first = width_from_drag(440.0, center, painted);
        let again = width_from_drag(440.0, center, painted);

        assert_eq!(first, 332.0, "a pointer 100px right of the seam asks for 100px more panel");
        assert_eq!(first, again, "and asking twice from the same frame must not ask for 200");
    }

    #[test]
    fn a_drag_width_follows_the_pointer_rather_than_the_path_taken_to_it() {
        // Same destination, reached in one hop or three. A batch of moves within one frame all
        // carry the same stale `center`, so the last one has to win outright.
        let (center, painted) = (340.0, DEFAULT_WIDTH);
        let direct = width_from_drag(500.0, center, painted);
        let stepped = [380.0, 460.0, 500.0]
            .into_iter()
            .map(|x| width_from_drag(x, center, painted))
            .last()
            .expect("three steps");

        assert_eq!(direct, stepped);
    }

    #[test]
    fn the_clamp_holds_the_panel_between_its_floor_and_its_ceiling() {
        assert_eq!(clamp_width(DEFAULT_WIDTH, WIDE), DEFAULT_WIDTH, "untouched");
        assert_eq!(clamp_width(40.0, WIDE), MIN_WIDTH, "a drag past the floor stops");
        assert_eq!(
            clamp_width(5_000.0, WIDE),
            MAX_WIDTH,
            "and one past the absolute ceiling stops there"
        );
    }

    #[test]
    fn a_narrow_window_lowers_the_ceiling_below_the_absolute_one() {
        // The half that only a *stored* width exercises: 600 was legal on the monitor it was
        // set on, and this session is now open on a 900px window. Without the proportional
        // ceiling the panel would take two thirds of it and leave the request pane unusable —
        // and because the clamp runs at render, merely dragging the window narrower is enough
        // to trigger this with no resize of the panel at all.
        assert_eq!(clamp_width(MAX_WIDTH, 900.0), 450.0);
        assert!(
            clamp_width(MAX_WIDTH, 900.0) < MAX_WIDTH,
            "the window, not the constant, is what binds here"
        );
    }

    #[test]
    fn a_window_too_narrow_for_both_limits_keeps_the_panel_readable() {
        // Below ~360px the fraction wants a panel narrower than `MIN_WIDTH`, and the two
        // limits contradict each other. The floor wins: at that size something must overflow,
        // and a panel of pure chrome is worse than cramped panes. Asserted because the naive
        // `min(MAX, fraction)` reading of it produces a *negative* clamp range and panics in
        // `f32::clamp`.
        assert_eq!(clamp_width(DEFAULT_WIDTH, 200.0), MIN_WIDTH);
        assert_eq!(clamp_width(10.0, 0.0), MIN_WIDTH, "and a zero viewport is survivable");
    }

    #[test]
    fn the_name_budget_shrinks_with_depth_and_stays_sane() {
        // Decides whether a row gets a hover tooltip, so both failure modes are silent: a budget
        // of zero puts one on every row, and an enormous one puts it on none. A bounded range is
        // the only assertion that catches either — the same shape as the response viewer's
        // `the_widest_row_can_actually_be_reached`.
        let root = name_budget(DEFAULT_WIDTH, 0, false);
        assert!(
            (20..=32).contains(&root),
            "a root-level name should fit roughly 30 characters, got {root}"
        );

        // Each level of nesting costs `INDENT`, which is about two characters.
        for depth in 1..6u16 {
            let deeper = name_budget(DEFAULT_WIDTH, depth, false);
            assert!(
                deeper < name_budget(DEFAULT_WIDTH, depth - 1, false),
                "depth {depth} must have less room than depth {}",
                depth - 1
            );
            assert!(deeper > 0, "a name must never be budgeted to nothing");
        }

        // A folder's glyph column is narrower than a request's method column, so its name has
        // more room. Asserted rather than assumed: the two used to be equal, and a reader
        // comparing them is the only way to notice the columns diverged.
        assert!(name_budget(DEFAULT_WIDTH, 2, true) > name_budget(DEFAULT_WIDTH, 2, false));
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
