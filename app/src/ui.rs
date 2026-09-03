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
    Close,
    ChevronLeft,
    ChevronRight,
    ChevronDown,
    Folder,
    FolderOpen,
    Replace,
    ReplaceAll,
    Minimize,
    Maximize,
    Restore,
    Sun,
    Moon,
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
            Icon::Close => "icons/close.svg",
            Icon::ChevronLeft => "icons/chevron-left.svg",
            Icon::ChevronRight => "icons/chevron-right.svg",
            Icon::ChevronDown => "icons/chevron-down.svg",
            Icon::Folder => "icons/folder.svg",
            Icon::FolderOpen => "icons/folder-open.svg",
            Icon::Replace => "icons/replace.svg",
            Icon::ReplaceAll => "icons/replace-all.svg",
            Icon::Minimize => "icons/minimize.svg",
            Icon::Maximize => "icons/maximize.svg",
            Icon::Restore => "icons/restore.svg",
            Icon::Sun => "icons/sun.svg",
            Icon::Moon => "icons/moon.svg",
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
        Icon::Close,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::ChevronDown,
        Icon::Folder,
        Icon::FolderOpen,
        Icon::Replace,
        Icon::ReplaceAll,
        Icon::Minimize,
        Icon::Maximize,
        Icon::Restore,
        Icon::Sun,
        Icon::Moon,
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
            "icons/close.svg" => include_bytes!("../assets/icons/close.svg"),
            "icons/chevron-left.svg" => include_bytes!("../assets/icons/chevron-left.svg"),
            "icons/chevron-right.svg" => include_bytes!("../assets/icons/chevron-right.svg"),
            "icons/chevron-down.svg" => include_bytes!("../assets/icons/chevron-down.svg"),
            "icons/folder.svg" => include_bytes!("../assets/icons/folder.svg"),
            "icons/folder-open.svg" => include_bytes!("../assets/icons/folder-open.svg"),
            "icons/replace.svg" => include_bytes!("../assets/icons/replace.svg"),
            "icons/replace-all.svg" => include_bytes!("../assets/icons/replace-all.svg"),
            "icons/minimize.svg" => include_bytes!("../assets/icons/minimize.svg"),
            "icons/maximize.svg" => include_bytes!("../assets/icons/maximize.svg"),
            "icons/restore.svg" => include_bytes!("../assets/icons/restore.svg"),
            "icons/sun.svg" => include_bytes!("../assets/icons/sun.svg"),
            "icons/moon.svg" => include_bytes!("../assets/icons/moon.svg"),
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
    /// One per line. A tooltip is the one surface here that may take more than one, because it
    /// floats and nothing clips it — so a picker row hands over both of its columns rather than
    /// whichever one happened to be cut.
    lines: Vec<SharedString>,
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

    /// A tooltip carrying plain text, for a label the layout had to cut short.
    pub fn text(label: impl Into<SharedString>, cx: &mut App) -> AnyView {
        Self::lines([label.into()], cx)
    }

    /// A tooltip of several lines, one per string.
    ///
    /// **No maximum width.** The strings that need a tooltip are the ones too long for the row
    /// they came from, and wrapping one mid-path is the thing the tooltip exists to undo — a
    /// bound would break the primary line to save space the tooltip does not have to save. A
    /// pathological path therefore makes a very wide tooltip, which is the accepted cost.
    pub fn lines(lines: impl IntoIterator<Item = SharedString>, cx: &mut App) -> AnyView {
        let lines: Vec<SharedString> = lines.into_iter().filter(|line| !line.is_empty()).collect();
        cx.new(|_| Tooltip { lines }).into()
    }

    pub fn for_action(
        label: &str,
        action: &dyn gpui::Action,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        let label = Self::label_for(label, action, window);
        cx.new(|_| Tooltip {
            lines: vec![SharedString::from(label)],
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
            .flex()
            .flex_col()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.text)
            // One element per line rather than one string with newlines in it: `shape_line` has
            // a `debug_assert!` against newlines, so a `\n` here is a panic waiting for a debug
            // build.
            .children(
                self.lines
                    .iter()
                    .map(|line| div().whitespace_nowrap().child(line.clone())),
            )
    }
}

/// A glyph standing on its own, in a button sized around it.
pub(crate) const GLYPH: f32 = 15.;
/// A glyph set beside text, which is `text_xs` — `rems(0.75)` against gpui's `px(16.)` rem, so
/// 12px. Matching it matters: at `GLYPH` the icon is 25% taller than the word next to it.
pub(crate) const GLYPH_INLINE: f32 = 12.;

