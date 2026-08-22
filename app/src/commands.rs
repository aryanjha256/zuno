//! The command palette's contents: every verb worth offering by name, with a label.
//!
//! **Why this table exists at all.** `ROADMAP.md` once claimed `actions.rs` was already
//! most of what a palette needs. It isn't. The runtime list is `cx.all_action_names()`,
//! which returns namespaced strings for *every* registered action — including the twenty-odd
//! `text_input::` and `editor::` ones that must never appear in a palette — and it carries
//! no human labels. "Backspace" and "SelectLeft" are not commands; they're keystrokes.
//!
//! **Actions, not name strings.** Each entry holds a real action value, so renaming
//! `SendRequest` is a compile error here rather than a palette row that silently stops
//! working. That's the whole reason this is a function returning values instead of a
//! `const` array of `&str`.
//!
//! Adding an action to `actions.rs` and forgetting this file is caught by
//! `every_action_is_either_in_the_palette_or_deliberately_excluded` — the palette can't
//! quietly fall behind.

use gpui::Action;

use crate::actions::*;

/// One palette row.
pub struct Command {
    /// Sentence case, imperative, and phrased as the thing it does rather than as the
    /// action's type name: "Save request to collection", not "SaveRequest".
    pub label: &'static str,
    pub action: Box<dyn Action>,
}

fn command(label: &'static str, action: impl Action) -> Command {
    Command {
        label,
        action: action.boxed_clone(),
    }
}

/// Every command the palette offers, in the order it offers them.
///
/// Ordered by how often you'd reach for it, not alphabetically — the fuzzy filter handles
/// finding things by name, so the unfiltered list should read like a list of what Zuno
/// does. `fuzzy::rank` is stable, so this order survives ties.
pub fn palette() -> Vec<Command> {
    vec![
        // The loop.
        command("Send request", SendRequest),
        command("Cancel request", CancelRequest),
        command("Save request to collection", SaveRequest),
        command("Import request from curl on the clipboard", ImportCurl),
        // Navigation.
        command("Find request", OpenRequest),
        command("Switch environment", SwitchEnvironment),
        command("New tab", NewTab),
        command("Close tab", CloseTab),
        command("Next tab", NextTab),
        command("Previous tab", PrevTab),
        // Editing the request.
        command("Add header", AddHeader),
        command("Add query parameter", AddQuery),
        command("Toggle focused row", ToggleRow),
        command("Remove focused row", RemoveRow),
        command("Change method", OpenMethod),
        command("Change body type", OpenBodyType),
        command("Add form field", AddFormField),
        command("Add multipart part", AddMultipartField),
        command("Attach a file to the body", ChooseBodyFile),
        // Moving around.
        command("Focus URL", FocusUrl),
        command("Focus body", FocusBody),
        command("Focus response", FocusResponse),
        // The response viewer.
        command("Copy response body", CopyResponse),
        command("Save response body to a file", SaveResponse),
        command("Show response history", ShowHistory),
        command("Switch between response body and headers", ToggleResponseView),
        command("Fold all", FoldAll),
        command("Unfold all", UnfoldAll),
        // Settings.
        command("Request settings", OpenSettings),
        command("Clear stored cookies", ClearCookies),
        // Application.
        command("Toggle theme", ToggleTheme),
        command("Quit", Quit),
    ]
}

/// Actions deliberately kept out of the palette, and why.
///
/// Listing them explicitly rather than filtering by a naming convention is what lets the
/// drift test be strict: a new action is either a command or a considered exclusion, never
/// an oversight.
#[cfg(test)]
const EXCLUDED: &[(&str, &str)] = &[
    // Meaningless outside an open picker, where they're already bound.
    ("zuno::PickerNext", "only valid inside the picker"),
    ("zuno::PickerPrev", "only valid inside the picker"),
    ("zuno::PickerConfirm", "only valid inside the picker"),
    ("zuno::PickerDismiss", "only valid inside the picker"),
    // A palette that lists itself is noise; Ctrl+K is how you got here.
    ("zuno::OpenPalette", "this is the palette"),
    // Only valid inside the settings panel, where they're already bound.
    ("zuno::SettingNext", "only valid inside the settings panel"),
    ("zuno::SettingPrev", "only valid inside the settings panel"),
    ("zuno::SettingIncrease", "only valid inside the settings panel"),
    ("zuno::SettingDecrease", "only valid inside the settings panel"),
    ("zuno::SettingConfirm", "only valid inside the settings panel"),
    ("zuno::SettingsDismiss", "only valid inside the settings panel"),
    // Tab/Shift-Tab move focus within a buffer. As a named command it reads as "go
    // somewhere" without saying where, which is worse than not offering it.
    ("zuno::FocusNext", "keystroke-only; no meaningful name"),
    ("zuno::FocusPrev", "keystroke-only; no meaningful name"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn labels_are_unique_and_readable() {
        let mut seen = BTreeSet::new();
        for command in palette() {
            assert!(
                seen.insert(command.label),
                "duplicate label {:?} — two rows would be indistinguishable",
                command.label
            );
            // A label that is just the type name means someone added a row without
            // writing a label for it.
            assert!(
                command.label.contains(' ') || command.label == "Quit",
                "{:?} reads like a type name, not a command",
                command.label
            );
            assert!(
                !command.label.ends_with('.'),
                "{:?} should not end in a period",
                command.label
            );
        }
    }

    /// The test that keeps this file honest.
    ///
    /// Every `zuno::` action must be either offered in the palette or explicitly excluded.
    /// Without this, adding an action to `actions.rs` silently leaves it unreachable by
    /// name, which is the exact failure mode a palette exists to prevent.
    #[gpui::test]
    fn every_action_is_either_in_the_palette_or_deliberately_excluded(cx: &mut gpui::TestAppContext) {
        // Registration is what populates `all_action_names`, so the real keymap has to be
        // installed first.
        let (registered, offered) = cx.update(|cx| {
            crate::register_keymap(cx);
            let registered: BTreeSet<&str> = cx
                .all_action_names()
                .iter()
                .copied()
                // Text editing lives in its own namespaces and is keystroke-only.
                .filter(|name| name.starts_with("zuno::"))
                .collect();
            let offered: BTreeSet<&str> =
                palette().iter().map(|command| command.action.name()).collect();
            (registered, offered)
        });

        let excluded: BTreeSet<&str> = EXCLUDED.iter().map(|(name, _)| *name).collect();

        let missing: Vec<&str> = registered
            .difference(&offered)
            .filter(|name| !excluded.contains(*name))
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "these actions are neither in the palette nor excluded: {missing:?}\n\
             Add them to `palette()`, or to `EXCLUDED` with a reason."
        );

        // The reverse: an exclusion for an action that no longer exists is dead weight,
        // and a palette row for one is a row that can never dispatch.
        let stale: Vec<&str> = excluded.difference(&registered).copied().collect();
        assert!(stale.is_empty(), "EXCLUDED names actions that don't exist: {stale:?}");

        let unregistered: Vec<&str> = offered.difference(&registered).copied().collect();
        assert!(
            unregistered.is_empty(),
            "palette offers unregistered actions: {unregistered:?}"
        );
    }
}
