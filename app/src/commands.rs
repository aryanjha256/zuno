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
        command("Import from an OpenAPI spec", ImportOpenApi),
        // Navigation.
        command("Find request", OpenRequest),
        command("Show or hide the collection panel", ToggleCollectionPanel),
        // Unlike the panel's other verbs this one needs no selection — with nothing selected it
        // makes a folder at the collection root — so it is a real palette row rather than an
        // exclusion.
        command("New folder in collection", NewFolder),
        command("Collapse all folders", CollectionCollapseAll),
        command("Expand all folders", CollectionExpandAll),
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
        command("Show request headers", ShowHeadersTab),
        command("Show request params", ShowParamsTab),
        command("Show request body", ShowBodyTab),
        command("Next request tab", NextRequestTab),
        command("Previous request tab", PrevRequestTab),
        command("Add multipart part", AddMultipartField),
        command("Attach a file to the body", ChooseBodyFile),
        // Moving around.
        command("Focus URL", FocusUrl),
        command("Focus body", FocusBody),
        command("Focus response", FocusResponse),
        // The response viewer.
        command("Copy response body", CopyResponse),
        command("Save response body to a file", SaveResponse),
        command("Copy request as a curl command", CopyAsCurl),
        command("Show response history", ShowHistory),
        command("Switch between response body and headers", ToggleResponseView),
        command("Find in response", FindInResponse),
        command("Find and replace in request body", FindInBody),
        command("Fold all", FoldAll),
        command("Unfold all", UnfoldAll),
        command("Copy selected row's value", CopyRowValue),
        command("Copy selected row's path", CopyRowPath),
        command("Fold or unfold the selected row", ToggleFold),
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
    // The application menu is a *discovery* surface for people who don't know the palette
    // exists. Reaching it from the palette is the wrong way round, and everything inside it is
    // either offered here already or a link.
    ("zuno::OpenAppMenu", "a menu reached from the palette is backwards"),
    // Only valid inside the settings panel, where they're already bound.
    ("zuno::SettingNext", "only valid inside the settings panel"),
    ("zuno::SettingPrev", "only valid inside the settings panel"),
    ("zuno::SettingIncrease", "only valid inside the settings panel"),
    ("zuno::SettingDecrease", "only valid inside the settings panel"),
    ("zuno::SettingConfirm", "only valid inside the settings panel"),
    ("zuno::SettingsDismiss", "only valid inside the settings panel"),
    // Only valid while the unsaved-changes prompt is open. `CloseTab` is the entry point and it
    // *is* offered; a row for "discard" would be a destructive command reachable on its own.
    ("zuno::ConfirmClose", "only valid inside the unsaved-changes prompt"),
    ("zuno::CancelClose", "only valid inside the unsaved-changes prompt"),
    ("zuno::CloseChoiceNext", "only valid inside the unsaved-changes prompt"),
    ("zuno::CloseChoicePrev", "only valid inside the unsaved-changes prompt"),
    // Only valid while the find bar is open, where they're already bound. "Find in response"
    // is the entry point and it *is* offered; a palette row for "next match" would be a
    // command you can only reach by first running another one.
    ("zuno::FindNext", "only valid inside the find bar"),
    ("zuno::FindPrev", "only valid inside the find bar"),
    ("zuno::CloseFind", "only valid inside the find bar"),
    // Same reasoning for the body's bar: "Find and replace in request body" is the entry point
    // and it *is* offered, so a row for "next match" would be a command reachable only by first
    // running another one. Replace is excluded on the same grounds — it needs a match to act on.
    ("zuno::BodyFindNext", "only valid inside the body find bar"),
    ("zuno::BodyFindPrev", "only valid inside the body find bar"),
    ("zuno::CloseBodyFind", "only valid inside the body find bar"),
    ("zuno::ReplaceNext", "only valid inside the body find bar"),
    ("zuno::ReplaceAll", "only valid inside the body find bar"),
    // Placed by a right-click or by `delete` on a selected row, so it has no meaning without
    // one — there is nowhere to put it and nothing for it to act on.
    ("zuno::OpenCollectionMenu", "needs a selected row in the collection panel"),
    // Deliberately **not** offered. It only asks, but a palette row reading "Delete request"
    // acts on whatever the panel happens to have selected, which the palette does not show you
    // — a destructive verb aimed at a target off screen.
    ("zuno::DeleteRequest", "acts on the panel's selection, which the palette cannot show"),
    // The half that actually removes a file. Reachable only by choosing it in the confirmation
    // menu, which is the entire point: a palette row would be the one-keystroke delete the
    // confirmation exists to prevent.
    ("zuno::ConfirmDeleteRequest", "only valid inside the delete confirmation"),
    // The rest of the panel's row verbs, all for `DeleteRequest`'s reason: they act on whatever
    // the collection panel has selected, and the palette covers that selection while asking you
    // to choose. A palette row aimed at a target you cannot see is worse than no row.
    ("zuno::TrashRequest", "acts on the panel's selection, which the palette cannot show"),
    ("zuno::DuplicateRequest", "acts on the panel's selection, which the palette cannot show"),
    ("zuno::RevealRequest", "acts on the panel's selection, which the palette cannot show"),
    (
        "zuno::OpenRequestExternally",
        "acts on the panel's selection, which the palette cannot show",
    ),
    ("zuno::CopyRequestPath", "acts on the panel's selection, which the palette cannot show"),
    (
        "zuno::CopyRequestRelativePath",
        "acts on the panel's selection, which the palette cannot show",
    ),
    ("zuno::RenameRequest", "acts on the panel's selection, which the palette cannot show"),
    ("zuno::MoveRequest", "acts on the panel's selection, which the palette cannot show"),
    // The two halves of an open rename box. Neither means anything without one on screen.
    ("zuno::CommitRename", "only valid while renaming a row"),
    // The two halves of the import modal, meaningless without one on screen.
    ("zuno::ImportConfirm", "only valid inside the import dialog"),
    ("zuno::ImportDismiss", "only valid inside the import dialog"),
    ("zuno::CancelRename", "only valid while renaming a row"),
    // Only valid while the panel has focus, where they're already bound. "Show or hide the
    // collection panel" is the entry point and it *is* offered; a row for "next row in the
    // tree" would be a command reachable only by first running another one — the same
    // reasoning as the find bars and the settings panel above.
    ("zuno::CollectionNext", "only valid inside the collection panel"),
    ("zuno::CollectionPrev", "only valid inside the collection panel"),
    ("zuno::CollectionConfirm", "only valid inside the collection panel"),
    ("zuno::CollectionCollapse", "only valid inside the collection panel"),
    ("zuno::CollectionExpand", "only valid inside the collection panel"),
    // The menu is placed by a right-click, so it has no meaning without one — there is nowhere
    // to put it. Everything inside it is offered directly above, which is the point: the menu is
    // the *mouse* path to verbs the keyboard already reaches.
    ("zuno::OpenRowMenu", "needs a click position; its items are all offered directly"),
    ("zuno::MenuNext", "only valid inside the row menu"),
    ("zuno::MenuPrev", "only valid inside the row menu"),
    ("zuno::MenuConfirm", "only valid inside the row menu"),
    ("zuno::MenuDismiss", "only valid inside the row menu"),
    // Stepping the selection through the response body. Same reasoning as the find bar's
    // next/previous: the entry point is offered ("Focus response"), and a row-at-a-time cursor
    // move run from a palette you had to open first is not a command anyone wants. The two
    // verbs that act *on* the selection — copy value, copy path — are offered.
    ("zuno::ResponseRowNext", "cursor movement; the arrow keys are the interface"),
    ("zuno::ResponseRowPrev", "cursor movement; the arrow keys are the interface"),
    ("zuno::ScrollLeft", "view movement; the arrow keys are the interface"),
    ("zuno::ScrollRight", "view movement; the arrow keys are the interface"),
    ("zuno::ScrollStart", "view movement; the arrow keys are the interface"),
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
