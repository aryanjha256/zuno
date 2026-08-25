//! Window chrome: the titlebar and the resize edges.
//!
//! Zuno asks for **client-side decorations**, so this module is the window's titlebar
//! and its resize borders — the compositor draws nothing. That's the same choice Zed
//! makes, and on Wayland it's effectively the only one that works consistently: GNOME
//! prefers CSD and won't reliably draw a server titlebar for us.
//!
//! It also explains a bug this fixes. `WindowOptions::window_decorations` defaults to
//! `None`, so GPUI was never told which mode to use; the window ended up client-decorated
//! with nobody drawing the decorations. Hence no close/minimize/maximize buttons and no
//! way to resize — the window had borders in name only.

use gpui::{
    CursorStyle, Div, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, ResizeEdge, SharedString, Stateful, Styled, Window, div, px,
};

use crate::theme::Theme;

/// Width of the invisible strips along each edge that start a resize.
///
/// 6px is the usual compromise: wide enough to hit without aiming, narrow enough not to
/// steal clicks from the content underneath.
const RESIZE_GRAB: f32 = 6.0;

/// Draw the titlebar.
///
/// The whole bar is a drag handle (`start_window_move`) and double-click maximises, which
/// is what people expect of a titlebar whether or not the OS drew it.
pub fn titlebar(title: SharedString, theme: &Theme, window: &Window) -> impl IntoElement {
    let controls = window.window_controls();
    let maximized = window.is_maximized();

    div()
        .id("titlebar")
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .h(px(34.))
        .pl_3()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        // Dragging anywhere on the bar moves the window.
        .on_mouse_down(MouseButton::Left, |event: &MouseDownEvent, window, _| {
            if event.click_count == 2 {
                window.zoom_window();
            } else {
                window.start_window_move();
            }
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w(px(0.))
                // The app's own name, which a client-decorated window has to supply
                // itself — there's no OS titlebar to put it in.
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.accent)
                        .child("Zuno"),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.border)
                        .child("│"),
                )
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(theme.text)
                        .child(title),
                )
                // **In the titlebar rather than at the end of the tab strip**, which is where a
                // `+` conventionally goes — because the strip hides itself at one buffer, so a
                // button there would be missing in exactly the state where you want a second tab.
                // Sitting inside the drag-to-move region is why `icon_button` stops propagation.
                .child(crate::ui::icon_button(
                    "action-new-tab",
                    crate::ui::Icon::Plus,
                    "New tab",
                    crate::actions::NewTab,
                    theme,
                )),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                // Clickable, not just informative. This label named its own shortcut while being
                // the one thing in the titlebar you couldn't press — which is the discoverability
                // gap in miniature: the app *told* you the keystroke and still had no button.
                .child(
                    div().px_2().child(crate::ui::text_action(
                        "theme-toggle",
                        theme.appearance.label().into(),
                        "Toggle theme",
                        crate::actions::ToggleTheme,
                        theme,
                    )),
                )
                .children(controls.minimize.then(|| {
                    control_button("minimize", "–", theme, false, |window| {
                        window.minimize_window()
                    })
                }))
                .children(controls.maximize.then(|| {
                    // The glyph reflects what the button will *do*, not the current state.
                    let glyph = if maximized { "▣" } else { "□" };
                    control_button("maximize", glyph, theme, false, |window| {
                        window.zoom_window()
                    })
                }))
                .child(control_button("close", "✕", theme, true, |window| {
                    window.remove_window()
                })),
        )
}

fn control_button(
    id: &'static str,
    glyph: &'static str,
    theme: &Theme,
    danger: bool,
    action: impl Fn(&mut Window) + 'static,
) -> impl IntoElement {
    let hover_bg = if danger {
        theme.status_server_error
    } else {
        theme.bg_hover
    };

    div()
        .id(id)
        // A no-op outside test builds. It's what lets a test click these buttons, which is the
        // only way to cover the propagation rule below — the bug was *in* the click path.
        .debug_selector(|| id.to_string())
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .w(px(44.))
        .h(px(34.))
        .text_xs()
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(hover_bg).text_color(theme.text))
        // **`stop_propagation` is load-bearing, and for a while this comment described a call
        // that wasn't here.** `on_mouse_down` registers a *Bubble*-phase listener, and GPUI runs
        // every bubble listener whose hitbox was hit, in reverse paint order, until one clears
        // `propagate_event`. The titlebar is this button's ancestor and its hitbox contains the
        // click, so without stopping here the button acts *and then* the titlebar calls
        // `start_window_move` — the compositor starts dragging a window the user was trying to
        // close. Verified against gpui 0.2.2's `Window::dispatch_mouse_event`, not assumed.
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            action(window);
        })
        .child(glyph.to_string())
}

/// The eight invisible strips that let the window be resized.
///
/// Absolutely positioned over the content, so the parent must be `relative()`. Corners
/// come last because later children win hit-testing, and a corner has to beat the two
/// edges it overlaps.
///
/// Skipped entirely while maximised — there is nothing to drag, and live strips would
/// just eat clicks along the content's edge.
pub fn resize_handles(window: &Window) -> Vec<Stateful<Div>> {
    if window.is_maximized() {
        return Vec::new();
    }

    let grab = px(RESIZE_GRAB);
    let corner = px(RESIZE_GRAB * 2.0);

    let mut handles = vec![
        edge("resize-top", ResizeEdge::Top)
            .top(px(0.))
            .left(px(0.))
            .right(px(0.))
            .h(grab)
            .cursor(CursorStyle::ResizeUpDown),
        edge("resize-bottom", ResizeEdge::Bottom)
            .bottom(px(0.))
            .left(px(0.))
            .right(px(0.))
            .h(grab)
            .cursor(CursorStyle::ResizeUpDown),
        edge("resize-left", ResizeEdge::Left)
            .top(px(0.))
            .bottom(px(0.))
            .left(px(0.))
            .w(grab)
            .cursor(CursorStyle::ResizeLeftRight),
        edge("resize-right", ResizeEdge::Right)
            .top(px(0.))
            .bottom(px(0.))
            .right(px(0.))
            .w(grab)
            .cursor(CursorStyle::ResizeLeftRight),
    ];

    handles.extend([
        edge("resize-top-left", ResizeEdge::TopLeft)
            .top(px(0.))
            .left(px(0.))
            .w(corner)
            .h(corner)
            .cursor(CursorStyle::ResizeUpLeftDownRight),
        edge("resize-top-right", ResizeEdge::TopRight)
            .top(px(0.))
            .right(px(0.))
            .w(corner)
            .h(corner)
            .cursor(CursorStyle::ResizeUpRightDownLeft),
        edge("resize-bottom-left", ResizeEdge::BottomLeft)
            .bottom(px(0.))
            .left(px(0.))
            .w(corner)
            .h(corner)
            .cursor(CursorStyle::ResizeUpRightDownLeft),
        edge("resize-bottom-right", ResizeEdge::BottomRight)
            .bottom(px(0.))
            .right(px(0.))
            .w(corner)
            .h(corner)
            .cursor(CursorStyle::ResizeUpLeftDownRight),
    ]);

    handles
}

fn edge(id: &'static str, edge: ResizeEdge) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, _| {
            window.start_window_resize(edge);
        })
}
