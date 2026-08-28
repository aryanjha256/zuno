//! Shared chrome: the icon set, the icon button, and the tooltip they hang off.
//!
//! **Why this exists.** Keyboard-first was never meant to be keyboard-*only*, but it drifted that
//! way: an audit found only six of Zuno's verbs had any mouse path at all, and nine had none —
//! including find, copy-as-curl, copy response, settings and import. A shortcut nobody can
//! discover is a feature nobody has, and the command palette only helps once you know it exists.
//!
//! So every button here does two jobs. It is a mouse path, and its tooltip **reads the keystroke
//! from the live keymap** (`workspace::keybinding_label`), so using the mouse teaches the keyboard
//! rather than replacing it. That is also why a rebinding can never leave a tooltip lying.

use gpui::{
    AnyView, App, AppContext, AssetSource, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, Result, SharedString, StatefulInteractiveElement, Styled,
    Svg, Window, div, px, svg,
};

use crate::theme::{ActiveTheme, Theme};

/// The height of the two bars that head the panes: the request's address bar and the response's
/// status line. Shared rather than written twice, because their whole job is to line up — and
/// `taffy` sizes with `BoxSizing::BorderBox`, so this includes each one's bottom border even
/// though the address bar's is 2px and the status line's is 1px.
pub const BAR_HEIGHT: f32 = 30.;

/// Icons, embedded at compile time.
///
/// **Embedded rather than installed.** gpui resolves an asset path through this source, so shipping
/// the SVGs as files would mean a new directory in the `.deb`, a path that differs between a cargo
/// run and an installed binary, and a blank icon whenever the two disagree. `include_bytes!` makes
/// the icons part of the binary and the whole question disappear — note this is the *opposite*
/// choice from the application icon, which has to be a real file in `hicolor/` because the
/// launcher, not Zuno, reads it.
///
/// A miss returns `Ok(None)` rather than an error because that is the trait's contract, and gpui
/// then draws **nothing at all** — `paint_svg` swallows it with `log_err()`. That silence is why
/// `every_icon_resolves` exists.
pub struct Assets;

/// One entry per icon. An enum rather than free strings so a typo is a compile error instead of an
/// invisible button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Search,
    Copy,
    Download,
    History,
    Terminal,
    Clipboard,
    Save,
    Settings,
    Plus,
    Globe,
}

impl Icon {
    pub fn path(self) -> &'static str {
        match self {
            Icon::Search => "icons/search.svg",
            Icon::Copy => "icons/copy.svg",
            Icon::Download => "icons/download.svg",
            Icon::History => "icons/history.svg",
            Icon::Terminal => "icons/terminal.svg",
            Icon::Clipboard => "icons/clipboard.svg",
            Icon::Save => "icons/save.svg",
            Icon::Settings => "icons/settings.svg",
            Icon::Plus => "icons/plus.svg",
            Icon::Globe => "icons/globe.svg",
        }
    }

    /// Every icon, for the test that proves each one loads.
    pub const ALL: &'static [Icon] = &[
        Icon::Search,
        Icon::Copy,
        Icon::Download,
        Icon::History,
        Icon::Terminal,
        Icon::Clipboard,
        Icon::Save,
        Icon::Settings,
        Icon::Plus,
        Icon::Globe,
    ];
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let bytes: &'static [u8] = match path {
            "icons/search.svg" => include_bytes!("../assets/icons/search.svg"),
            "icons/copy.svg" => include_bytes!("../assets/icons/copy.svg"),
            "icons/download.svg" => include_bytes!("../assets/icons/download.svg"),
            "icons/history.svg" => include_bytes!("../assets/icons/history.svg"),
            "icons/terminal.svg" => include_bytes!("../assets/icons/terminal.svg"),
            "icons/clipboard.svg" => include_bytes!("../assets/icons/clipboard.svg"),
            "icons/save.svg" => include_bytes!("../assets/icons/save.svg"),
            "icons/settings.svg" => include_bytes!("../assets/icons/settings.svg"),
            "icons/plus.svg" => include_bytes!("../assets/icons/plus.svg"),
            "icons/globe.svg" => include_bytes!("../assets/icons/globe.svg"),
            _ => return Ok(None),
        };
        Ok(Some(std::borrow::Cow::Borrowed(bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Icon::ALL
            .iter()
            .map(|icon| SharedString::from(icon.path()))
            .collect())
    }
}

/// A one-line tooltip: what the button does, and the key that does it.
///
/// gpui 0.2.2 ships no tooltip view, only the `tooltip()` hook that wants an `AnyView`, so this is
/// the smallest thing that satisfies it.
pub struct Tooltip {
    label: SharedString,
}

