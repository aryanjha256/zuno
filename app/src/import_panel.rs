//! The import modal: one field, taking a URL or a file path.
//!
//! **A modal rather than another strip of chrome.** Rename got away with an inline box because a
//! tree row *is* a text field's worth of space; an import needs a field, a hint, and somewhere to
//! report what happened, and there is nowhere in the panes to put that without the clutter this
//! was built to avoid.
//!
//! **Concrete, not a form framework.** One consumer, so it is one file — the bet `picker.rs`
//! made and won, reaching eight consumers without a rewrite because it stayed a `Vec<Item>` and a
//! `Target` rather than becoming a trait. When a second modal wants a text field (an environment
//! editor, opening a project), that is the moment to lift the shared part out, and not before.
//!
//! **One field for both sources, deliberately.** A URL/file radio pair would be a mode to choose
//! before typing, to describe a difference the text itself already carries: `http` at the front
//! or not. Paste a link or a path and press Enter.
//!
//! **And no format picker either, for the stronger version of the same reason.** Which format a
//! document is written in is a property of the document, and `import::parse` reads it — asking
//! someone to classify their own export before they can use it is the friction importing exists
//! to remove.

use gpui::{
    AppContext, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    Styled, Window, div, px,
};

use crate::input::TextInput;
use crate::theme::ActiveTheme;

/// Wide enough for a real spec URL without wrapping, and narrow enough to read as a dialog.
const WIDTH: f32 = 520.;

pub enum ImportEvent {
    Dismissed,
    /// The source to read, exactly as typed. Deciding whether it is a URL or a path belongs to
    /// the workspace, which is what owns the engine and the filesystem.
    Confirmed(String),
}

impl EventEmitter<ImportEvent> for ImportPanel {}

pub struct ImportPanel {
    focus_handle: FocusHandle,
    pub source: Entity<TextInput>,
    /// What happened, once something has. Holds the parse error or the skipped-operations
    /// notice, because both are answers to "why did I get fewer requests than I expected" and
    /// the status bar is cleared by the next thing that touches it.
    message: Option<SharedString>,
    /// True while a fetch is in flight, so Enter cannot start a second one.
    pub busy: bool,
    /// Where focus was, so dismissing puts it back — the same guard the picker and the settings
    /// panel each carry, for the same reason.
    restore_focus: Option<FocusHandle>,
}

impl ImportPanel {
    pub fn new(restore_focus: Option<FocusHandle>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let source = cx.new(|cx| {
            TextInput::new(
                "",
                "A path to a Postman export or OpenAPI spec, or a URL to fetch one from",
                "ImportSource",
                cx,
            )
        });
        let handle = source.read(cx).focus_handle(cx);
        window.focus(&handle);

        Self {
            focus_handle: cx.focus_handle(),
            source,
            message: None,
            busy: false,
            restore_focus,
        }
    }

    pub fn typed(&self, cx: &gpui::App) -> String {
        self.source.read(cx).text().to_string()
    }

    /// Report without closing. An import that failed leaves the modal open with what you typed
    /// still in it, because the fix is usually a character or two.
    pub fn report(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.message = Some(message.into());
        self.busy = false;
        cx.notify();
    }

    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.restore_focus.take() {
            window.focus(&handle);
        }
        cx.emit(ImportEvent::Dismissed);
    }

    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let typed = self.typed(cx);
        if typed.trim().is_empty() {
            self.report("Paste a spec URL, or the path to a .json file", cx);
            return;
        }
        self.busy = true;
        cx.notify();
        cx.emit(ImportEvent::Confirmed(typed));
    }
}

/// Fill the source from the file dialog.
///
/// Select-all then replace rather than a `set_text` API, the same route `WorkspacePanel`
/// takes: `replace_text_in_range` with a `None` range acts on the selection, so the write
/// keeps the sanitisation, the undo entry and the `Changed` emit every other edit path gets.
/// Typing over it afterwards is the point — a browsed path is a starting value, not a mode.
impl ImportPanel {
    #[cfg(test)]
    pub fn source_text(&self, cx: &gpui::App) -> String {
        self.source.read(cx).text().to_string()
    }

    pub fn set_source(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.source.update(cx, |input, cx| {
            input.select_all_text(cx);
            input.replace_text_in_range(None, &path, window, cx);
        });
        cx.notify();
    }
}

impl Focusable for ImportPanel {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImportPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // A modal owns the mouse, not just the keyboard: without this the wheel reaches the
            // response body behind it, because scroll gates on hit-testing rather than on
            // propagation. See `picker.rs`.
            .occlude()
            // The same scrim value the picker uses. Not a theme token, because it is the
            // absence of the app rather than a colour in it — and one hand-written literal in
            // two places is cheaper than a token nobody else would read.
            .bg(gpui::hsla(0., 0., 0., 0.4))
            .track_focus(&self.focus_handle)
            .key_context("ImportPanel")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|panel, _: &MouseDownEvent, window, cx| panel.dismiss(window, cx)),
            )
            .child(
                div()
                    .id("import-panel")
                    .debug_selector(|| "import-panel".to_string())
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(WIDTH))
                    .p_4()
                    .rounded_sm()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border)
                    // Or clicking inside the dialog dismisses it, through the scrim behind.
                    .on_mouse_down(
                        MouseButton::Left,
                        |_: &MouseDownEvent, _, cx| cx.stop_propagation(),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text)
                            .child("Import from OpenAPI"),
                    )
                    // Field and browse button on one row, the shape `WorkspacePanel` already
                    // uses for its location: the path is editable either way, and the dialog is
                    // a faster way to fill it rather than a different way to answer.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(crate::ui::field_box(self.source.clone(), &theme)),
                            )
                            .child(crate::ui::icon_button(
                                "import-browse",
                                crate::ui::Icon::File,
                                "Choose a file",
                                crate::actions::ImportBrowse,
                                &theme,
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_faint)
                            .child(if self.busy {
                                SharedString::from("Reading…")
                            } else {
                                SharedString::from(
                                    "Reads OpenAPI 3.x in JSON. Enter to import, Escape to close.",
                                )
                            }),
                    )
                    .children(self.message.clone().map(|message| {
                        div()
                            .text_xs()
                            // `text_muted`, never `border`: in the dark theme `border` equals
                            // `bg_hover` and this line would vanish on an elevated surface.
                            .text_color(theme.text_muted)
                            .child(message)
                    })),
            )
    }
}
