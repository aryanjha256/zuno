//! The GraphQL schema browser: under the query editor, in the same tab.
//!
//! **A split, not a tab of its own.** ROADMAP once planned a workspace tab — the first that was not
//! a request — but switching tabs to read the schema is exactly what a browser should spare you,
//! and a tab either way means switching. Below the query, at the pane's full width, is the one
//! place both can be read at once: the request and response panes already sit side by side, so a
//! split by width would leave two quarter-width strips, while a query rarely needs much height.
//!
//! **What it shows is decided in core** (`graphql::browse`); this lays it out. The type list is
//! computed once per schema and a type's page once per visit — never per frame — so browsing a
//! schema with a thousand types costs a filter over names while typing, and nothing else.

use std::sync::Arc;

use gpui::{
    AppContext, Context, DragMoveEvent, Empty, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Pixels, SharedString, StatefulInteractiveElement,
    Styled, UniformListScrollHandle, WeakEntity, div, px, uniform_list,
};
use zuno_core::graphql::browse::{TypeDetail, TypeSummary};
use zuno_core::graphql::complete::SchemaIndex;

use crate::input::TextInput;
use crate::request_view::RequestView;
use crate::theme::Theme;

/// The browser's height when first opened.
const DEFAULT_HEIGHT: f32 = 260.;
/// Neither half may be dragged smaller than this.
const MIN_HEIGHT: f32 = 120.;
const ROW_HEIGHT: f32 = 22.;
const LIST_WIDTH: f32 = 200.;

/// The payload marking a resize of this split in flight — the collection panel's `ResizePanel`,
/// for the same reasons.
struct ResizeBrowser;

pub struct SchemaBrowser {
    pub filter: Entity<TextInput>,
    /// The type whose page is showing.
    selected: Option<String>,
    /// Where Back goes: the types visited before this one, most recent last.
    back: Vec<String>,
    height: Pixels,
    index: Option<Arc<SchemaIndex>>,
    types: Arc<Vec<TypeSummary>>,
    detail: Option<TypeDetail>,
    list_scroll: UniformListScrollHandle,
}

impl SchemaBrowser {
    pub fn new(index: Option<Arc<SchemaIndex>>, cx: &mut Context<RequestView>) -> Self {
        let mut browser = Self {
            filter: cx.new(|cx| TextInput::new("", "Filter types", "SchemaFilter", cx)),
            selected: None,
            back: Vec::new(),
            height: px(DEFAULT_HEIGHT),
            index: None,
            types: Arc::new(Vec::new()),
            detail: None,
            list_scroll: UniformListScrollHandle::new(),
        };
        browser.set_index(index);
        browser
    }

    /// The schema to browse. A fresh one keeps the page open if that type still exists — a
    /// re-fetch should not throw away where you were — and otherwise starts at the first type,
    /// which is the query root.
    pub fn set_index(&mut self, index: Option<Arc<SchemaIndex>>) {
        let same = match (&self.index, &index) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.index = index;
        self.types = Arc::new(self.index.as_ref().map(|index| index.types()).unwrap_or_default());
        let keep = self
            .selected
            .clone()
            .filter(|name| self.types.iter().any(|summary| summary.name == *name));
        self.back.retain(|name| self.types.iter().any(|summary| summary.name == *name));
        match keep.or_else(|| self.types.first().map(|summary| summary.name.clone())) {
            Some(name) => self.show(&name),
            None => {
                self.selected = None;
                self.detail = None;
            }
        }
    }

    /// Go to `name`'s page, remembering this one for Back.
    pub fn open(&mut self, name: &str) {
        if self.selected.as_deref() == Some(name) {
            return;
        }
        if let Some(current) = self.selected.take() {
            self.back.push(current);
        }
        self.show(name);
    }

    pub fn go_back(&mut self) {
        if let Some(previous) = self.back.pop() {
            self.show(&previous);
        }
    }

    fn show(&mut self, name: &str) {
        self.selected = Some(name.to_string());
        self.detail = self.index.as_ref().and_then(|index| index.describe(name));
    }

    /// The page showing, for tests.
    #[cfg(test)]
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The types the filter leaves, by index into `types`: names that start with it first, then
    /// names that contain it — the order every other list in the app uses.
    fn visible(&self, cx: &gpui::App) -> Vec<usize> {
        let needle = self.filter.read(cx).text().trim().to_ascii_lowercase();
        if needle.is_empty() {
            return (0..self.types.len()).collect();
        }
        let (mut starts, mut contains) = (Vec::new(), Vec::new());
        for (ix, summary) in self.types.iter().enumerate() {
            let name = summary.name.to_ascii_lowercase();
            if name.starts_with(&needle) {
                starts.push(ix);
            } else if name.contains(&needle) {
                contains.push(ix);
            }
        }
        starts.append(&mut contains);
        starts
    }
}

/// Apply `f` to the active GraphQL buffer's browser, if it has one open.
fn with_browser(view: &mut RequestView, f: impl FnOnce(&mut SchemaBrowser)) {
    if let Some(browser) = view.kind.as_graphql_mut().and_then(|graphql| graphql.browser.as_mut()) {
        f(browser);
    }
}

