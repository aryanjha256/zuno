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
    App, AppContext, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px,
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
    /// Clone `url` into `location`/`name` — the name read off the URL, as `git clone` would.
    Clone { url: String, name: String, location: String },
}

/// Where the new workspace's contents come from. **Clone is how a teammate joins**: without it,
/// everyone after the first needs a terminal before Zuno is any use to them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Empty,
    Clone,
}

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    pub name: Entity<TextInput>,
    pub location: Entity<TextInput>,
    pub url: Entity<TextInput>,
    pub source: Source,
    /// Set while a clone runs, so the button says so and a second Enter does nothing.
    pub busy: bool,
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

        // The name field's context, so Enter and Escape mean what they mean there.
        let url = cx.new(|cx| {
            TextInput::new("", "git@github.com:acme/acme-platform.git", "WorkspaceName", cx)
        });

        let focus_handle = cx.focus_handle();
        window.focus(&name.read(cx).focus_handle(cx));

        Self {
            focus_handle,
            name,
            location,
            url,
            source: Source::Empty,
            busy: false,
            message: None,
            restore_focus,
        }
    }

    pub fn report(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.busy = false;
        self.message = Some(message.into());
        cx.notify();
    }

    /// Switch between an empty folder and a clone, with the keyboard in the first field of the
    /// one chosen.
    pub fn set_source(&mut self, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        self.source = source;
        self.message = None;
        let first = match source {
            Source::Empty => &self.name,
            Source::Clone => &self.url,
        };
        window.focus(&first.read(cx).focus_handle(cx));
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
        if self.busy {
            return;
        }
        let location = self.location.read(cx).text().trim().to_string();
        if self.source == Source::Clone {
            let url = self.url.read(cx).text().trim().to_string();
            if url.is_empty() {
                self.report("Paste the repository's URL", cx);
                return;
            }
            let Some(name) = zuno_core::git::repo_name(&url) else {
                self.report("That URL doesn't name a repository", cx);
                return;
            };
            cx.emit(WorkspaceEvent::Clone { url, name, location });
            return;
        }
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.report("Give the workspace a name", cx);
            return;
        }
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
        let cloning = self.source == Source::Clone;
        let switch = div()
            .flex()
            .flex_row()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(5.))
            .bg(theme.bg)
            .border_1()
            .border_color(theme.border)
            .child(source_tab("workspace-source-empty", "Empty folder", Source::Empty, self.source, &theme, cx))
            .child(source_tab("workspace-source-clone", "From a git URL", Source::Clone, self.source, &theme, cx));
        let first_field = if cloning {
            labelled("Repository URL", &theme, crate::ui::field_box(self.url.clone(), &theme))
                .into_any_element()
        } else {
            labelled("Name", &theme, crate::ui::field_box(self.name.clone(), &theme))
                .into_any_element()
        };
        let where_to = cloning.then(|| {
            let url = self.url.read(cx).text().to_string();
            let location = self.location.read(cx).text().trim().trim_end_matches('/').to_string();
            let line = match zuno_core::git::repo_name(&url) {
                Some(name) => format!(
                    "Clones into {location}/{name}, with your own git — your SSH keys and saved \
                     credentials work exactly as in a terminal."
                ),
                None => "Cloned with your own git — your SSH keys and saved credentials work \
                         exactly as in a terminal. The name comes from the repository."
                    .to_string(),
            };
            div().text_xs().text_color(theme.text_faint).child(line)
        });
        let action_label: SharedString = match (cloning, self.busy) {
            (true, true) => "Cloning…".into(),
            (true, false) => "Clone and open".into(),
            (false, _) => "Create".into(),
        };

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
                    .shadow_lg()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text)
                            .child("New workspace"),
                    )
                    .child(div().flex().child(switch))
                    .child(first_field)
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
                                    .child(crate::ui::field_box(self.location.clone(), &theme)),
                            )
                            .child(crate::ui::icon_button(
                                "workspace-browse",
                                crate::ui::Icon::Folder,
                                "Choose a folder",
                                WorkspaceBrowse,
                                &theme,
                            )),
                    ))
                    .children(where_to)
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
                                action_label,
                                "Create the workspace",
                                WorkspaceConfirm,
                                &theme,
                            )),
                    ),
            )
    }
}

/// One half of the Empty folder / From a git URL switch. The selected half is filled.
fn source_tab(
    id: &'static str,
    label: &'static str,
    source: Source,
    current: Source,
    theme: &crate::theme::Theme,
    cx: &mut Context<WorkspacePanel>,
) -> impl IntoElement + use<> {
    let selected = source == current;
    let tab = div()
        .id(id)
        .debug_selector(move || id.to_string())
        .h(px(24.))
        .px_3()
        .flex()
        .items_center()
        .rounded(px(3.))
        .text_xs()
        .cursor_pointer()
        .text_color(if selected { theme.text } else { theme.text_muted })
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |panel, _: &gpui::MouseDownEvent, window, cx| {
                // `set_source` puts the keyboard in the chosen field; letting the click bubble on
                // to the dialog's `track_focus` would take it straight back to the dialog, and the
                // URL typed next would land nowhere.
                cx.stop_propagation();
                panel.set_source(source, window, cx);
            }),
        )
        .child(label);
    if selected { tab.bg(theme.bg_hover) } else { tab }
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
