//! The keyboard shortcuts sheet: every global shortcut, grouped, with a search.
//!
//! **The keys come from the live keymap; only the groups and labels are written here.** A binding
//! that changes in `register_keymap` changes on the sheet with it, the way tooltips and the palette
//! already read theirs. What the keymap cannot say is which actions belong together or what to call
//! them, so that is this file's job, and `every_global_shortcut_is_on_the_sheet` makes sure a new
//! global binding is not left off it.
//!
//! **The ~100 bindings that only work inside one panel or list are not listed one by one.** They are
//! the same few keys every time (arrows move, Enter confirms, Escape closes), so one group states
//! the pattern instead of burying the useful rows under repeats of it.

use gpui::prelude::*;
use gpui::{
    div, px, Action, Entity, FocusHandle, Focusable, MouseButton, MouseDownEvent, Subscription,
    Window,
};

use crate::actions::*;
use crate::input::text_input::{Changed, TextInput};
use crate::theme::Theme;
use crate::ui::{glyph, keycaps, Icon};
use crate::workspace::Workspace;

const WIDTH: f32 = 940.;
const BODY_HEIGHT: f32 = 560.;

/// Where a row's keys come from.
enum Keys {
    /// A global binding, read from the keymap.
    Global(Box<dyn Action>),
    /// A binding scoped to the response pane, read as reached from it.
    InResponse(Box<dyn Action>),
    /// The shared panel-and-list pattern, which no single binding stands for.
    Pattern(&'static [&'static str]),
}

struct Row {
    label: &'static str,
    keys: Keys,
}

struct Group {
    title: &'static str,
    rows: Vec<Row>,
}

fn global(label: &'static str, action: impl Action) -> Row {
    Row { label, keys: Keys::Global(Box::new(action)) }
}

fn in_response(label: &'static str, action: impl Action) -> Row {
    Row { label, keys: Keys::InResponse(Box::new(action)) }
}

fn pattern(label: &'static str, keys: &'static [&'static str]) -> Row {
    Row { label, keys: Keys::Pattern(keys) }
}

/// The sheet's three columns, in reading order.
fn columns() -> [Vec<Group>; 3] {
    [
        vec![
            Group {
                title: "Essentials",
                rows: vec![
                    global("Send request", SendRequest),
                    global("Cancel request", CancelRequest),
                    global("Save request", SaveRequest),
                    global("Find request", OpenRequest),
                    global("Command palette", OpenPalette),
                    global("Switch environment", SwitchEnvironment),
                    global("Keyboard shortcuts", ShowShortcuts),
                ],
            },
            Group {
                title: "Tabs",
                rows: vec![
                    global("New tab", NewTab),
                    global("Close tab", CloseTab),
                    global("Next tab", NextTab),
                    global("Previous tab", PrevTab),
                ],
            },
            Group {
                title: "Focus",
                rows: vec![
                    global("URL bar", FocusUrl),
                    global("Request body", FocusBody),
                    global("Response", FocusResponse),
                    global("Show or hide collections", ToggleCollectionPanel),
                    global("Next field", FocusNext),
                    global("Previous field", FocusPrev),
                ],
            },
        ],
        vec![
            Group {
                title: "Request",
                rows: vec![
                    global("Choose method", OpenMethod),
                    global("Add header", AddHeader),
                    global("Add query parameter", AddQuery),
                    global("Add form field", AddFormField),
                    global("Add multipart field", AddMultipartField),
                    global("Enable or disable row", ToggleRow),
                    global("Remove row", RemoveRow),
                    global("Body type", OpenBodyType),
                    global("Choose body file", ChooseBodyFile),
                    global("Format body", FormatBody),
                    global("Minify body", MinifyBody),
                    global("Next request tab", NextRequestTab),
                    global("Previous request tab", PrevRequestTab),
                ],
            },
            Group {
                title: "Import and share",
                rows: vec![
                    global("Import from curl", ImportCurl),
                    global("Import collection or spec", ImportDocument),
                    global("Copy as code", CopyAsCode),
                ],
            },
            Group {
                title: "In lists and panels",
                rows: vec![
                    pattern("Move", &["up", "down"]),
                    pattern("Open or confirm", &["enter"]),
                    pattern("Close", &["escape"]),
                    pattern("Remove", &["delete"]),
                    pattern("Rename", &["f2"]),
                ],
            },
        ],
        vec![
            Group {
                title: "Response",
                rows: vec![
                    global("Find in response", FindInResponse),
                    global("Copy response", CopyResponse),
                    global("Save response to file", SaveResponse),
                    global("Fold all", FoldAll),
                    global("Unfold all", UnfoldAll),
                    global("Next response tab", NextResponseTab),
                    global("Previous response tab", PrevResponseTab),
                    in_response("Copy row value", CopyRowValue),
                    in_response("Copy row path", CopyRowPath),
                ],
            },
            Group {
                title: "Workspace",
                rows: vec![
                    global("History of this tab", ShowHistory),
                    global("All history", ShowAllHistory),
                    global("Git", ShowGit),
                    global("Run folder", RunFolder),
                    global("Run flow", RunFlow),
                    global("Edit environments", EditEnvironments),
                    global("Request settings", OpenSettings),
                    global("Default request settings", OpenDefaults),
                    global("Toggle theme", ToggleTheme),
                    global("App menu", OpenAppMenu),
                    global("Quit", Quit),
                ],
            },
        ],
    ]
}

/// Every action the sheet reads from the global keymap, for the drift test.
#[cfg(test)]
pub fn listed_global_actions() -> Vec<&'static str> {
    columns()
        .into_iter()
        .flatten()
        .flat_map(|group| group.rows)
        .filter_map(|row| match row.keys {
            Keys::Global(action) => Some(action.name()),
            _ => None,
        })
        .collect()
}

