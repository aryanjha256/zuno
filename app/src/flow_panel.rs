//! The flow editor: the set of flows, and the order inside one.
//!
//! Modelled on `environment_panel` down to the shape — a list on the left, its contents on the
//! right, a reserved directory underneath — because it is the same problem: committed files in
//! the collection that are not requests, so the tree cannot hold them and a picker has to.
//!
//! **The only real difference is that order is the content.** A variable's position in an
//! environment means nothing; a step's position in a flow is the whole point, so the right pane
//! is a list you move things within rather than a table you type into.

use std::path::PathBuf;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use zuno_core::flow::{self, Flow};

use crate::actions::{FlowNew, FlowRename, FlowTrash};
use crate::input::TextInput;
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{Icon, glyph};

const WIDTH: f32 = 720.;
const HEIGHT: f32 = 420.;
const LIST_WIDTH: f32 = 168.;
const ROW_HEIGHT: f32 = 28.;

pub enum FlowEvent {
    /// A flow was renamed or removed, so anything holding its name has to follow.
    Changed,
}

impl EventEmitter<FlowEvent> for FlowPanel {}

pub struct FlowPanel {
    focus_handle: FocusHandle,
    root: PathBuf,
    flows: Vec<Flow>,
    selected: usize,
    /// Which step the keyboard is on, within the selected flow.
    step: usize,
    renaming: Option<Entity<TextInput>>,
    creating: Option<Entity<TextInput>>,
    message: Option<SharedString>,
    restore_focus: Option<FocusHandle>,
}

impl FlowPanel {
    pub fn new(
        root: PathBuf,
        restore_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);

        Self {
            focus_handle,
            flows: flow::scan(&root),
            root,
            selected: 0,
            step: 0,
            renaming: None,
            creating: None,
            message: None,
            restore_focus,
        }
    }

    pub fn restore_focus(&self) -> Option<FocusHandle> {
        self.restore_focus.clone()
    }

    fn current(&self) -> Option<&Flow> {
        self.flows.get(self.selected)
    }

    pub fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.flows.is_empty() || self.renaming.is_some() || self.creating.is_some() {
            return;
        }
        let count = self.flows.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
        self.step = 0;
        cx.notify();
    }

    pub fn select_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.flows.len() {
            self.selected = ix;
            self.step = 0;
            cx.notify();
        }
    }

    pub fn select_step(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.step = ix;
        cx.notify();
    }

    /// Move the step *cursor*, as distinct from moving the step.
    pub fn step_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.current().map(|flow| flow.steps.len()).unwrap_or(0) as isize;
        if count == 0 {
            return;
        }
        self.step = (self.step as isize + delta).rem_euclid(count) as usize;
        cx.notify();
    }

    /// Move the selected step, carrying the selection with it.
    ///
    /// **The selection follows the step, not the position.** Moving something up three places is
    /// three presses of the same key, and a selection that stayed put would move a different
    /// step each time.
    pub fn move_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(flow) = self.flows.get_mut(self.selected) else { return };
        let target = self.step as isize + delta;
        if target < 0 || target as usize >= flow.steps.len() {
            return;
        }

        let target = target as usize;
        flow.steps.swap(self.step, target);
        self.step = target;
        self.persist(cx);
    }

    pub fn remove_step(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.flows.get_mut(self.selected) else { return };
        if self.step >= flow.steps.len() {
            return;
        }
        flow.steps.remove(self.step);
        self.step = self.step.min(flow.steps.len().saturating_sub(1));
        self.persist(cx);
    }

    /// Write the selected flow back.
    ///
    /// After every edit rather than on close: a reorder is one keystroke and there is nothing to
    /// batch, and the alternative is a modal that can lose work — the thing the environment
    /// editor also refuses.
    fn persist(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.current() else { return };
        if let Err(error) = flow::save(&self.root, flow) {
            self.message = Some(format!("{error}").into());
        }
        cx.notify();
    }

    pub fn start_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new("", "user-lifecycle", "FlowName", cx));
        window.focus(&input.read(cx).focus_handle(cx));
        self.creating = Some(input);
        self.renaming = None;
        cx.notify();
    }

    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(flow) = self.current() else { return };
        let input = cx.new(|cx| TextInput::new(flow.name.clone(), "name", "FlowName", cx));
        window.focus(&input.read(cx).focus_handle(cx));
        self.renaming = Some(input);
        self.creating = None;
        cx.notify();
    }

    pub fn trash_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.current().map(|flow| flow.name.clone()) else { return };
        if let Err(error) = flow::trash(&self.root, &name) {
            self.message = Some(format!("{error}").into());
            cx.notify();
            return;
        }
        cx.emit(FlowEvent::Changed);
        self.rescan(None, window, cx);
    }

    /// Finish whichever name box is open. Returns whether one was.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(input) = self.creating.take() {
            let label = input.read(cx).text().to_string();
            match flow::create(&self.root, &label) {
                Ok(name) => self.rescan(Some(name), window, cx),
                Err(error) => {
                    self.message = Some(format!("{error}").into());
                    self.creating = Some(input);
                    cx.notify();
                }
            }
            return true;
        }

        if let Some(input) = self.renaming.take() {
            let label = input.read(cx).text().to_string();
            let Some(from) = self.current().map(|flow| flow.name.clone()) else { return true };
            match flow::rename(&self.root, &from, &label) {
                Ok(name) => {
                    cx.emit(FlowEvent::Changed);
                    self.rescan(Some(name), window, cx);
                }
                Err(error) => {
                    self.message = Some(format!("{error}").into());
                    self.renaming = Some(input);
                    cx.notify();
                }
            }
            return true;
        }

        false
    }

    /// Back out of the innermost thing, so `escape` can fall through to closing the panel.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.renaming.take().is_some() || self.creating.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
            return true;
        }
        false
    }

    fn rescan(&mut self, select: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = select.or_else(|| self.current().map(|flow| flow.name.clone()));
        self.flows = flow::scan(&self.root);
        self.selected = wanted
            .and_then(|name| self.flows.iter().position(|flow| flow.name == name))
            .unwrap_or(0);
        self.step = 0;
        self.renaming = None;
        self.creating = None;
        self.message = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    #[cfg(test)]
    pub fn listed(&self) -> Vec<String> {
        self.flows.iter().map(|flow| flow.name.clone()).collect()
    }

    #[cfg(test)]
    pub fn steps(&self) -> Vec<String> {
        self.current().map(|flow| flow.steps.clone()).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn selected_name(&self) -> Option<String> {
        self.current().map(|flow| flow.name.clone())
    }
}

impl Focusable for FlowPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FlowPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .track_focus(&self.focus_handle)
                    .key_context("FlowPanel")
                    .w(px(WIDTH))
                    .h(px(HEIGHT))
                    .flex()
                    .flex_col()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg_elevated)
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .text_sm()
                            .text_color(theme.text)
                            .child("Flows"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .flex()
                            .flex_row()
                            .child(self.list(&theme, cx))
                            .child(self.steps_pane(&theme, cx)),
                    )
                    .children(self.message.clone().map(|message| {
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(theme.status_server_error)
                            .child(message)
                    })),
            )
    }
}

