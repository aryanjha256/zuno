//! The engine as a GPUI global.
//!
//! One engine per process, not per buffer: the connection pool inside it is what makes
//! a resend feel instant, and per-tab engines would each start cold. Stored as a global
//! for the same reason as `Theme` — every buffer needs it and threading it through the
//! view tree would only add noise.
//!
//! Installation can fail (spawning the runtime thread), and that is not fatal: the app
//! still opens, and `send` reports the failure inline like any other error.

use std::sync::Arc;

use gpui::{App, Global};
use zuno_core::Engine;

struct EngineHandle(Arc<Engine>);

impl Global for EngineHandle {}

/// Start the engine thread and register it. Returns the error rather than panicking so
/// the caller can decide — an API client that refuses to open because a thread failed
/// to spawn is worse than one that opens and says so.
pub fn install(cx: &mut App) -> std::io::Result<()> {
    let engine = Engine::new()?;
    cx.set_global(EngineHandle(Arc::new(engine)));
    Ok(())
}

pub trait ActiveEngine {
    /// `None` when the engine failed to start.
    fn engine(&self) -> Option<Arc<Engine>>;
}

impl ActiveEngine for App {
    fn engine(&self) -> Option<Arc<Engine>> {
        self.try_global::<EngineHandle>()
            .map(|handle| handle.0.clone())
    }
}
