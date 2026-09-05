//! The environment editor: the set, and what is in each one.
//!
//! Selecting an environment has existed since M3. Authoring one meant opening a text editor,
//! which is the bet ROADMAP recorded — "the files are the interface" — and the same bet the
//! collection made before it grew a panel. This is that panel for environments.
//!
//! **It owns its own file I/O**, unlike `workspace_panel`, which emits and lets `Workspace` do
//! the writing. The difference is what the write touches: creating a workspace mutates a global
//! registry and switches the whole app, while this only edits files in one directory. What does
//! reach outward is emitted — a rename or a removal can move the ground under the *selected*
//! environment, and a new secret is what arms the `.gitignore` offer.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use zuno_core::environment::{self, EnvironmentFile};

use crate::actions::{
    EnvNewEnvironment, EnvNewVariable, EnvRenameEnvironment, EnvTrashEnvironment,
};
use crate::input::TextInput;
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{Icon, glyph};

const WIDTH: f32 = 720.;
const HEIGHT: f32 = 420.;
const LIST_WIDTH: f32 = 168.;
const ROW_HEIGHT: f32 = 28.;

pub enum EnvEvent {
    /// An environment was renamed. The selected one may be it, and the session stores a name.
    Renamed { from: String, to: String },
    /// An environment is gone. Same reason.
    Removed(String),
    /// A secret was written for the first time in this sitting — the trigger for the
    /// `.gitignore` offer, which until now only fired when a *selected* environment had one.
    SecretWritten,
}

impl EventEmitter<EnvEvent> for EnvironmentPanel {}

struct VarRow {
    name: Entity<TextInput>,
    value: Entity<TextInput>,
    secret: bool,
}

impl VarRow {
    fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.name.read(cx).focus_handle(cx).is_focused(window)
            || self.value.read(cx).focus_handle(cx).is_focused(window)
    }
}

pub struct EnvironmentPanel {
    focus_handle: FocusHandle,
    root: PathBuf,
    /// `globals` first, then everything `scan` lists. Globals is not in `scan` because it
    /// cannot be *selected*; it can very much be edited, and leaving it out would just move
    /// the text-editor problem rather than solve it.
    names: Vec<String>,
    selected: usize,
    /// The file as read, kept so a save can put back a committed placeholder that the merged
    /// view has no room for. See `EnvironmentFile`.
    loaded: EnvironmentFile,
    rows: Vec<VarRow>,
    renaming: Option<Entity<TextInput>>,
    creating: Option<Entity<TextInput>>,
    message: Option<SharedString>,
    restore_focus: Option<FocusHandle>,
}

impl EnvironmentPanel {
    pub fn new(
        root: PathBuf,
        active: Option<String>,
        restore_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut names = vec![environment::GLOBALS.to_string()];
        names.extend(environment::scan(&root).into_iter().map(|env| env.name));

        let selected = active
            .and_then(|name| names.iter().position(|other| *other == name))
            .unwrap_or(0);

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);

