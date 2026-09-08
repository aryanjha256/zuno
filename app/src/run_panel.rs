//! The run report: what each step did, and why a failed one failed.
//!
//! **A panel rather than a picker.** The picker is a chooser — you open it to pick one thing and
//! it closes. This is a report you read, whose rows happen to also be openable, and whose most
//! important content is the *second* line of a failed row. Reusing the picker would have meant
//! bending a one-line-per-row list around a variable-height one.
//!
//! Rows arrive while the run is still going, because a forty-request run that shows nothing until
//! it ends is indistinguishable from one that has hung.

use std::path::PathBuf;

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Window, div, px,
};
use zuno_core::runner::Outcome;

use crate::theme::{ActiveTheme, Theme};

const WIDTH: f32 = 620.;
const MAX_HEIGHT: f32 = 420.;

pub enum RunEvent {
    /// A row was chosen: open that request so it can be debugged.
    Open(PathBuf),
}

impl gpui::EventEmitter<RunEvent> for RunPanel {}

pub struct RunPanel {
    focus_handle: FocusHandle,
    /// What is being run, for the title — "Users", or the workspace name for a whole collection.
    subject: SharedString,
    root: PathBuf,
    outcomes: Vec<Outcome>,
    /// How many steps the run will attempt, so the header can count towards it.
    total: usize,
    running: bool,
    cancelled: bool,
    restore_focus: Option<FocusHandle>,
}

impl RunPanel {
    pub fn new(
        subject: impl Into<SharedString>,
        root: PathBuf,
        total: usize,
        restore_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);

        Self {
            focus_handle,
            subject: subject.into(),
            root,
            outcomes: Vec::new(),
            total,
            running: true,
            cancelled: false,
            restore_focus,
        }
    }

    pub fn restore_focus(&self) -> Option<FocusHandle> {
        self.restore_focus.clone()
    }

    pub fn push(&mut self, outcome: Outcome, cx: &mut Context<Self>) {
        self.outcomes.push(outcome);
        cx.notify();
    }

    pub fn finish(&mut self, cancelled: bool, cx: &mut Context<Self>) {
        self.running = false;
        self.cancelled = cancelled;
        cx.notify();
    }

    pub fn running(&self) -> bool {
        self.running
    }

    fn passed(&self) -> usize {
        self.outcomes.iter().filter(|outcome| outcome.passed()).count()
    }

    #[cfg(test)]
    pub fn rows_for_test(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .map(|outcome| {
                let mark = if outcome.passed() { "pass" } else { "fail" };
                let detail = outcome
                    .unresolved
                    .iter()
                    .cloned()
                    .chain(outcome.failures.iter().map(|failure| failure.to_string()))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("{mark} {} {detail}", outcome.label).trim_end().to_string()
            })
            .collect()
    }

    #[cfg(test)]
    pub fn summary_for_test(&self) -> String {
        self.summary()
    }

    fn summary(&self) -> String {
        if self.running {
            return format!("Running… {} of {}", self.outcomes.len(), self.total);
        }

        let failed = self.outcomes.len() - self.passed();
        let mut text = format!("{} passed, {failed} failed", self.passed());
        // A cancelled run has to say so: the counts alone read as a complete result, and acting
        // on "0 failed" when half the steps never ran is the worst thing this panel could cause.
        if self.cancelled {
            text.push_str(" — stopped early");
        }
        text
    }
}

impl Focusable for RunPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RunPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let root = self.root.clone();

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
                    .key_context("RunPanel")
                    .w(px(WIDTH))
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
                            .child(format!("Run · {}", self.subject)),
                    )
                    .child(
                        div()
                            .id("run-rows")
                            .max_h(px(MAX_HEIGHT))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .children(self.outcomes.iter().enumerate().map(|(ix, outcome)| {
                                row(ix, outcome, &root, &theme, cx)
                            })),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .border_t_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(self.summary()),
                    ),
            )
    }
}

fn row(
    ix: usize,
    outcome: &Outcome,
    root: &std::path::Path,
    theme: &Theme,
    cx: &mut Context<RunPanel>,
) -> impl IntoElement + use<> {
    let passed = outcome.passed();
    let target = root.join(&outcome.label);
    // Every failure, one per line. A row that says "failed" and not *what* failed is a run you
    // have to repeat by hand, which is the cost this whole feature removes.
    let details: Vec<String> = outcome
        .error
        .iter()
        .map(|error| error.to_string())
        .chain(outcome.failures.iter().map(|failure| failure.to_string()))
        .collect();

    let status = match outcome.status {
        Some(code) => SharedString::from(code.to_string()),
        None => SharedString::from("—"),
    };

    div()
        .id(("run-row", ix))
        .debug_selector(move || format!("run-row-{ix}"))
        .w_full()
        .flex()
        .flex_col()
        .px_3()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                cx.emit(RunEvent::Open(target.clone()));
            }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .w(px(10.))
                        .h(px(10.))
                        .rounded_full()
                        .bg(if passed { theme.accent } else { theme.status_server_error }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(theme.text)
                        .child(outcome.label.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme.mono.clone())
                        .text_color(theme.text_muted)
                        .child(status),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(64.))
                        .font_family(theme.mono.clone())
                        .text_color(theme.text_faint)
                        .child(format!("{}ms", outcome.duration.as_millis())),
                ),
        )
        .children(details.into_iter().map(|detail| {
            div()
                .pl_4()
                .text_color(theme.status_server_error)
                .child(detail)
        }))
}