pub struct ShortcutsSheet {
    pub search: Entity<TextInput>,
    pub restore_focus: Option<FocusHandle>,
    _search_changed: Subscription,
}

impl ShortcutsSheet {
    pub fn new(restore_focus: Option<FocusHandle>, cx: &mut Context<Workspace>) -> Self {
        let search = cx.new(|cx| TextInput::new("", "Search by action or key", "Shortcuts", cx));
        // Held: dropping a `Subscription` unsubscribes.
        let _search_changed = cx.subscribe(&search, |_, _, _: &Changed, cx| cx.notify());
        Self { search, restore_focus, _search_changed }
    }

    pub fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }
}

/// A row as shown: its label and its keys spelled for this platform.
struct Shown {
    label: &'static str,
    keys: String,
}

/// Whether every word of the query is in the row's label or keys.
fn matches(query: &str, row: &Shown) -> bool {
    let haystack = format!("{} {}", row.label, row.keys).to_lowercase();
    query.split_whitespace().all(|word| haystack.contains(&word.to_lowercase()))
}

fn spelled(keys: &Keys, response: Option<&FocusHandle>, window: &Window) -> String {
    match keys {
        Keys::Global(action) => crate::workspace::keybinding_label(action.as_ref(), window),
        Keys::InResponse(action) => match response {
            Some(focus) => crate::workspace::keybinding_label_in(action.as_ref(), focus, window),
            None => String::new(),
        },
        Keys::Pattern(keys) => keys
            .iter()
            .map(|key| {
                crate::workspace::spell_keystroke(
                    &gpui::Modifiers::default(),
                    key,
                    cfg!(target_os = "macos"),
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub fn render(
    sheet: &ShortcutsSheet,
    response: Option<&FocusHandle>,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let query = sheet.search.read(cx).text().to_string();

    let columns = columns().map(|groups| {
        groups
            .into_iter()
            .filter_map(|group| {
                let rows: Vec<Shown> = group
                    .rows
                    .iter()
                    .map(|row| Shown { label: row.label, keys: spelled(&row.keys, response, window) })
                    // A binding removed from the keymap leaves nothing to teach; drop the row.
                    .filter(|row| !row.keys.is_empty())
                    .filter(|row| matches(&query, row))
                    .collect();
                (!rows.is_empty()).then_some((group.title, rows))
            })
            .collect::<Vec<_>>()
    });
    let empty = columns.iter().all(Vec::is_empty);

    let row = |shown: Shown| {
        div()
            .h(px(24.))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(theme.text)
                    .child(shown.label),
            )
            .child(keycaps(&shown.keys, theme))
    };
    let group = |title: &'static str, rows: Vec<Shown>| {
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .pt_2()
                    .pb_1()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(title.to_uppercase()),
            )
            .children(rows.into_iter().map(row))
    };
    let column_count = columns.len();
    let body = div()
        .id("shortcuts-body")
        .max_h(px(BODY_HEIGHT))
        .overflow_y_scroll()
        .flex()
        .flex_row()
        .when(empty, |body| {
            body.child(
                div()
                    .w_full()
                    .py_6()
                    .flex()
                    .justify_center()
                    .text_color(theme.text_faint)
                    .child("No shortcut matches"),
            )
        })
        .when(!empty, |body| {
            body.children(columns.into_iter().enumerate().map(|(ix, groups)| {
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .px_4()
                    .pb_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(ix + 1 < column_count, |column| {
                        column.border_r_1().border_color(theme.border)
                    })
                    .children(groups.into_iter().map(|(title, rows)| group(title, rows)))
            }))
        });

    let opener = crate::workspace::keybinding_label(&ShowShortcuts, window);
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child("Keyboard shortcuts"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(glyph(Icon::Search, theme.text_faint, theme.text_faint, 13.))
                .child(crate::ui::field_box(sheet.search.clone(), theme)),
        )
        .child(
            div()
                .id("shortcuts-close")
                .debug_selector(|| "shortcuts-close".to_string())
                .flex_none()
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_pointer()
                .group(crate::ui::ICON_GROUP)
                .hover(|style| style.bg(theme.bg_hover))
                .active(|style| style.bg(theme.bg))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.close_shortcuts(window, cx);
                    }),
                )
                .child(glyph(Icon::Close, theme.text_faint, theme.text, 13.)),
        );

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_4()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
        .text_size(px(11.))
        .text_color(theme.text_muted)
        .when(!opener.is_empty(), |footer| {
            footer.child("Open this anytime with").child(keycaps(&opener, theme))
        })
        .child(div().flex_1())
        .child("Keys in a panel or list work while it has focus");

    div()
        .id("shortcuts-scrim")
        // A scrim that catches clicks does not stop the wheel: scroll handlers gate on the hit
        // test, not on propagation. Every overlay in this app needs this.
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .justify_center()
        .items_start()
        .pt(px(64.))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| this.close_shortcuts(window, cx)),
        )
        .child(
            div()
                .id("shortcuts-sheet")
                .debug_selector(|| "shortcuts-sheet".to_string())
                .w(px(WIDTH))
                .max_w(gpui::relative(0.94))
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg_elevated)
                .shadow_lg()
                .text_xs()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(header)
                .child(body)
                .child(footer),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_every_word_against_label_and_keys() {
        let row = Shown { label: "Add header", keys: "Ctrl+Shift+H".to_string() };
        assert!(matches("", &row));
        assert!(matches("header add", &row));
        assert!(matches("shift+h", &row));
        assert!(!matches("header query", &row));
    }
}