        let mut panel = Self {
            focus_handle,
            root,
            names,
            selected,
            loaded: EnvironmentFile::default(),
            rows: Vec::new(),
            renaming: None,
            creating: None,
            message: None,
            restore_focus,
        };
        panel.load(cx);
        panel
    }

    pub fn restore_focus(&self) -> Option<FocusHandle> {
        self.restore_focus.clone()
    }

    fn name(&self) -> Option<&String> {
        self.names.get(self.selected)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.name().cloned() else {
            self.loaded = EnvironmentFile::default();
            self.rows.clear();
            return;
        };

        self.loaded = environment::read(&self.root, &name).unwrap_or(EnvironmentFile {
            name,
            ..Default::default()
        });

        let resolved = self.loaded.resolved();
        self.rows = resolved
            .values
            .iter()
            .map(|(key, value)| VarRow {
                name: cx.new(|cx| TextInput::new(key.clone(), "name", "EnvField", cx)),
                value: cx.new(|cx| TextInput::new(value.clone(), "value", "EnvField", cx)),
                secret: resolved.is_secret(key),
            })
            .collect();
        cx.notify();
    }

    /// Rebuild both halves from the rows and write them.
    ///
    /// The one rule worth reading twice: a secret row whose name *also* exists in the committed
    /// file keeps that committed value untouched. That is the placeholder-plus-real-token pattern
    /// `EnvironmentFile` exists for, and rebuilding the committed map from the rows alone would
    /// quietly overwrite the placeholder with the token.
    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.name().cloned() else { return };

        let mut committed = BTreeMap::new();
        let mut local = BTreeMap::new();
        let mut new_secret = false;

        for row in &self.rows {
            let key = row.name.read(cx).text().trim().to_string();
            if key.is_empty() {
                continue;
            }
            let value = row.value.read(cx).text().to_string();

            if row.secret {
                let was_secret = self.loaded.local.contains_key(&key);
                new_secret |= !was_secret;
                local.insert(key.clone(), value);

                // Only an *already* secret name keeps its committed entry, and the distinction
                // is the whole of invariant 10. For one that was already secret, the committed
                // value is a separate placeholder and must survive. For one being marked secret
                // now, the committed value *is* the thing being hidden — carrying it over would
                // move the token into the sidecar and leave a copy in the file that gets pushed.
                if was_secret && let Some(placeholder) = self.loaded.committed.get(&key) {
                    committed.insert(key, placeholder.clone());
                }
            } else {
                committed.insert(key, value);
            }
        }

        let file = EnvironmentFile {
            name,
            committed,
            local,
        };
        // Moving through the list calls this on the way out of each one, so an unconditional
        // write would create `globals.json` for anybody who merely opened the editor.
        if file == self.loaded {
            return;
        }
        if let Err(error) = environment::save(&self.root, &file) {
            self.message = Some(format!("{error}").into());
            cx.notify();
            return;
        }

        self.loaded = file;
        self.message = None;
        if new_secret {
            cx.emit(EnvEvent::SecretWritten);
        }
    }

    /// Persist and close. There is no discard, deliberately: the settings panel commits as you
    /// go for the same reason, and an editor over files that can silently throw an edit away is
    /// a worse story than one that always lands.
    pub fn commit(&mut self, cx: &mut Context<Self>) {
        self.save(cx);
    }

    pub fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.names.is_empty() || self.renaming.is_some() || self.creating.is_some() {
            return;
        }
        self.save(cx);

        let count = self.names.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
        self.load(cx);
    }

    pub fn select_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.names.len() || ix == self.selected {
            return;
        }
        self.save(cx);
        self.selected = ix;
        self.load(cx);
    }

    pub fn add_variable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = VarRow {
            name: cx.new(|cx| TextInput::new("", "name", "EnvField", cx)),
            value: cx.new(|cx| TextInput::new("", "value", "EnvField", cx)),
            secret: false,
        };
        window.focus(&row.name.read(cx).focus_handle(cx));
        self.rows.push(row);
        cx.notify();
    }

    pub fn remove_variable(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.rows.len() {
            return;
        }
        self.rows.remove(ix);
        self.save(cx);
        cx.notify();
    }

    pub fn toggle_secret(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(ix) else { return };
        row.secret = !row.secret;
        self.save(cx);
        cx.notify();
    }

    pub fn focused_row(&self, window: &Window, cx: &App) -> Option<usize> {
        self.rows.iter().position(|row| row.is_focused(window, cx))
    }

    // ---- the set of environments -------------------------------------------

    pub fn start_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new("", "staging", "EnvRename", cx));
        window.focus(&input.read(cx).focus_handle(cx));
        self.creating = Some(input);
        self.renaming = None;
        cx.notify();
    }

    /// Globals is the always-active layer rather than a peer, so it has no rename and no delete
    /// — renaming it would mean "stop being the layer", which is not a rename.
    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.name().cloned() else { return };
        if name == environment::GLOBALS {
            self.report("globals is always active — it has no other name", cx);
            return;
        }

        let input = cx.new(|cx| TextInput::new(name, "name", "EnvRename", cx));
        window.focus(&input.read(cx).focus_handle(cx));
        self.renaming = Some(input);
        self.creating = None;
        cx.notify();
    }

    pub fn trash_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.name().cloned() else { return };
        if name == environment::GLOBALS {
            self.report("globals is always active — it cannot be removed", cx);
            return;
        }

        if let Err(error) = environment::trash(&self.root, &name) {
            self.report(format!("{error}"), cx);
            return;
        }

        cx.emit(EnvEvent::Removed(name));
        self.rescan(None, window, cx);
    }

    /// Finish whichever text box is open. Returns whether one was.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(input) = self.creating.take() {
            let label = input.read(cx).text().to_string();
            match environment::create(&self.root, &label) {
                Ok(name) => self.rescan(Some(name), window, cx),
                Err(error) => {
                    self.report(format!("{error}"), cx);
                    self.creating = Some(input);
                }
            }
            return true;
        }

        if let Some(input) = self.renaming.take() {
            let label = input.read(cx).text().to_string();
            let Some(from) = self.name().cloned() else { return true };
            match environment::rename(&self.root, &from, &label) {
                Ok(to) => {
                    cx.emit(EnvEvent::Renamed { from, to: to.clone() });
                    self.rescan(Some(to), window, cx);
                }
                Err(error) => {
                    self.report(format!("{error}"), cx);
                    self.renaming = Some(input);
                }
            }
            return true;
        }

        false
    }

    /// Back out of the innermost thing. Returns whether there was one, so `escape` can fall
    /// through to closing the panel when there wasn't.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.renaming.take().is_some() || self.creating.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
            return true;
        }
        false
    }

    fn rescan(&mut self, select: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = select.or_else(|| self.name().cloned());

        self.names = vec![environment::GLOBALS.to_string()];
        self.names
            .extend(environment::scan(&self.root).into_iter().map(|env| env.name));
        self.selected = wanted
            .and_then(|name| self.names.iter().position(|other| *other == name))
            .unwrap_or(0);

        self.renaming = None;
        self.creating = None;
        self.message = None;
        window.focus(&self.focus_handle);
        self.load(cx);
    }

    fn report(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.message = Some(message.into());
        cx.notify();
    }

    #[cfg(test)]
    pub fn listed(&self) -> &[String] {
        &self.names
    }

    #[cfg(test)]
    pub fn selected_name(&self) -> Option<&String> {
        self.name()
    }

    #[cfg(test)]
    pub fn set_value_for_test(&self, ix: usize, value: &str, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else { return };
        let value = value.to_string();
        row.value.update(cx, |input, cx| {
            *input = TextInput::new(value, "value", "EnvField", cx);
        });
    }

    #[cfg(test)]
    pub fn variables(&self, cx: &App) -> Vec<(String, String, bool)> {
        self.rows
            .iter()
            .map(|row| {
                (
                    row.name.read(cx).text().to_string(),
                    row.value.read(cx).text().to_string(),
                    row.secret,
                )
            })
            .collect()
    }
}