impl Tooltip {
    /// `"Find in response · Ctrl+F"`, or just the label when the action is unbound.
    ///
    /// The same `is_empty` guard as `workspace::hint_sentence`, for the same reason: a trailing
    /// separator with nothing after it looks like a rendering bug.
    ///
    /// Split out from `for_action` so the rule is testable — a rendered tooltip is not inspectable
    /// from the test platform, and this is the half that carries the decision.
    pub fn label_for(label: &str, action: &dyn gpui::Action, window: &Window) -> String {
        let key = crate::workspace::keybinding_label(action, window);
        if key.is_empty() {
            label.to_string()
        } else {
            format!("{label} · {key}")
        }
    }

    pub fn for_action(
        label: &str,
        action: &dyn gpui::Action,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        let label = Self::label_for(label, action, window);
        cx.new(|_| Tooltip {
            label: SharedString::from(label),
        })
        .into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.text)
            .child(self.label.clone())
    }
}

/// Ties a glyph's hover colour to its button's hover state.
///
/// One shared name is safe: `GroupHitboxes` keeps a *stack* per name and resolves to the innermost
/// one currently painting, so each button matches itself rather than the first on screen.
const ICON_GROUP: &str = "icon-button";

/// One icon glyph, always carrying its own colour.
///
/// **A parent's `text_color` does not reach an `svg()`, and this function exists because of that.**
/// `Interactivity::compute_style_internal` starts from `Style::default()` and refines only with the
/// element's *own* base style — inherited text style is never merged in — so an uncoloured svg has
/// `text.color == None`, and `Svg::paint` then skips `paint_svg` entirely. The result is an
/// invisible icon on a button that still hovers, still shows its tooltip, and still dispatches:
/// nothing looks broken except the pixels.
///
/// That is exactly what shipped. The rule was written as a comment on `icon_button` and then
/// applied to the wrapping `div` instead of to the glyph, so the comment was right and the code was
/// wrong three lines below it. It is a signature now rather than a sentence: there is no way to
/// build an icon without passing a colour.
fn glyph(icon: Icon, color: Hsla, hovered: Hsla) -> Svg {
    svg()
        .path(icon.path())
        .size(px(15.))
        .text_color(color)
        // `hover` on the parent cannot reach here either, for the same reason — hence the group.
        .group_hover(ICON_GROUP, move |style| style.text_color(hovered))
}

/// An icon button that dispatches an action, with a tooltip naming its keystroke.
///
/// Dispatches rather than calling anything directly, so the button and the keybinding are one verb
/// — the convention that the body-kind chip and the fold-all buttons were both found violating.
///
/// `tooltip` lives on `StatefulInteractiveElement`, so the `.id()` below is load-bearing twice
/// over — it names the element for tests *and* is what makes a tooltip possible at all. Same family
/// as `overflow_*_scroll`.
///
/// The colour lives on the glyph, not here — see `glyph`. This element only paints the background
/// and owns the hit area.
pub fn icon_button<A: gpui::Action + Clone + 'static>(
    id: &'static str,
    icon: Icon,
    label: &'static str,
    action: A,
    theme: &Theme,
) -> impl IntoElement + use<A> {
    let tooltip_action = action.clone();

    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .group(ICON_GROUP)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        .tooltip(move |window, cx| Tooltip::for_action(label, &tooltip_action, window, cx))
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window, cx| {
                // `on_mouse_down` is Bubble-phase, so an *ancestor's* handler fires too. One of
                // these buttons sits inside the drag-to-move titlebar, where without this the
                // click would also ask the compositor to start dragging the window — the exact
                // bug the window controls shipped with for several milestones. Unconditional
                // rather than per-call-site: harmless where there is no ancestor handler, and
                // impossible to forget when a button is later moved somewhere there is one.
                cx.stop_propagation();
                window.dispatch_action(action.boxed_clone(), cx);
            },
        )
        .child(glyph(icon, theme.text_muted, theme.text))
}

/// A thin bar showing how far a horizontally scrollable list is scrolled, and how much it hides.
///
/// **A `UniformListDecoration`, and it has to be.** The obvious build — a sibling `div` reading
/// the scroll handle — draws nothing on the frame the body first appears, because
/// `max_offset` and `bounds` are written during `interactivity.prepaint`, which runs *after* the
/// surrounding element tree was built. The bar would then wait for some unrelated repaint to
/// show up. Decorations are computed inside that same prepaint, after the geometry lands, and
/// are laid out at the list's own bounds — which is exactly an overlay.
///
/// **An indicator, not a control**, and styled to admit it: no hover state, no pointer cursor,
/// three pixels tall. Dragging would mean mirroring the track geometry onto the view plus a drag
/// mode, to duplicate a gesture the trackpad, the wheel and `left`/`right` already perform. A
/// thing that looks draggable and isn't is the dead-control bug this codebase keeps finding, so
/// the answer is to not look draggable.
pub struct HScrollIndicator {
    pub scroll: gpui::UniformListScrollHandle,
    pub color: gpui::Hsla,
}

