//! Text editing primitives.
//!
//! GPUI ships the IME plumbing (`EntityInputHandler`, `ElementInputHandler`) but no
//! editor, so the single-line input here is built from scratch on top of it —
//! adapted from gpui's own `examples/input.rs`. See architecture.md §7.

pub mod text_input;

pub use text_input::TextInput;