impl Focusable for EnvironmentPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EnvironmentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // Scroll gates on hit-testing rather than propagation, so a scrim that only catches
            // clicks lets the wheel through to the panes behind it.
            .occlude()
            .child(
                div()
                    .track_focus(&self.focus_handle)
                    .key_context("EnvPanel")
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
                            .child("Environments"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .flex()
                            .flex_row()
                            .child(self.list(&theme, cx))
                            .child(self.table(&theme, cx)),
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

impl EnvironmentPanel {
    fn list(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let selected = self.selected;

        div()
            .id("env-list")
            .w(px(LIST_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .children(self.names.iter().enumerate().map(|(ix, name)| {
                let is_selected = ix == selected;
                let renaming = is_selected.then(|| self.renaming.clone()).flatten();

                div()
                    .id(("env-name", ix))
                    .debug_selector(move || format!("env-name-{ix}"))
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
                        Some(input) => div().w_full().overflow_hidden().child(input).into_any_element(),
                        None => div().child(name.clone()).into_any_element(),
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
            .child(
                div()
                    .px_1()
                    .py_1()
                    .child(crate::ui::text_action(
                        "env-new",
                        "New environment".into(),
                        "Create an environment",
                        EnvNewEnvironment,
                        theme,
                    )),
            )
    }

    fn table(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let selected = self.name().cloned().unwrap_or_default();
        // Globals is the always-active layer rather than a peer, so it is the one row with no
        // rename and no delete — and the buttons are simply absent rather than shown disabled,
        // since a control you cannot use teaches nothing.
        let editable = selected != environment::GLOBALS;

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
                            .child(selected),
                    )
                    .children(editable.then(|| {
                        crate::ui::icon_button(
                            "env-rename",
                            Icon::Pencil,
                            "Rename this environment",
                            EnvRenameEnvironment,
                            theme,
                        )
                    }))
                    .children(editable.then(|| {
                        crate::ui::icon_button(
                            "env-trash",
                            Icon::Trash,
                            "Move this environment to the trash",
                            EnvTrashEnvironment,
                            theme,
                        )
                    })),
            )
            .child(self.rows_area(theme, cx))
    }

    fn rows_area(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id("env-table")
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .children(
                self.rows
                    .iter()
                    .enumerate()
                    .map(|(ix, row)| self.variable_row(ix, row, theme, cx)),
            )
            .child(
                div()
                    .px_1()
                    .py_1()
                    .child(crate::ui::text_action(
                        "env-add-variable",
                        "Add variable".into(),
                        "Add a variable",
                        EnvNewVariable,
                        theme,
                    )),
            )
    }

    fn variable_row(
        &self,
        ix: usize,
        row: &VarRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (icon, hint) = if row.secret {
            (Icon::Lock, "Secret — kept in the gitignored file")
        } else {
            (Icon::LockOpen, "Committed with the collection")
        };

        div()
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .child(div().w(px(180.)).flex_none().overflow_hidden().child(row.name.clone()))
            .child(div().flex_1().min_w(px(0.)).overflow_hidden().child(row.value.clone()))
            .child(
                div()
                    .id(("env-secret", ix))
                    .debug_selector(move || format!("env-secret-{ix}"))
                    .group(crate::ui::ICON_GROUP)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.bg_hover))
                    .tooltip({
                        let hint = hint.to_string();
                        move |_, cx| crate::ui::Tooltip::text(hint.clone(), cx)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            panel.toggle_secret(ix, cx);
                        }),
                    )
                    .child(glyph(
                        icon,
                        if row.secret { theme.accent } else { theme.text_muted },
                        theme.text,
                        14.,
                    )),
            )
            .child(
                div()
                    .id(("env-remove", ix))
                    .debug_selector(move || format!("env-remove-{ix}"))
                    .group(crate::ui::ICON_GROUP)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.bg_hover))
                    .tooltip(move |_, cx| crate::ui::Tooltip::text("Remove this variable", cx))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            panel.remove_variable(ix, cx);
                        }),
                    )
                    .child(glyph(Icon::Close, theme.text_muted, theme.text, 12.)),
            )
    }
}