/// The browser: a divider to drag, the type list, and the chosen type's page.
pub fn render(
    browser: &SchemaBrowser,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> impl IntoElement + use<> {
    let view = cx.entity().downgrade();
    let height = browser.height;

    div()
        .id("schema-browser")
        .debug_selector(|| "schema-browser".to_string())
        .flex_none()
        .flex()
        .flex_col()
        .h(height)
        .border_t_1()
        .border_color(theme.border)
        // On the browser rather than on the divider, because this box's *bottom* edge is fixed
        // while it resizes — so a height measured up from it to the pointer is right even though
        // the bounds are last frame's, which is the trap `DragMoveEvent::bounds` sets (CLAUDE.md).
        .on_drag_move(cx.listener(
            move |view, event: &DragMoveEvent<ResizeBrowser>, _window, cx| {
                let wanted = event.bounds.bottom() - event.event.position.y;
                with_browser(view, |browser| {
                    browser.height = wanted.max(px(MIN_HEIGHT));
                });
                cx.notify();
            },
        ))
        .child(divider(theme, cx))
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .child(type_list(browser, view.clone(), theme, cx))
                .child(type_page(browser, view, theme)),
        )
}

/// The grab strip along the top edge. Double-click puts the height back.
fn divider(theme: &Theme, cx: &mut Context<RequestView>) -> impl IntoElement + use<> {
    let dragging = cx.active_drag_cursor_style() == Some(gpui::CursorStyle::ResizeRow);
    div()
        .id("schema-browser-divider")
        .debug_selector(|| "schema-browser-divider".to_string())
        .group("schema-resize")
        .flex_none()
        .h(px(5.))
        .w_full()
        .flex()
        .items_center()
        .cursor_row_resize()
        .on_drag(ResizeBrowser, |_, _, _, cx| cx.new(|_| Empty))
        .on_click(cx.listener(|view, event: &gpui::ClickEvent, _window, cx| {
            if event.click_count() == 2 {
                with_browser(view, |browser| browser.height = px(DEFAULT_HEIGHT));
                cx.notify();
            }
        }))
        .child(
            div()
                .h(px(2.))
                .w_full()
                .bg(if dragging { theme.accent } else { gpui::transparent_black() })
                .group_hover("schema-resize", |style| style.bg(theme.accent)),
        )
}

fn type_list(
    browser: &SchemaBrowser,
    view: WeakEntity<RequestView>,
    theme: &Theme,
    cx: &mut Context<RequestView>,
) -> impl IntoElement + use<> {
    let visible = browser.visible(cx);
    let types = browser.types.clone();
    let selected = browser.selected.clone();
    let count = visible.len();
    let row_theme = theme.clone();

    let list = uniform_list("schema-types", count, move |range, _window, _cx| {
        range
            .map(|row| {
                let summary = &types[visible[row]];
                let name = summary.name.clone();
                let active = selected.as_deref() == Some(summary.name.as_str());
                let view = view.clone();
                // The operation a root type serves says more than "type" does.
                let kind = SharedString::from(summary.root.unwrap_or(summary.kind));
                div()
                    .id(("schema-type", row))
                    .debug_selector(move || format!("schema-type-{row}"))
                    // `w_full`, or the row is as wide as its label and the rest does not click —
                    // the picker's trap, which this list would otherwise repeat.
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .cursor_pointer()
                    .font_family(row_theme.mono.clone())
                    .text_xs()
                    .bg(if active { row_theme.bg_hover } else { row_theme.bg_panel })
                    .text_color(if active { row_theme.text } else { row_theme.text_muted })
                    .hover(|style| style.bg(row_theme.bg_hover))
                    .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                        let _ = view.update(cx, |view, cx| {
                            with_browser(view, |browser| browser.open(&name));
                            cx.notify();
                        });
                    })
                    .child(SharedString::from(zuno_core::request::elide(&summary.name, 24).into_owned()))
                    .child(div().flex_none().text_color(row_theme.text_faint).child(kind))
            })
            .collect()
    })
    .track_scroll(browser.list_scroll.clone())
    .flex_1();

    div()
        .flex_none()
        .w(px(LIST_WIDTH))
        .flex()
        .flex_col()
        .bg(theme.bg_panel)
        .border_r_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .p_1()
                .child(crate::ui::field_box(browser.filter.clone(), theme)),
        )
        .child(list)
}

