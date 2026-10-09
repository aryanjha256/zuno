//! Where the active workspace's global history lives, what of it is loaded, and the off-thread
//! reads and writes.
//!
//! The format is `zuno_core::history`; this is the seam, shaped like `session`'s: the registry
//! resolves a directory per workspace, and the test harness leaves it `None` so the suite never
//! writes into the developer's own history (invariant 6).
//!
//! **The list is held here, not re-read per frame.** It is loaded once, off-thread, the first time
//! the History view is shown, and every send after that is prepended in memory as well as appended
//! on disk — so the panel updates as you send without reading the log back. `Workspace` observes
//! this global for exactly that.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use gpui::{App, Global, Task};
use zuno_core::history::{Entry, MAX_ENTRIES, Store};

/// The store, and a lock that keeps two quick sends from interleaving their writes — each
/// append runs as its own background task.
#[derive(Clone)]
struct Handle {
    store: Store,
    lock: Arc<Mutex<()>>,
}

#[derive(Default)]
pub struct HistoryDir {
    handle: Option<Handle>,
    /// Newest first. `None` until the first load finishes.
    entries: Option<Arc<Vec<Entry>>>,
    loading: bool,
}

impl Global for HistoryDir {}

/// Point history at a directory, or turn it off with `None`. Forgets whatever was loaded, which is
/// what a workspace switch needs.
pub fn install_at(cx: &mut App, dir: Option<PathBuf>) {
    cx.set_global(HistoryDir {
        handle: dir.map(|dir| Handle {
            store: Store::new(dir),
            lock: Arc::new(Mutex::new(())),
        }),
        entries: None,
        loading: false,
    });
}

fn handle(cx: &App) -> Option<Handle> {
    cx.try_global::<HistoryDir>()?.handle.clone()
}

/// The loaded entries, newest first — `None` while nothing is loaded or history is off.
pub fn entries(cx: &App) -> Option<Arc<Vec<Entry>>> {
    cx.try_global::<HistoryDir>()?.entries.clone()
}

/// Whether there is a history to show at all.
pub fn enabled(cx: &App) -> bool {
    handle(cx).is_some()
}

/// Read the log off-thread, once.
pub fn ensure_loaded(cx: &mut App) {
    let Some(state) = cx.try_global::<HistoryDir>() else { return };
    if state.entries.is_some() || state.loading {
        return;
    }
    let Some(handle) = state.handle.clone() else { return };
    cx.global_mut::<HistoryDir>().loading = true;
    let load = cx.background_executor().spawn({
        let handle = handle.clone();
        async move {
            let _guard = handle.lock.lock();
            handle.store.load()
        }
    });
    cx.spawn(async move |cx| {
        let loaded = load.await;
        let _ = cx.update(|cx| {
            let Some(state) = cx.try_global::<HistoryDir>() else { return };
            // A workspace switch while reading installed a different store; this list is not its.
            if state.handle.as_ref().map(|h| h.store.dir()) != Some(handle.store.dir()) {
                return;
            }
            let state = cx.global_mut::<HistoryDir>();
            state.entries = Some(Arc::new(loaded));
            state.loading = false;
        });
    })
    .detach();
}

/// Record a send: appended on disk off the UI thread (serializing the spec is formatting), and
/// prepended to the loaded list so an open History view shows it at once.
pub fn record(entry: Entry, body: Option<Bytes>, cx: &mut App) {
    let Some(handle) = handle(cx) else { return };
    if let Some(entries) = cx.global::<HistoryDir>().entries.clone() {
        let mut next = Vec::with_capacity(entries.len() + 1);
        next.push(entry.clone());
        next.extend(entries.iter().take(MAX_ENTRIES - 1).cloned());
        cx.global_mut::<HistoryDir>().entries = Some(Arc::new(next));
    }
    cx.background_executor()
        .spawn(async move {
            let _guard = handle.lock.lock();
            if let Err(error) = handle.store.append(&entry, body.as_deref()) {
                eprintln!("[zuno] could not record history: {error}");
            }
        })
        .detach();
}

/// Forget every entry in this workspace, on disk and in memory.
pub fn clear(cx: &mut App) {
    let Some(handle) = handle(cx) else { return };
    cx.global_mut::<HistoryDir>().entries = Some(Arc::new(Vec::new()));
    cx.background_executor()
        .spawn(async move {
            let _guard = handle.lock.lock();
            if let Err(error) = handle.store.clear() {
                eprintln!("[zuno] could not clear history: {error}");
            }
        })
        .detach();
}

/// A kept body, read off the UI thread.
pub fn read_body(id: u64, cx: &App) -> Task<Option<Bytes>> {
    let Some(handle) = handle(cx) else {
        return Task::ready(None);
    };
    cx.background_executor()
        .spawn(async move { handle.store.read_body(id) })
}

static LOCAL_OFFSET: OnceLock<i32> = OnceLock::new();

/// Read the local timezone's offset. **Call from `main` before gpui starts**: on Unix `time`
/// refuses to read it once the process has a second thread, since another thread could be
/// changing the environment underneath it. UTC when it cannot be read.
pub fn read_local_offset() {
    let offset = time::UtcOffset::current_local_offset()
        .map(|offset| offset.whole_seconds())
        .unwrap_or(0);
    let _ = LOCAL_OFFSET.set(offset);
}

/// Seconds east of UTC, as read at startup.
pub fn local_offset() -> i32 {
    LOCAL_OFFSET.get().copied().unwrap_or(0)
}
