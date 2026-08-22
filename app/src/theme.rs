//! The theme: one place where every color in Zuno is decided.
//!
//! Held as a GPUI global rather than threaded through the view tree, because a
//! theme switch has to repaint everything at once. Read it with `cx.theme()`.
//!
//! `Theme` is `Clone` but not `Copy` (it carries a font name), so renders start
//! with a single `cx.theme().clone()` — that keeps the immutable global borrow
//! from colliding with the `&mut Context` needed to build elements.

use gpui::{App, Global, Hsla, SharedString, hsla, rgb};
use zuno_core::{Method, StatusClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub fn label(&self) -> &'static str {
        match self {
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub appearance: Appearance,

    // Surfaces
    pub bg: Hsla,
    pub bg_panel: Hsla,
    pub bg_elevated: Hsla,
    pub bg_hover: Hsla,

    // Lines
    pub border: Hsla,
    pub border_focused: Hsla,

    // Text editing
    pub cursor: Hsla,
    /// Translucent — it paints *under* the glyphs, so it must not hide them.
    pub selection: Hsla,

    // Text
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_on_accent: Hsla,
    pub accent: Hsla,

    // HTTP methods
    pub method_get: Hsla,
    pub method_post: Hsla,
    pub method_put: Hsla,
    pub method_patch: Hsla,
    pub method_delete: Hsla,
    pub method_other: Hsla,

    // Status classes
    pub status_info: Hsla,
    pub status_success: Hsla,
    pub status_redirect: Hsla,
    pub status_client_error: Hsla,
    pub status_server_error: Hsla,

    /// Token colors for the response viewer, read by `response_pane::json_row`.
    ///
    /// Grouped into its own struct because the whole palette was decided in one sitting in M1.0,
    /// before anything read it — the `#[allow(dead_code)]` that scoped the forward declaration is
    /// gone now that M1.3 shipped and every field is used.
    pub syntax: SyntaxTheme,

    /// Resolved at startup from what the OS actually has installed.
    pub mono: SharedString,
}

/// Token colors for the response viewer.
#[derive(Debug, Clone)]
pub struct SyntaxTheme {
    pub key: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub literal: Hsla,
    pub punct: Hsla,
}

impl Global for Theme {}

impl Theme {
    pub fn new(appearance: Appearance, mono: SharedString) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(mono),
            Appearance::Light => Self::light(mono),
        }
    }

    pub fn dark(mono: SharedString) -> Self {
        Self {
            appearance: Appearance::Dark,

            bg: rgb(0x1c1c1f).into(),
            bg_panel: rgb(0x18181b).into(),
            bg_elevated: rgb(0x26262b).into(),
            bg_hover: rgb(0x2e2e35).into(),

            border: rgb(0x2e2e35).into(),
            border_focused: rgb(0x5b8def).into(),

            cursor: rgb(0x7fa9f5).into(),
            selection: hsla(0.61, 0.72, 0.62, 0.32),

            text: rgb(0xe4e4e7).into(),
            text_muted: rgb(0x8b8b94).into(),
            text_on_accent: rgb(0xffffff).into(),
            accent: rgb(0x4470d0).into(),

            method_get: rgb(0x4ea8de).into(),
            method_post: rgb(0x6bbf59).into(),
            method_put: rgb(0xe0a33e).into(),
            method_patch: rgb(0xb98ee6).into(),
            method_delete: rgb(0xe06c6c).into(),
            method_other: rgb(0x8b8b94).into(),

            status_info: rgb(0x8b8b94).into(),
            status_success: rgb(0x6bbf59).into(),
            status_redirect: rgb(0x4ea8de).into(),
            status_client_error: rgb(0xe0a33e).into(),
            status_server_error: rgb(0xe06c6c).into(),

            syntax: SyntaxTheme {
                key: rgb(0x7cb7e8).into(),
                string: rgb(0xa8cf85).into(),
                number: rgb(0xe0b070).into(),
                literal: rgb(0xb98ee6).into(),
                punct: rgb(0x7b7b84).into(),
            },

            mono,
        }
    }

    pub fn light(mono: SharedString) -> Self {
        Self {
            appearance: Appearance::Light,

            bg: rgb(0xffffff).into(),
            bg_panel: rgb(0xf6f6f7).into(),
            bg_elevated: rgb(0xfbfbfc).into(),
            bg_hover: rgb(0xeeeef0).into(),

            border: rgb(0xdededf).into(),
            border_focused: rgb(0x3b6fd4).into(),

            cursor: rgb(0x2c5cc0).into(),
            selection: hsla(0.61, 0.70, 0.55, 0.26),

            text: rgb(0x1c1c1f).into(),
            text_muted: rgb(0x6b6b74).into(),
            text_on_accent: rgb(0xffffff).into(),
            accent: rgb(0x3b6fd4).into(),

            method_get: rgb(0x1f6f9e).into(),
            method_post: rgb(0x2f7a24).into(),
            method_put: rgb(0x92650b).into(),
            method_patch: rgb(0x7040a8).into(),
            method_delete: rgb(0xb32d2d).into(),
            method_other: rgb(0x6b6b74).into(),

            status_info: rgb(0x6b6b74).into(),
            status_success: rgb(0x2f7a24).into(),
            status_redirect: rgb(0x1f6f9e).into(),
            status_client_error: rgb(0x92650b).into(),
            status_server_error: rgb(0xb32d2d).into(),

            syntax: SyntaxTheme {
                key: rgb(0x1f5f9e).into(),
                string: rgb(0x2f7a24).into(),
                number: rgb(0x8a5a00).into(),
                literal: rgb(0x7040a8).into(),
                punct: rgb(0x8b8b94).into(),
            },

            mono,
        }
    }

    pub fn toggle(&mut self) {
        let mono = self.mono.clone();
        *self = match self.appearance {
            Appearance::Dark => Self::light(mono),
            Appearance::Light => Self::dark(mono),
        };
    }

    pub fn method_color(&self, method: &Method) -> Hsla {
        match method {
            Method::Get => self.method_get,
            Method::Post => self.method_post,
            Method::Put => self.method_put,
            Method::Patch => self.method_patch,
            Method::Delete => self.method_delete,
            Method::Head | Method::Options | Method::Other(_) => self.method_other,
        }
    }

    pub fn status_color(&self, class: StatusClass) -> Hsla {
        match class {
            StatusClass::Informational => self.status_info,
            StatusClass::Success => self.status_success,
            StatusClass::Redirect => self.status_redirect,
            StatusClass::ClientError => self.status_client_error,
            StatusClass::ServerError => self.status_server_error,
            StatusClass::Unknown => self.text_muted,
        }
    }

    /// The border color for a focusable region, given whether it holds focus.
    /// Focus has to be *visible* for a keyboard-first app to be usable at all.
    pub fn focus_border(&self, focused: bool) -> Hsla {
        if focused { self.border_focused } else { self.border }
    }
}

/// Monospace families worth trying, best first. GPUI falls back gracefully on a
/// miss, but picking from what's actually installed avoids a silent substitution
/// that makes column alignment look broken.
const MONO_CANDIDATES: &[&str] = &[
    "JetBrains Mono",
    "Fira Code",
    "Cascadia Code",
    "SF Mono",
    "Menlo",
    "Consolas",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Ubuntu Mono",
    "monospace",
];

pub fn pick_mono_font(cx: &App) -> SharedString {
    let available = cx.text_system().all_font_names();
    for candidate in MONO_CANDIDATES {
        if available.iter().any(|name| name == candidate) {
            return SharedString::from(*candidate);
        }
    }
    SharedString::from("monospace")
}

/// Sugar for reading the theme off any context that derefs to `App`.
pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}
