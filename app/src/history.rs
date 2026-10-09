//! Where the active workspace's global history lives, and the off-thread reads and writes.
//!
//! The format is `zuno_core::history`; this is only the seam, shaped like `session`'s: the
//! registry resolves a directory per workspace, and the test harness leaves it `None` so the suite
//! never writes into the developer's own history (invariant 6).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use gpui::{App, Global, Task};
use zuno_core::history::{Entry, Store};

/// The store, and a lock that keeps two quick sends from interleaving their writes — each
/// append runs as its own background task.
#[derive(Clone)]
struct Handle {
    store: Store,
    lock: Arc<Mutex<()>>,
}

struct HistoryDir(Option<Handle>);

impl Global for HistoryDir {}

/// Point history at a directory, or turn it off with `None`.
pub fn install_at(cx: &mut App, dir: Option<PathBuf>) {
    cx.set_global(HistoryDir(dir.map(|dir| Handle {
        store: Store::new(dir),
        lock: Arc::new(Mutex::new(())),
    })));
}

fn handle(cx: &App) -> Option<Handle> {
    cx.try_global::<HistoryDir>()?.0.clone()
}

/// Append an entry off the UI thread. Serializing the spec is formatting, so it goes too.
pub fn record(entry: Entry, body: Option<Bytes>, cx: &App) {
    let Some(handle) = handle(cx) else { return };
    cx.background_executor()
        .spawn(async move {
            let _guard = handle.lock.lock();
            if let Err(error) = handle.store.append(&entry, body.as_deref()) {
                eprintln!("[zuno] could not record history: {error}");
            }
        })
        .detach();
}

/// Every readable entry, newest first — `None` when history is off.
pub fn load(cx: &App) -> Option<Task<Vec<Entry>>> {
    let handle = handle(cx)?;
    Some(cx.background_executor().spawn(async move {
        let _guard = handle.lock.lock();
        handle.store.load()
    }))
}

/// A kept body, read off the UI thread.
pub fn read_body(id: u64, cx: &App) -> Task<Option<Bytes>> {
    let Some(handle) = handle(cx) else {
        return Task::ready(None);
    };
    cx.background_executor()
        .spawn(async move { handle.store.read_body(id) })
}
