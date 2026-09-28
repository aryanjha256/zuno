//! Keybindings as each platform expects them.
//!
//! **Every binding is written once, in its Linux and Windows spelling, and translated here for
//! macOS** — rather than a second keymap kept in step by hand, or 177 `#[cfg]` pairs. Off macOS
//! this returns its input untouched, so the Linux keymap is byte-identical to what it was.
//!
//! On macOS, by keystroke:
//!
//! - **`ctrl-` becomes `cmd-`**: `ctrl-enter` is `⌘↩`, `ctrl-shift-h` is `⌘⇧H`.
//! - **`alt-` + a letter becomes `ctrl-`**: `⌥` *types* on a Mac — `⌥E` is the accent key — so
//!   `alt-e` there would unfold the response when someone meant to write `é`. Ctrl is free once
//!   the app's shortcuts are on `⌘`. **`alt-` + an arrow stays `⌥`**, because `⌥↑` types nothing
//!   and `⌃↑` is Mission Control.
//! - **Text editing follows the Mac, not a rule**: words move on `⌥←`/`⌥→` (`⌃←` switches desktops),
//!   delete on `⌥⌫`/`⌥⌦`, the document ends on `⌘↑`/`⌘↓`, and redo is only `⌘⇧Z`.
//! - **Three exceptions**, each a collision with the system or with another binding: history is
//!   `⌘Y` as in Safari (`⌘H` hides the app), the method picker is `⌃M` (`⌘M` minimizes, and `⌘⇧M`
//!   is already *Add multipart field*), and tab switching stays `⌃Tab` (`⌘Tab` is the app
//!   switcher, and Mac browsers use `⌃Tab` too).
//!
//! `mac` is a parameter rather than a `cfg!`, so a test on any host can translate the whole keymap
//! and check it for collisions — `a_mac_keymap_has_no_collisions`, which is how `⌘⇧M` was caught.

/// One binding's keystrokes — possibly a sequence, space-separated — for the platform.
pub fn for_platform(keys: &str, mac: bool) -> String {
    if !mac {
        return keys.to_string();
    }
    keys.split(' ')
        .map(mac_keystroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn mac_keystroke(keystroke: &str) -> String {
    // The exceptions first, by exact keystroke.
    let exact = match keystroke {
        "ctrl-h" => Some("cmd-y"),
        "ctrl-m" => Some("ctrl-m"),
        "ctrl-tab" => Some("ctrl-tab"),
        "ctrl-shift-tab" => Some("ctrl-shift-tab"),
        "ctrl-left" => Some("alt-left"),
        "ctrl-right" => Some("alt-right"),
        "ctrl-shift-left" => Some("alt-shift-left"),
        "ctrl-shift-right" => Some("alt-shift-right"),
        "ctrl-backspace" => Some("alt-backspace"),
        "ctrl-delete" => Some("alt-delete"),
        "ctrl-home" => Some("cmd-up"),
        "ctrl-end" => Some("cmd-down"),
        "ctrl-shift-home" => Some("cmd-shift-up"),
        "ctrl-shift-end" => Some("cmd-shift-down"),
        // Redo's second key. `⌘Y` is history on a Mac, and `⌘⇧Z` is already redo.
        "ctrl-y" => Some("cmd-shift-z"),
        _ => None,
    };
    if let Some(mac) = exact {
        return mac.to_string();
    }

    if let Some(rest) = keystroke.strip_prefix("ctrl-") {
        return format!("cmd-{rest}");
    }
    if let Some(rest) = keystroke.strip_prefix("alt-") {
        // The key is whatever follows the last modifier.
        let key = rest.rsplit('-').next().unwrap_or(rest);
        if matches!(key, "up" | "down" | "left" | "right") {
            return keystroke.to_string();
        }
        return format!("ctrl-{rest}");
    }
    keystroke.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_a_mac_every_binding_is_left_exactly_as_written() {
        for keys in ["ctrl-enter", "alt-e", "ctrl-left", "ctrl-h", "escape", "ctrl-k ctrl-s"] {
            assert_eq!(for_platform(keys, false), keys);
        }
    }

    #[test]
    fn on_a_mac_the_rules_and_exceptions_apply() {
        let cases = [
            // App shortcuts move to ⌘, sequences keystroke by keystroke.
            ("ctrl-enter", "cmd-enter"),
            ("ctrl-shift-h", "cmd-shift-h"),
            ("ctrl-alt-e", "cmd-alt-e"),
            ("ctrl-k ctrl-s", "cmd-k cmd-s"),
            // ⌥ + a letter types on a Mac; ⌥ + an arrow does not, and ⌃↑ is Mission Control.
            ("alt-e", "ctrl-e"),
            ("alt-shift-r", "ctrl-shift-r"),
            ("alt-up", "alt-up"),
            ("alt-down", "alt-down"),
            // Text editing.
            ("ctrl-left", "alt-left"),
            ("ctrl-shift-right", "alt-shift-right"),
            ("ctrl-backspace", "alt-backspace"),
            ("ctrl-home", "cmd-up"),
            ("ctrl-shift-end", "cmd-shift-down"),
            ("ctrl-y", "cmd-shift-z"),
            // The three collisions, resolved as decided.
            ("ctrl-h", "cmd-y"),
            ("ctrl-m", "ctrl-m"),
            ("ctrl-tab", "ctrl-tab"),
            // Plain keys never change.
            ("escape", "escape"),
            ("shift-tab", "shift-tab"),
            ("f2", "f2"),
        ];
        for (linux, mac) in cases {
            assert_eq!(for_platform(linux, true), mac, "{linux}");
        }
    }
}
