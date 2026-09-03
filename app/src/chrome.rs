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
    StatefulInteractiveElement,
    ParentElement, ResizeEdge, SharedString, Stateful, Styled, Window, div, px,
};

use crate::theme::{Appearance, Theme};
use crate::ui::Icon;

/// Width of the invisible strips along each edge that start a resize.
///
/// 6px is the usual compromise: wide enough to hit without aiming, narrow enough not to
/// steal clicks from the content underneath.
const RESIZE_GRAB: f32 = 6.0;

/// Draw the titlebar.
///
/// The whole bar is a drag handle (`start_window_move`) and double-click maximises, which
/// is what people expect of a titlebar whether or not the OS drew it.
/// The bar's height, which the application menu anchors itself below.
pub const TITLEBAR_HEIGHT: f32 = 34.;

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
        .h(px(TITLEBAR_HEIGHT))
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
                // The app's own name, which a client-decorated window has to supply itself —
                // there's no OS titlebar to put it in — and also the application menu's button.
                //
                // Hand-rolled rather than `text_action`, which paints `text_muted`: this is the
                // app's identity and stays accent and semibold. The chevron is what says it
                // opens something.
                .child(
                    div()
                        .id("app-menu-button")
                        .debug_selector(|| "app-menu-button".to_string())
                        .group(crate::ui::ICON_GROUP)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.accent)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.bg_hover))
                        .tooltip(move |window, cx| {
                            crate::ui::Tooltip::for_action(
                                "Application menu",
                                &crate::actions::OpenAppMenu,
                                window,
                                cx,
                            )
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            move |_: &MouseDownEvent, window, cx| {
                                // Inside the drag-to-move region, so without this the click
                                // also asks the compositor to start dragging the window.
                                cx.stop_propagation();
                                window.dispatch_action(
                                    Box::new(crate::actions::OpenAppMenu),
                                    cx,
                                );
                            },
                        )
                        .child("Zuno")
                        .child(crate::ui::glyph(
                            crate::ui::Icon::ChevronDown,
                            theme.accent,
                            theme.accent,
                            crate::ui::GLYPH_INLINE,
                        )),
                )
                .child(crate::ui::separator(&theme))
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
                // An icon, and it names the *destination* rather than the current theme — a sun
                // while dark means "this makes it light". Same rule as the maximize button three
                // lines down, which is the argument for it: two buttons in one titlebar reading
                // opposite conventions is worse than either convention on its own.
                //
                // It was the word "Dark"/"Light" until now, which is the one place the titlebar
                // stated a *state* instead of an action, and the only text button among icons.
                .child(
                    div().px_2().child(crate::ui::icon_button(
                        "theme-toggle",
                        match theme.appearance {
                            Appearance::Dark => Icon::Sun,
                            Appearance::Light => Icon::Moon,
                        },
                        "Toggle theme",
                        crate::actions::ToggleTheme,
                        theme,
                    )),
                )
                .children(controls.minimize.then(|| {
                    control_button("minimize", Icon::Minimize, theme, false, |window| {
                        window.minimize_window()
                    })
                }))
                .children(controls.maximize.then(|| {
                    // The icon reflects what the button will *do*, not the current state.
                    let icon = if maximized { Icon::Restore } else { Icon::Maximize };
                    control_button("maximize", icon, theme, false, |window| {
                        window.zoom_window()
                    })
                }))
                .child(control_button("close", Icon::Close, theme, true, |window| {
                    window.remove_window()
                })),
        )
}

fn control_button(
    id: &'static str,
    icon: Icon,
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
        // The glyph takes its colour from this group, since an `svg()` cannot be reached by an
        // ancestor's `hover`.
        .group(crate::ui::ICON_GROUP)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .w(px(44.))
        .h(px(34.))
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
        .child(crate::ui::glyph(icon, theme.text_muted, theme.text, crate::ui::GLYPH))
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
