//! The new-workspace dialog: a name, and where to put it.
//!
//! **Two fields, unlike `import_panel`'s one.** There the URL/path distinction was already
//! carried by the text, so a mode would have described a difference you could see. Here the name
//! and the location are genuinely separate answers, and the location has a default worth showing
//! — which is the whole IDE bargain: naming one is enough, and the override exists for the case
//! that matters, a workspace living inside the repo it belongs to.
//!
//! Concrete rather than a form framework, still. `import_panel` made that bet and `picker.rs`
//! made it before that; a second consumer is the moment to *consider* lifting the shared part
//! out, not the moment it pays.

use gpui::{
    App, AppContext, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px,
};

use crate::actions::{WorkspaceBrowse, WorkspaceConfirm};
use crate::input::TextInput;
use crate::theme::ActiveTheme;

const WIDTH: f32 = 520.;

/// One variant, unlike `ImportEvent`. Dismissal is handled by the workspace's own
/// `WorkspaceDismiss` handler rather than routed back through an event, since nothing here has
/// to happen inside the panel first.
pub enum WorkspaceEvent {
    /// Name and location, exactly as typed. Turning them into a directory belongs to the
    /// workspace, which owns the filesystem and the registry.
    Confirmed { name: String, location: String },
}

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    pub name: Entity<TextInput>,
    pub location: Entity<TextInput>,
    message: Option<SharedString>,
    restore_focus: Option<FocusHandle>,
}

impl EventEmitter<WorkspaceEvent> for WorkspacePanel {}

impl WorkspacePanel {
    pub fn new(
        location: String,
        restore_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| TextInput::new("", "payments", "WorkspaceName", cx));
        let location = cx.new(|cx| {
            TextInput::new(location, "where the folder goes", "WorkspaceLocation", cx)
        });

        let focus_handle = cx.focus_handle();
        window.focus(&name.read(cx).focus_handle(cx));

        Self {
            focus_handle,
            name,
            location,
            message: None,
            restore_focus,
        }
    }

    pub fn report(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.message = Some(message.into());
        cx.notify();
    }

    pub fn restore_focus(&self) -> Option<FocusHandle> {
        self.restore_focus.clone()
    }

    /// Fill the location from the folder dialog.
    ///
    /// Select-all then replace, rather than a new `set_text` API: `replace_text_in_range` with a
    /// `None` range acts on the *selection*, and going through it keeps the sanitisation, the
    /// undo entry and the `Changed` emit that every other edit path gets.
    pub fn set_location(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.location.update(cx, |input, cx| {
            input.select_all_text(cx);
            input.replace_text_in_range(None, &path, window, cx);
        });
        cx.notify();
    }

    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.report("Give the workspace a name", cx);
            return;
        }
        let location = self.location.read(cx).text().trim().to_string();
        cx.emit(WorkspaceEvent::Confirmed { name, location });
    }
}

impl Focusable for WorkspacePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // A modal owns the mouse as well as the keyboard; scroll gates on hit-testing
            // rather than propagation, so a scrim that only catches clicks is not enough.
            .occlude()
            .child(
                div()
                    .track_focus(&self.focus_handle)
                    .key_context("WorkspacePanel")
                    .w(px(WIDTH))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg_elevated)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text)
                            .child("New workspace"),
                    )
                    .child(labelled("Name", &theme, input_box(self.name.clone(), &theme)))
                    // The browse button pairs with the *box*, not with the field: a row of
                    // [label-over-box, button] centres the button against both lines together,
                    // which lands it up at the label rather than on the input.
                    .child(labelled(
                        "Location",
                        &theme,
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(input_box(self.location.clone(), &theme)),
                            )
                            .child(crate::ui::icon_button(
                                "workspace-browse",
                                crate::ui::Icon::Folder,
                                "Choose a folder",
                                WorkspaceBrowse,
                                &theme,
                            )),
                    ))
                    .children(self.message.clone().map(|message| {
                        div()
                            .text_xs()
                            .text_color(theme.status_server_error)
                            .child(message)
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .child(crate::ui::text_action(
                                "workspace-create",
                                "Create".into(),
                                "Create the workspace",
                                WorkspaceConfirm,
                                &theme,
                            )),
                    ),
            )
    }
}

/// `use<C>` rather than `use<>`: the return has to mention every type parameter in scope, and
/// `content` is one even though it is written as `impl IntoElement` (CLAUDE.md).
fn labelled<C: IntoElement>(
    label: &'static str,
    theme: &crate::theme::Theme,
    content: C,
) -> impl IntoElement + use<C> {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(theme.text_faint).child(label))
        .child(content)
}

fn input_box(input: Entity<TextInput>, theme: &crate::theme::Theme) -> impl IntoElement + use<> {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg)
        .text_xs()
        .child(input)
}