impl gpui::UniformListDecoration for HScrollIndicator {
    fn compute(
        &self,
        _visible: std::ops::Range<usize>,
        bounds: gpui::Bounds<gpui::Pixels>,
        scroll_offset: gpui::Point<gpui::Pixels>,
        _item_height: gpui::Pixels,
        _item_count: usize,
        _window: &mut Window,
        _cx: &mut gpui::App,
    ) -> gpui::AnyElement {
        let hidden = self.scroll.0.borrow().base_handle.max_offset().width;
        let viewport = bounds.size.width;

        // Nothing hidden means nothing to say. The bar exists to report "there is more to the
        // right"; drawing it permanently would report that when it isn't true.
        if hidden <= px(0.) || viewport <= px(0.) {
            return div().into_any_element();
        }

        // The thumb is as much of the *track* as the viewport is of the content, expressed as a
        // fraction rather than in pixels. `bounds` here is the list's outer width but the
        // decoration is laid out inside its padding, so a pixel figure is wrong by the padding
        // on both sides; a fraction is right whatever the padding turns out to be.
        let content = viewport + hidden;
        let ratio = (viewport / content).clamp(0.02, 1.);
        // Offsets run negative as you scroll right, hence the negation.
        let progress = (-scroll_offset.x / hidden).clamp(0., 1.);

        // **Full height, pushed to the bottom with `justify_end`.** The first version made the
        // root itself 3px tall and set `bottom_0` on it — which does nothing, because gpui lays
        // a decoration out as a *root* at the list's origin, so "bottom" meant the bottom of the
        // 3px box and the bar appeared along the top edge of the response.
        //
        // Safe to cover the whole list: `should_insert_hitbox` creates a hitbox only for an
        // element with a cursor, a group, a hover style, a focus handle or a listener, and this
        // has none — so it cannot swallow a click meant for a row.
        // **The bar is placed by arithmetic, not by alignment.** A decoration is a child of the
        // list, so gpui shifts it by `scroll_offset` along with the rows — the bar slid *left*
        // by the amount you scrolled right and *up* by the amount you scrolled down, drifting
        // into the middle of the response.
        //
        // Two tidier-looking versions each pinned one axis and not the other. `justify_end` with
        // a top margin on the child does nothing vertically, because flex end-alignment pins the
        // item to the bottom whatever its margin. Moving both margins to a `relative` root with
        // an `absolute` child then lost the *horizontal* pin. Margins on the bar itself, offset
        // from the viewport height `compute` already hands us, need neither.
        div()
            .w_full()
            .h_full()
            .child(
                div()
                    .debug_selector(|| "h-scroll".to_string())
                    .ml(-scroll_offset.x)
                    .mt(bounds.size.height - px(3.) - scroll_offset.y)
                    .w_full()
                    .h(px(3.))
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .debug_selector(|| "h-scroll-thumb".to_string())
                            .flex_none()
                            .ml(gpui::DefiniteLength::Fraction(progress * (1. - ratio)))
                            .w(gpui::DefiniteLength::Fraction(ratio))
                            // A floor, or a very wide body leaves a thumb too small to see —
                            // the one thing it must never be.
                            .min_w(px(24.))
                            .h_full()
                            .rounded_full()
                            .bg(self.color),
                    ),
            )
            .into_any_element()
    }
}

/// A vertical rule drawn as a glyph, for separating items inside a single row.
///
/// `theme.border` and not a text colour: this is a rule that happens to be a character, so being
/// barely-there is the point. It is the one place `border` is legitimately used to paint text —
/// `theme::tests::border_is_too_dim_to_read_as_text` names this function as that exception.
pub fn separator(theme: &Theme) -> impl IntoElement + use<> {
    div().flex_none().text_xs().text_color(theme.border).child("│")
}

/// A text label that dispatches an action — for places where a word carries information an icon
/// can't, like the selected environment's name.
pub fn text_action<A: gpui::Action + Clone + 'static>(
    id: &'static str,
    text: SharedString,
    label: &'static str,
    action: A,
    theme: &Theme,
) -> impl IntoElement + use<A> {
    let tooltip_action = action.clone();

    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex_none()
        .px_1()
        .rounded_sm()
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover).text_color(theme.text))
        .tooltip(move |window, cx| Tooltip::for_action(label, &tooltip_action, window, cx))
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window, cx| {
                // See `icon_button`: Bubble phase means an ancestor handler runs too.
                cx.stop_propagation();
                window.dispatch_action(action.boxed_clone(), cx);
            },
        )
        .child(text)
}
