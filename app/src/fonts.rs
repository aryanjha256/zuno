//! The two typefaces Zuno draws with, shipped inside the binary.
//!
//! **Bundled, so Zuno looks the same everywhere.** Left to the system, the interface font was
//! gpui's `.SystemUIFont`: San Francisco on a Mac, Segoe UI on Windows, and on Linux a request
//! for IBM Plex Sans that most machines cannot meet, so it fell back to whatever was installed.
//! The code font was the first of a list that happened to be installed. Now both are these files.
//!
//! **Changing the font is this file and `assets/fonts/`.** Swap the files, change the two names
//! below, and nothing else needs to know. IBM Plex Sans 1.1.0 and JetBrains Mono 2.304, from
//! their own releases, each under the SIL Open Font License (`assets/fonts/OFL-*.txt`).
//!
//! JetBrains Mono over Plex Mono, its natural partner, because it reads better in a body of JSON;
//! its taller lowercase only shows where the two share a line, which is mostly the URL row.

use std::borrow::Cow;

use gpui::App;

/// The interface: labels, tabs, menus, buttons. Set once at the window's root.
pub const UI: &str = "IBM Plex Sans";
/// Code: URLs, bodies, headers' values, line numbers. Held as `Theme::mono`.
pub const MONO: &str = "JetBrains Mono";

/// The weights the interface uses (regular, medium, semibold, bold) and the code font's three.
const FILES: &[&[u8]] = &[
    include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf"),
    include_bytes!("../assets/fonts/IBMPlexSans-Medium.ttf"),
    include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/IBMPlexSans-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
];

/// Register the bundled fonts. Before anything is laid out, so no text is shaped in a fallback.
pub fn install(cx: &mut App) {
    let fonts = FILES.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("[zuno] could not load the bundled fonts: {error}");
    }
}