impl FlowPanel {
    fn list(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let selected = self.selected;

        div()
            .id("flow-list")
            .w(px(LIST_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .children(self.flows.iter().enumerate().map(|(ix, flow)| {
                let is_selected = ix == selected;
                let renaming = is_selected.then(|| self.renaming.clone()).flatten();

                div()
                    .id(("flow-name", ix))
                    .debug_selector(move || format!("flow-name-{ix}"))
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .px_2()
                    .text_xs()
                    .cursor_pointer()
                    .bg(if is_selected { theme.bg_hover } else { theme.bg_elevated })
                    .text_color(if is_selected { theme.text } else { theme.text_muted })
                    .hover(|style| style.bg(theme.bg_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, _: &MouseDownEvent, _, cx| {
                            panel.select_at(ix, cx);
                        }),
                    )
                    .child(match renaming {
                        Some(input) => {
                            div().w_full().overflow_hidden().child(input).into_any_element()
                        }
                        None => div().child(flow.name.clone()).into_any_element(),
                    })
            }))
            .children(self.creating.clone().map(|input| {
                div()
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .px_2()
                    .overflow_hidden()
                    .text_xs()
                    .child(input)
            }))
            .child(div().px_1().py_1().child(crate::ui::text_action(
                "flow-new",
                "New flow".into(),
                "Create a flow",
                FlowNew,
                theme,
            )))
    }

    fn steps_pane(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let name = self.current().map(|flow| flow.name.clone()).unwrap_or_default();
        let steps = self.current().map(|flow| flow.steps.clone()).unwrap_or_default();
        let editable = self.current().is_some();
        let selected = self.step;

        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h(px(ROW_HEIGHT))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_xs()
                            .text_color(theme.text_faint)
                            .child(name),
                    )
                    .children(editable.then(|| {
                        crate::ui::icon_button(
                            "flow-rename",
                            Icon::Pencil,
                            "Rename this flow",
                            FlowRename,
                            theme,
                        )
                    }))
                    .children(editable.then(|| {
                        crate::ui::icon_button(
                            "flow-trash",
                            Icon::Trash,
                            "Move this flow to the trash",
                            FlowTrash,
                            theme,
                        )
                    })),
            )
            .child(
                div()
                    .id("flow-steps")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(steps.iter().enumerate().map(|(ix, step)| {
                        step_row(ix, step, ix == selected, theme, cx)
                    })),
            )
    }
}

fn step_row(
    ix: usize,
    step: &str,
    selected: bool,
    theme: &Theme,
    cx: &mut Context<FlowPanel>,
) -> impl IntoElement + use<> {
    div()
        .id(("flow-step", ix))
        .debug_selector(move || format!("flow-step-{ix}"))
        .w_full()
        .h(px(ROW_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .text_xs()
        .cursor_pointer()
        .bg(if selected { theme.bg_hover } else { theme.bg_elevated })
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, _: &MouseDownEvent, _, cx| panel.select_step(ix, cx)),
        )
        // The position, because the order is the content and a list with no numbers makes you
        // count rows to talk about one.
        .child(
            div()
                .flex_none()
                .w(px(20.))
                .font_family(theme.mono.clone())
                .text_color(theme.text_faint)
                .child(format!("{}", ix + 1)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(theme.text)
                .child(step.to_string()),
        )
        .child(
            div()
                .id(("flow-step-up", ix))
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |panel, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        panel.select_step(ix, cx);
                        panel.move_step(-1, cx);
                    }),
                )
                .child(glyph(Icon::ChevronUp, theme.text_muted, theme.text, 12.)),
        )
        .child(
            div()
                .id(("flow-step-down", ix))
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |panel, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        panel.select_step(ix, cx);
                        panel.move_step(1, cx);
                    }),
                )
                .child(glyph(Icon::ChevronDown, theme.text_muted, theme.text, 12.)),
        )
        .child(
            div()
                .id(("flow-step-remove", ix))
                .debug_selector(move || format!("flow-step-remove-{ix}"))
                .group(crate::ui::ICON_GROUP)
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.bg_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |panel, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        panel.select_step(ix, cx);
                        panel.remove_step(cx);
                    }),
                )
                .child(glyph(Icon::Close, theme.text_muted, theme.text, 12.)),
        )
}
