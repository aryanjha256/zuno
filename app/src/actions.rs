//! Every keyboard-reachable verb in Zuno. See architecture.md §5.
//!
//! Actions are declared centrally so the keymap in `main.rs` is the single place
//! you look to answer "what does this key do". Dispatch travels up the focus
//! tree, so an action fires on the nearest ancestor element that handles it.
//!
//! Note the name `SendRequest` rather than `Send`: an action named `Send` would
//! shadow `std::marker::Send` at every `use` site, which breaks generic bounds
//! in confusing ways.

use gpui::actions;

actions!(
    zuno,
    [
        // Focus movement
        FocusUrl,
        FocusBody,
        FocusResponse,
        FocusNext,
        FocusPrev,
        // Request lifecycle
        SendRequest,
        // Application
        ToggleTheme,
        Quit,
    ]
);