/// The chosen type's page: what it is, what it relates to, and its fields or values.
fn type_page(
    browser: &SchemaBrowser,
    view: WeakEntity<RequestView>,
    theme: &Theme,
) -> impl IntoElement + use<> {
    let link = |id: SharedString, name: String, theme: &Theme| {
        let view = view.clone();
        let target = name.clone();
        let selector = id.clone();
        div()
            .id(id)
            .debug_selector(move || selector.to_string())
            .cursor_pointer()
            .text_color(theme.accent)
            .hover(|style| style.underline())
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                let _ = view.update(cx, |view, cx| {
                    with_browser(view, |browser| browser.open(&target));
                    cx.notify();
                });
            })
            .child(SharedString::from(name))
    };

    let page = div()
        .id("schema-page")
        .flex_1()
        .min_w(px(0.))
        .overflow_y_scroll()
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .text_xs();

    let Some(detail) = browser.detail.as_ref() else {
        return page
            .text_color(theme.text_muted)
            .child("No schema loaded — use From server on the Schema row.");
    };

    let back = (!browser.back.is_empty()).then(|| {
        let view = view.clone();
        div()
            .id("schema-back")
            .debug_selector(|| "schema-back".to_string())
            // The glyph brightens through this group — an `svg()` does not inherit `hover`.
            .group(crate::ui::ICON_GROUP)
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .text_color(theme.text_muted)
            .hover(|style| style.text_color(theme.text))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                let _ = view.update(cx, |view, cx| {
                    with_browser(view, |browser| browser.go_back());
                    cx.notify();
                });
            })
            .child(crate::ui::glyph(
                crate::ui::Icon::ChevronLeft,
                theme.text_muted,
                theme.text,
                crate::ui::GLYPH_INLINE,
            ))
            .child(SharedString::from(browser.back.last().cloned().unwrap_or_default()))
    });

    let heading = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap_2()
        .child(div().text_color(theme.text_faint).child(detail.kind))
        .child(
            div()
                .text_sm()
                .text_color(theme.text)
                .font_family(theme.mono.clone())
                .child(SharedString::from(detail.name.clone())),
        );

    let related = (!detail.related.is_empty()).then(|| {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .font_family(theme.mono.clone())
            .child(div().text_color(theme.text_faint).child(detail.related_label))
            .children(detail.related.iter().enumerate().map(|(ix, name)| {
                link(
                    SharedString::from(format!("schema-related-{ix}")),
                    name.clone(),
                    theme,
                )
            }))
    });

    let entries = detail.entries.iter().enumerate().map(|(ix, entry)| {
        let deprecated = entry.deprecated.is_some();
        let mut signature = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .font_family(theme.mono.clone())
            .text_color(if deprecated { theme.text_faint } else { theme.text })
            .child(SharedString::from(entry.name.clone()));
        if !entry.arguments.is_empty() {
            signature = signature.child("(");
            for (arg_ix, arg) in entry.arguments.iter().enumerate() {
                if arg_ix > 0 {
                    signature = signature.child(", ");
                }
                signature = signature
                    .child(div().text_color(theme.text_muted).child(SharedString::from(format!("{}: ", arg.name))))
                    .child(type_name(
                        SharedString::from(format!("schema-arg-{ix}-{arg_ix}")),
                        &arg.ty,
                        arg.target.clone(),
                        &link,
                        theme,
                    ));
                if let Some(default) = &arg.default {
                    signature = signature.child(
                        div().text_color(theme.text_faint).child(SharedString::from(format!(" = {default}"))),
                    );
                }
            }
            signature = signature.child(")");
        }
        if !entry.ty.is_empty() {
            signature = signature.child(": ").child(type_name(
                SharedString::from(format!("schema-field-{ix}")),
                &entry.ty,
                entry.target.clone(),
                &link,
                theme,
            ));
        }
        if let Some(default) = &entry.default {
            signature = signature
                .child(div().text_color(theme.text_faint).child(SharedString::from(format!(" = {default}"))));
        }

        let note = match (&entry.deprecated, &entry.description) {
            (Some(reason), _) if !reason.is_empty() => Some(format!("Deprecated: {reason}")),
            (Some(_), _) => Some("Deprecated".to_string()),
            (None, Some(description)) => Some(description.clone()),
            (None, None) => None,
        };
        div()
            .flex()
            .flex_col()
            .child(signature)
            .children(note.map(|note| {
                div().pl_3().text_color(theme.text_faint).child(SharedString::from(note))
            }))
    });

    page.children(back)
        .child(heading)
        .children(detail.description.clone().map(|description| {
            div().text_color(theme.text_muted).child(SharedString::from(description))
        }))
        .children(related)
        .children(entries)
}

/// A type as written — `[User!]!` — with the named type inside it a link when it has a page.
fn type_name(
    id: SharedString,
    written: &str,
    target: Option<String>,
    link: &dyn Fn(SharedString, String, &Theme) -> gpui::Stateful<gpui::Div>,
    theme: &Theme,
) -> gpui::AnyElement {
    let Some(target) = target else {
        return div()
            .text_color(theme.text_muted)
            .child(SharedString::from(written.to_string()))
            .into_any_element();
    };
    // The brackets and `!` around the name stay plain; only the name itself opens its page.
    let at = written.find(target.as_str()).unwrap_or(0);
    let (before, rest) = written.split_at(at);
    let after = &rest[target.len().min(rest.len())..];
    div()
        .flex()
        .flex_row()
        .text_color(theme.text_muted)
        .child(SharedString::from(before.to_string()))
        .child(link(id, target, theme))
        .child(SharedString::from(after.to_string()))
        .into_any_element()
}