/// Ties a glyph's hover colour to the hover state of whatever surface owns it.
///
/// One shared name is safe: `GroupHitboxes` keeps a *stack* per name and resolves to the innermost
/// one currently painting, so each glyph matches its own owner rather than the first on screen.
///
/// Usually that owner is the `icon_button` around it. The tab strip puts the group on the whole
/// **tab** instead, because revealing a close button when the cursor is anywhere on the tab is the
/// feature — see `workspace::tab_strip`.
pub(crate) const ICON_GROUP: &str = "icon-button";

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
pub(crate) fn glyph(icon: Icon, color: Hsla, hovered: Hsla, size: f32) -> Svg {
    svg()
        .path(icon.path())
        .size(px(size))
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
        .child(glyph(icon, theme.text_muted, theme.text, GLYPH))
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

/// Cut a line into segments, each carrying the colour that covers it and whether it is marked.
///
/// **Overlapping styles have to be split, not layered.** `StyledText::compute_runs` walks its
/// highlights in order doing `range.start - ix`, so it requires them sorted and disjoint and
/// underflows on anything else; `shape_line` likewise takes runs that tile the text exactly. So a
/// search match sitting inside a coloured token cannot simply be pushed on top — the token has to
/// be cut at the match's edges and the pieces styled separately.
///
/// Collecting every boundary either rule cares about and then asking, per segment, what applies
/// avoids a case per overlap. Nesting the two rules was the first attempt in the editor and it
/// needed one branch per way a range can straddle another.
///
/// Segments tile `0..len` in order, so callers can emit runs directly from them.
pub fn split_spans(
    len: usize,
    colours: &[(std::ops::Range<usize>, gpui::Hsla)],
    marked: Option<std::ops::Range<usize>>,
) -> Vec<(std::ops::Range<usize>, Option<gpui::Hsla>, bool)> {
    let mut cuts: Vec<usize> = Vec::with_capacity(colours.len() * 2 + 4);
    cuts.push(0);
    cuts.push(len);
    for (range, _) in colours {
        cuts.push(range.start.min(len));
        cuts.push(range.end.min(len));
    }
    if let Some(marked) = &marked {
        cuts.push(marked.start.min(len));
        cuts.push(marked.end.min(len));
    }
    cuts.sort_unstable();
    cuts.dedup();

    cuts.windows(2)
        .map(|pair| {
            let (from, to) = (pair[0], pair[1]);
            let colour = colours
                .iter()
                .find(|(range, _)| range.start <= from && range.end >= to)
                .map(|(_, colour)| *colour);
            let marked = marked
                .as_ref()
                .is_some_and(|marked| marked.start <= from && marked.end >= to);
            (from..to, colour, marked)
        })
        .filter(|(range, _, _)| !range.is_empty())
        .collect()
}

/// The colour one JSON token is painted in.
///
/// One function, three callers — the request editor, the raw response view, and (indirectly) the
/// JSON outline. The palette is the thing that must not drift: a body authored in the editor and
/// the response it comes back in should read as the same language, and the surest way to break
/// that is to write the match twice.
pub fn syntax_colour(kind: zuno_core::TokenKind, syntax: &crate::theme::SyntaxTheme) -> gpui::Hsla {
    match kind {
        zuno_core::TokenKind::Key => syntax.key,
        zuno_core::TokenKind::String => syntax.string,
        zuno_core::TokenKind::Number => syntax.number,
        zuno_core::TokenKind::Literal => syntax.literal,
        zuno_core::TokenKind::Punct => syntax.punct,
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

/// An icon *and* a word, dispatching one action.
///
/// For a control where the icon carries the verb but the word says which table it acts on — a
/// bare `+` in a section header would not. `tint` is the resting colour for both halves and is
/// passed rather than fixed: the add control is the one affirmative action in a header of muted
/// text, and hover changes only the background so that stays true.
pub fn icon_text_action<A: gpui::Action + Clone + 'static>(
    id: &'static str,
    icon: Icon,
    text: SharedString,
    label: &'static str,
    action: A,
    tint: Hsla,
    theme: &Theme,
) -> impl IntoElement + use<A> {
    let tooltip_action = action.clone();

    div()
        .id(id)
        .debug_selector(move || id.to_string())
        // The glyph reads its colour from this group; an `svg()` never inherits one.
        .group(ICON_GROUP)
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .flex_none()
        .px_1()
        .rounded_sm()
        .text_color(tint)
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        .tooltip(move |window, cx| Tooltip::for_action(label, &tooltip_action, window, cx))
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window, cx| {
                // See `icon_button`: Bubble phase means an ancestor handler runs too.
                cx.stop_propagation();
                window.dispatch_action(action.boxed_clone(), cx);
            },
        )
        .child(glyph(icon, tint, tint, GLYPH_INLINE))
        .child(text)
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;

    fn colour(n: f32) -> gpui::Hsla {
        hsla(n, 1., 0.5, 1.)
    }

    /// The contract every caller depends on: segments tile `0..len` exactly, in order, with no
    /// gaps and no overlaps.
    ///
    /// Worth asserting as a property rather than case by case, because the failure is not a
    /// wrong colour. `StyledText::compute_runs` walks highlights doing `range.start - ix`, so a
    /// gap paints the wrong text and an overlap **underflows a `usize`**; `shape_line` requires
    /// the same tiling. Getting this wrong panics rather than looking odd.
    fn assert_tiles(segments: &[(std::ops::Range<usize>, Option<gpui::Hsla>, bool)], len: usize) {
        let mut at = 0;
        for (range, _, _) in segments {
            assert_eq!(range.start, at, "gap or overlap at {at}: {segments:?}");
            assert!(range.end > range.start, "empty segment: {segments:?}");
            at = range.end;
        }
        assert_eq!(at, len, "segments must reach the end: {segments:?}");
    }

    #[test]
    fn a_mark_inside_a_colour_splits_it_into_three() {
        // The case the whole helper exists for: a search match landing inside a coloured token.
        // Layering the two would leave overlapping ranges, which is the underflow above.
        let colours = [(0..10, colour(0.5))];
        let segments = split_spans(10, &colours, Some(3..6));
        assert_tiles(&segments, 10);

        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].0, 0..3);
        assert_eq!(segments[1].0, 3..6);
        assert!(segments.iter().all(|(_, c, _)| *c == Some(colour(0.5))));
        assert_eq!(
            segments.iter().map(|(_, _, hit)| *hit).collect::<Vec<_>>(),
            vec![false, true, false]
        );
    }

    #[test]
    fn gaps_between_colours_still_produce_segments() {
        // Uncoloured text — whitespace between JSON tokens, or anything the lexer declined to
        // recognise — is not absent, it is default-coloured. Dropping it would leave a hole.
        let colours = [(0..2, colour(0.1)), (5..7, colour(0.2))];
        let segments = split_spans(9, &colours, None);
        assert_tiles(&segments, 9);
        assert_eq!(segments[1].0, 2..5);
        assert_eq!(segments[1].1, None);
    }

    #[test]
    fn a_mark_straddling_two_colours_is_cut_at_both() {
        let colours = [(0..4, colour(0.1)), (4..8, colour(0.2))];
        let segments = split_spans(8, &colours, Some(2..6));
        assert_tiles(&segments, 8);
        assert_eq!(
            segments
                .iter()
                .map(|(r, _, hit)| (r.clone(), *hit))
                .collect::<Vec<_>>(),
            vec![(0..2, false), (2..4, true), (4..6, true), (6..8, false)]
        );
    }

    #[test]
    fn ranges_past_the_end_are_clamped_rather_than_trusted() {
        // A match can outrun the text it is being drawn into: the raw view cuts a line at
        // `MAX_DISPLAY_LINE`, and a stale range can survive a frame while the body is reindexed.
        // Clamping keeps the tiling valid; trusting the caller panics.
        let colours = [(0..99, colour(0.3))];
        assert_tiles(&split_spans(5, &colours, Some(2..99)), 5);
        assert_tiles(&split_spans(5, &[], Some(7..9)), 5);
    }

    #[test]
    fn nothing_to_split_is_one_segment_and_empty_text_is_none() {
        assert_eq!(split_spans(4, &[], None).len(), 1);
        assert!(split_spans(0, &[], None).is_empty());
    }

    #[test]
    fn every_token_kind_has_a_colour_and_they_are_distinguishable() {
        // A palette where two kinds collide is worse than no colour: it reads as a rendering
        // fault rather than as a deliberate choice.
        let theme = crate::theme::Theme::new(crate::theme::Appearance::Dark, "mono".into());
        let kinds = [
            zuno_core::TokenKind::Key,
            zuno_core::TokenKind::String,
            zuno_core::TokenKind::Number,
            zuno_core::TokenKind::Literal,
            zuno_core::TokenKind::Punct,
        ];

        let colours: Vec<_> = kinds
            .iter()
            .map(|kind| syntax_colour(*kind, &theme.syntax))
            .collect();
        for (ix, a) in colours.iter().enumerate() {
            for b in &colours[ix + 1..] {
                assert_ne!(
                    (a.h, a.s, a.l),
                    (b.h, b.s, b.l),
                    "two token kinds share a colour: {colours:?}"
                );
            }
        }
    }
}
