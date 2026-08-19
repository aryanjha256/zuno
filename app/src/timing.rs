//! `ZUNO_TIMING=1` instrumentation.
//!
//! One place to ask "are we measuring?", so the boot stages and the per-request
//! timings share a switch. See architecture.md §8 — a latency budget nobody measures
//! is a budget already blown.

use std::sync::OnceLock;

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("ZUNO_TIMING").is_some())
}

/// Print a timing line. Cheap to call unconditionally — formatting only happens when
/// the switch is on.
///
/// Available crate-wide via `#[macro_use] mod timing;` in `main.rs`.
macro_rules! timing {
    ($($arg:tt)*) => {
        if $crate::timing::enabled() {
            eprintln!("[zuno] {}", format_args!($($arg)*));
        }
    };
}
