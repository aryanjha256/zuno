//! The root view. Owns the buffers, hosts every application action handler, and
//! draws the chrome around the panes.
//!
//! Action handlers live here rather than on `RequestView` on purpose: dispatch
//! travels up the focus tree, and `Workspace` is the one element guaranteed to be on
//! that path no matter which region holds focus — including when focus is inside a
//! `TextInput` nested two levels down. Handlers that need buffer state reach into the
//! active `RequestView` through its entity.

use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    ClipboardItem, Styled, Subscription, Task, Window, div, px,
};
use zuno_core::{
    Environment, RawKind, RequestId, RequestSpec, Resolver, collection, environment,
};

use crate::actions::{
    AddFormField, AddHeader, AddMultipartField, AddQuery, CancelRequest, ChooseBodyFile,
    ClearCookies, CloseTab, CopyResponse, FocusBody, FocusNext, FocusPrev, FocusResponse, FocusUrl, FoldAll, ImportCurl, NewTab, NextTab,
    OpenBodyType, OpenMethod, OpenPalette, OpenRequest, OpenSettings, PickerConfirm, PickerDismiss,
    PickerNext, PickerPrev, PrevTab, Quit, RemoveRow, SaveRequest, SaveResponse, SendRequest,
    SettingConfirm, SettingDecrease, SettingIncrease, SettingNext, SettingPrev, SettingsDismiss,
    ShowHistory, SwitchEnvironment, ToggleRow, ToggleTheme, UnfoldAll,
};
use crate::engine::ActiveEngine;
use crate::picker;
use crate::settings_panel::{SettingsEvent, SettingsPanel};
use crate::request_view::{BodyType, RequestView, RowKind};
use crate::theme::{ActiveTheme, Theme};

pub struct Workspace {
    focus_handle: FocusHandle,
    /// Last title handed to the OS, so `set_window_title` isn't called every frame.
    window_title: String,
    /// Holding the quit subscription is what keeps it alive.
    _quit_subscription: Subscription,
    /// One entry per open request; only `active_ix` is rendered as a pane. Every mutation
    /// goes through `activate`, which is what keeps focus and `active_ix` from disagreeing.
    views: Vec<Entity<RequestView>>,
    active_ix: usize,
    /// The picker, while it's open. `None` is the closed state, so a closed picker costs
    /// nothing to render and cannot hold stale results.
    picker: Option<PickerState>,
    /// Holding the task is what keeps the collection scan alive; dropping it cancels.
    picker_scan: Option<Task<()>>,
    /// The settings panel, while it's open.
    settings: Option<SettingsState>,
    /// Holding the task is what keeps a save-response dialog and its write alive.
    response_save: Option<Task<()>>,
    /// Same, for the checkpoint write a send kicks off.
    session_save: Option<Task<()>>,
    /// Same, for the choose-a-body-file dialog.
    body_file_prompt: Option<Task<()>>,
    /// The selected environment's name, restored from the session.
    ///
    /// Only the *name* is held. The values are re-read from disk on every send, so editing
    /// `dev.json` in another window takes effect on the next request rather than on the next
    /// restart — the files are the interface, so they have to stay authoritative.
    environment: Option<String>,
}

/// The settings panel and the subscription that lets it close, for the same reason
/// `PickerState` pairs them: either alone leaves a modal nothing can dismiss.
struct SettingsState {
    panel: Entity<SettingsPanel>,
    _subscription: Subscription,
}

/// The picker plus the subscription that lets it be closed. Dropping either without the
/// other would leave a modal nothing can dismiss, so they live and die together.
struct PickerState {
    picker: Entity<picker::Picker>,
    _subscription: Subscription,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Reopen where you left off. A missing or unreadable session falls back to one
        // sample request rather than an empty window. `session::load` guarantees a
        // non-empty `tabs` and an in-range `active`, so neither is re-checked here.
        let session = crate::session::load(cx)
            .unwrap_or_else(|| crate::session::Session::single(RequestSpec::sample()));
        let active_ix = session.active;
        let environment = session.environment.clone();
        let views: Vec<_> = session
            .tabs
            .into_iter()
            .map(|tab| {
                cx.new(|cx| {
                    let mut view = RequestView::new(tab.spec, cx);
                    // Restoring this is what makes Ctrl+S after a restart overwrite the
                    // request's own file instead of deriving a fresh name beside it.
                    view.path = tab.path;
                    view
                })
            })
            .collect();

        // Start focused on the URL bar of the buffer that was in front — the loop begins
        // with typing a URL, and it puts that input on the focus path so its key context
        // is live from the first frame.
        let url_focus = views[active_ix].read(cx).url_focus(cx);
        window.focus(&url_focus);

        // Save on *every* quit path, not just Ctrl+Q. Before this, closing the window
        // with the window manager's button lost every edit made since the last send —
        // `session::save` was only reachable from the Send and Quit actions.
        let quit_subscription = cx.on_app_quit(|workspace, cx| {
            // Drop any in-flight background checkpoint before writing. Otherwise a write queued
            // by a send moments ago could land *after* this one and put older state back on
            // disk; dropping the task cancels it.
            workspace.session_save = None;
            crate::session::save(&workspace.session(cx), cx);
            // The hook wants a future; there is nothing to await, since the write is
            // synchronous and must finish before the process goes away.
            async {}
        });

        Self {
            focus_handle: cx.focus_handle(),
            window_title: String::new(),
            _quit_subscription: quit_subscription,
            views,
            active_ix,
            picker: None,
            picker_scan: None,
            settings: None,
            response_save: None,
            session_save: None,
            body_file_prompt: None,
            environment,
        }
    }

    pub fn active(&self) -> Option<Entity<RequestView>> {
        self.views.get(self.active_ix).cloned()
    }

    /// Whether a modal currently owns the keyboard.
    ///
    /// One predicate rather than the same two checks spelled out at seven call sites, because
    /// spelling them out is how they drift: `open_request` and `open_palette` each shipped
    /// testing only `picker`, so `Ctrl+P` over the settings panel stacked a second modal — and
    /// closing the picker then restored focus to the buffer *behind* the panel, stranding it on
    /// screen with a key context that no longer matched anything.
    ///
    /// Everything that opens a modal, and everything that moves focus, has to consult this.
    fn modal_open(&self) -> bool {
        self.picker.is_some() || self.settings.is_some()
    }

    /// How many buffers are open. The strip renders from `render`'s own collected list, so
    /// this stays test-only until something in the UI needs the bare count.
    #[cfg(test)]
    pub fn tab_count(&self) -> usize {
        self.views.len()
    }

    #[cfg(test)]
    pub fn active_environment(&self) -> Option<String> {
        self.environment.clone()
    }

    #[cfg(test)]
    pub fn picker_is_open(&self) -> bool {
        self.picker.is_some()
    }

    /// The picker's visible rows as `label — detail`, which is what a test can assert on
    /// without reaching into rendered elements.
    #[cfg(test)]
    pub fn picker_rows(&self, cx: &App) -> Vec<String> {
        self.picker
            .as_ref()
            .map(|state| state.picker.read(cx).visible_rows())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn picker_selection(&self, cx: &App) -> usize {
        self.picker
            .as_ref()
            .map(|state| state.picker.read(cx).selection())
            .unwrap_or(0)
    }

    /// Move focus into a buffer's URL bar and repaint.
    ///
    /// Every switch has to end here. A `FocusHandle` belongs to the entity that made it,
    /// so after the active buffer changes, focus is still sitting inside the *old* view —
    /// and after a close it's inside a dropped one, where no key context matches and the
    /// keymap goes dead with nothing on screen explaining why.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.views.get(ix) else { return };
        self.active_ix = ix;

        let url_focus = view.read(cx).url_focus(cx);
        window.focus(&url_focus);
        // Unlike `focus_region`, this needs the notify: the strip and the title change
        // even when `window.focus` finds the handle already focused.
        cx.notify();
    }

    /// Ids have to be distinct across buffers, and nothing hands them out — `sample()`
    /// hardcodes 1 and `default()` 0. Highest-plus-one is enough for a session's lifetime
    /// and needs no counter to persist and keep in sync.
    fn next_id(&self, cx: &App) -> RequestId {
        let highest = self
            .views
            .iter()
            .map(|view| view.read(cx).id.0)
            .max()
            .unwrap_or(0);
        RequestId(highest + 1)
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let spec = RequestSpec {
            id: self.next_id(cx),
            ..RequestSpec::default()
        };
        self.open(spec, window, cx);
    }

    /// Add a buffer and switch to it. Shared by `NewTab` and curl import.
    fn open(&mut self, spec: RequestSpec, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| RequestView::new(spec, cx));
        self.views.push(view);
        self.activate(self.views.len() - 1, window, cx);
    }

    /// Closing the last buffer leaves a fresh one rather than an empty window.
    ///
    /// An empty `views` would make `active()` return `None`, which every action handler
    /// treats as "do nothing" — the window would still be there, silently inert. Ctrl+W
    /// also shouldn't quit the app; that's Ctrl+Q's job.
    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() {
            return;
        }

        // Cancellation has two halves, and dropping the buffer is only one of them. Dropping
        // its task stops the UI *consuming* events; the socket keeps draining into a buffer
        // nothing will ever read, for up to the request's timeout. `Escape` does both — so
        // must this, and only the workspace holds the engine to do it with.
        if let (Some(view), Some(engine)) = (self.active(), cx.engine()) {
            view.update(cx, |view, cx| {
                view.cancel(&engine, cx);
            });
        }

        self.views.remove(self.active_ix);

        if self.views.is_empty() {
            let spec = RequestSpec::default();
            self.open(spec, window, cx);
            return;
        }

        // Closing the last tab in the strip moves left; anything else keeps the index,
        // which now points at what was to the right — the behaviour every editor has.
        let next = self.active_ix.min(self.views.len() - 1);
        self.activate(next, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() {
            return;
        }
        // Wraps, so cycling never dead-ends at either edge.
        let next = (self.active_ix + 1) % self.views.len();
        self.activate(next, window, cx);
    }

    fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() {
            return;
        }
        let prev = (self.active_ix + self.views.len() - 1) % self.views.len();
        self.activate(prev, window, cx);
    }

    /// Open the request picker: every open buffer, then every saved request.
    ///
    /// Buffers come first because for the common case — a handful of tabs open — Ctrl+P is
    /// a tab switcher, and that makes it useful from the first keystroke rather than only
    /// once a collection has grown. A saved request already open as a buffer appears once,
    /// as the buffer, so choosing it switches instead of opening a second copy of the same
    /// file.
    ///
    /// The scan is file IO and JSON parsing, so it goes to the background executor
    /// (invariant 3) and the picker opens immediately with the buffer rows. Scanning on
    /// open rather than caching at startup is deliberate: a collection is a git directory,
    /// so it changes underneath us whenever someone pulls or edits a file by hand.
    fn open_request(&mut self, _: &OpenRequest, window: &mut Window, cx: &mut Context<Self>) {
        // A second Ctrl+P while any modal is open is a no-op, not a nested modal.
        if self.modal_open() {
            return;
        }

        let open_paths: Vec<Option<PathBuf>> = self
            .views
            .iter()
            .map(|view| view.read(cx).path.clone())
            .collect();

        let buffer_items: Vec<picker::Item> = self
            .views
            .iter()
            .enumerate()
            .map(|(ix, view)| {
                let view = view.read(cx);
                picker::Item {
                    label: view.label(cx),
                    detail: SharedString::from(view.url.read(cx).text().to_string()),
                    target: picker::Target::Buffer(ix),
                }
            })
            .collect();

        let picker = self.show_picker(
            buffer_items,
            "No saved requests yet — press Ctrl+S to save the one you're editing",
            window,
            cx,
        );

        // Fill in the saved requests as they arrive. The picker is already usable.
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };
        let scan = cx.background_executor().spawn(async move {
            zuno_core::collection::scan(&root)
                .into_iter()
                .map(|entry| (entry.relative, entry.path, entry.spec.url))
                .collect::<Vec<_>>()
        });

        self.picker_scan = Some(cx.spawn(async move |_this, cx| {
            let found = scan.await;
            let _ = picker.update(cx, |picker, cx| {
                picker.extend(
                    found
                        .into_iter()
                        // A request already open as a buffer is not listed twice.
                        .filter(|(_, path, _)| !open_paths.contains(&Some(path.clone())))
                        .map(|(relative, path, url)| picker::Item {
                            label: SharedString::from(relative),
                            detail: SharedString::from(url),
                            target: picker::Target::File(path),
                        }),
                    cx,
                );
            });
        }));
    }

    /// Open the command palette: every verb in `commands::palette`, with its keybinding.
    ///
    /// The same picker as Ctrl+P, which is the whole point of principle 2 — a different
    /// `Vec<Item>` and a different `Target` variant, no new interaction.
    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }

        let items = crate::commands::palette()
            .into_iter()
            .map(|command| picker::Item {
                label: SharedString::from(command.label),
                // The keybinding, so the palette teaches the shortcut rather than
                // replacing it. Blank for commands that have none.
                detail: SharedString::from(keybinding_hint(command.action.as_ref(), window)),
                target: picker::Target::Action(command.action),
            })
            .collect();

        // `palette()` is a non-empty literal, so an empty list means the filter matched
        // nothing, never that there was nothing to show.
        self.show_picker(items, "No commands", window, cx);
    }

    /// Put a picker on screen, focused, with the subscription that lets it close.
    ///
    /// Shared by Ctrl+P and Ctrl+K so there is exactly one place that gets focus and
    /// teardown right.
    fn show_picker(
        &mut self,
        items: Vec<picker::Item>,
        empty_hint: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<picker::Picker> {
        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        let picker = cx.new(|cx| picker::Picker::new(items, empty_hint, restore, cx));

        // Dropping this subscription would make the picker unclosable, so it's held
        // alongside the entity for exactly as long as the picker exists.
        let subscription =
            cx.subscribe_in(&picker, window, |workspace, picker, event, window, cx| match event {
                picker::PickerEvent::Dismissed => workspace.close_picker(window, cx),
                picker::PickerEvent::Confirmed => {
                    let chosen = picker.read(cx).chosen().cloned();
                    // Closed *before* acting, and the order is load-bearing for
                    // `Buffer`/`File`: `activate` focuses synchronously, so closing
                    // afterwards would have `close_picker` restore focus to the *previous*
                    // buffer, leaving `active_ix` and focus disagreeing — you'd type into
                    // the request you just navigated away from.
                    //
                    // It makes no difference for `Action`: `Window::dispatch_action`
                    // captures the focus id and then `cx.defer`s the dispatch, so a command
                    // behaves identically either way. Verified, not assumed — see
                    // `choosing_a_buffer_leaves_focus_in_that_buffer`.
                    workspace.close_picker(window, cx);
                    if let Some(target) = chosen {
                        workspace.choose(target, window, cx);
                    }
                }
            });

        let focus = picker.read(cx).focus_handle(cx);
        self.picker = Some(PickerState {
            picker: picker.clone(),
            _subscription: subscription,
        });
        window.focus(&focus);
        cx.notify();
        picker
    }

    /// Assemble the resolver for a send: globals underneath, the selected environment on
    /// top.
    ///
    /// Deliberately free of side effects. An earlier version ensured the `.gitignore` rule
    /// here, which meant a function running on *every send* wrote to the user's repository —
    /// and the notice it set was then wiped by `RequestView::send`, which clears `status`.
    /// Protecting the repo belongs at the moment of switching; see `choose`.
    fn resolver(&self, cx: &App) -> Resolver {
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return Resolver::default();
        };

        let globals = environment::load(&root, environment::GLOBALS).ok();
        let active = self
            .environment
            .as_deref()
            .and_then(|name| match environment::load(&root, name) {
                Ok(env) => Some(env),
                Err(error) => {
                    eprintln!("[zuno] {error}");
                    None
                }
            });

        Resolver::new(globals.as_ref(), active.as_ref())
    }

    /// Make sure the collection ignores `*.local.json`, if the selected environment has any
    /// secrets to protect.
    ///
    /// Done on *switch* rather than on send: it's the earliest moment we know secrets are in
    /// play, it happens once instead of per request, and the status message survives — a send
    /// clears `status`, so a notice set during one is never seen.
    ///
    /// Narrow on purpose. Zuno writing into a file it doesn't own is an intrusion, so it
    /// happens only when there is something to protect and is always reported.
    fn protect_secrets(&mut self, cx: &mut Context<Self>) {
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };
        let Some(name) = self.environment.clone() else {
            return;
        };

        let has_secrets = environment::load(&root, &name)
            .map(|env| !env.secret.is_empty())
            .unwrap_or(false);
        if !has_secrets {
            return;
        }

        match environment::ensure_gitignored(&root) {
            Ok(true) => self.set_status("Added *.local.json to the collection's .gitignore", cx),
            Ok(false) => {}
            Err(error) => eprintln!("[zuno] {error}"),
        }
    }

    /// Browse the responses this buffer has already received.
    ///
    /// Ten runs per buffer were already retained and, until now, read by nothing at all —
    /// not even the diff, which is computed once when a response lands. So this isn't only a
    /// feature: it's what makes the retention worth its memory, since holding ten response
    /// bodies per tab that nothing can reach is pure cost.
    fn show_history(&mut self, _: &ShowHistory, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(view) = self.active() else { return };

        let current = view.read(cx).viewing();
        let items: Vec<picker::Item> = view
            .read(cx)
            .runs()
            .into_iter()
            .map(|(offset, response)| {
                // The label carries the status, because "which run was the 500?" is the
                // question you open this to answer.
                let label = match offset {
                    0 => format!("live · {} {}", response.status, response.status_text),
                    1 => format!("1 send ago · {} {}", response.status, response.status_text),
                    n => format!("{n} sends ago · {} {}", response.status, response.status_text),
                };
                let mut detail = format!(
                    "{} · {:?}",
                    format_bytes(response.size.decoded),
                    response.timing.total
                );
                if offset == current {
                    detail.push_str(" · showing");
                }
                picker::Item {
                    label: SharedString::from(label),
                    detail: SharedString::from(detail),
                    target: picker::Target::Run(offset),
                }
            })
            .collect();

        self.show_picker(items, "Nothing sent yet from this request", window, cx);
    }

    /// Pick the active environment. `None` is always offered, since "send it raw" is a
    /// legitimate choice and otherwise there'd be no way back out.
    fn switch_environment(
        &mut self,
        _: &SwitchEnvironment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }

        let root = crate::collections::root(cx).map(Path::to_path_buf);
        let found: Vec<Environment> = root
            .as_deref()
            .map(environment::scan)
            .unwrap_or_default();

        let current = self.environment.clone();
        let mut items = vec![picker::Item {
            label: SharedString::from("None"),
            detail: if current.is_none() {
                SharedString::from("current — variables are left unresolved")
            } else {
                SharedString::from("send requests without substitution")
            },
            target: picker::Target::Environment(None),
        }];

        items.extend(found.into_iter().map(|env| {
            let is_current = current.as_deref() == Some(env.name.as_str());
            // Counts rather than values: a switcher is not a place to leak a token onto the
            // screen, and the count is what tells you the file was actually found.
            let secrets = env.secret.len();
            let summary = match (env.values.len(), secrets) {
                (n, 0) => format!("{n} variables"),
                (n, s) => format!("{n} variables, {s} secret"),
            };
            picker::Item {
                label: SharedString::from(env.name.clone()),
                detail: SharedString::from(if is_current {
                    format!("current — {summary}")
                } else {
                    summary
                }),
                target: picker::Target::Environment(Some(env.name)),
            }
        }));

        self.show_picker(
            items,
            "No environments — add one to environments/ in your collection",
            window,
            cx,
        );
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.picker.take() else { return };
        self.picker_scan = None;

        // Focus is currently inside the picker's filter input, which is about to be
        // dropped. Leaving it there means no key context matches and the whole keymap goes
        // dead with nothing on screen explaining why — the same failure as switching tabs
        // without moving focus.
        if let Some(handle) = state.picker.read(cx).restore_focus() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// Act on a picked row.
    fn choose(&mut self, target: picker::Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            picker::Target::Buffer(ix) => self.activate(ix, window, cx),
            picker::Target::Action(action) => {
                // Dispatched rather than called directly, so a palette entry and its
                // keybinding run the identical path — the convention in CLAUDE.md.
                window.dispatch_action(action, cx);
            }
            picker::Target::Run(offset) => {
                if let Some(view) = self.active() {
                    view.update(cx, |view, cx| view.view_run(offset, cx));
                }
            }
            picker::Target::Environment(name) => {
                self.environment = name;
                // Persisted immediately rather than at the next send: switching environment
                // and then closing the window should not silently forget which one you chose.
                crate::session::save(&self.session(cx), cx);
                self.protect_secrets(cx);
                cx.notify();
            }
            picker::Target::BodyType(body_type, kind) => {
                let Some(view) = self.active() else { return };
                view.update(cx, |view, cx| {
                    if let Some(kind) = kind {
                        view.body_kind = kind;
                    }
                    view.set_body_type(body_type, cx);
                });

                // A stale `Content-Type` header outranks the body you just chose, so the
                // request would go out urlencoded while claiming to be JSON. Say so at the
                // moment the choice is made, which is the only moment it's surprising.
                if let Some((declared, expected)) = view.read(cx).conflicting_content_type(cx) {
                    self.set_status(
                        &format!("Header Content-Type: {declared} overrides this — expected {expected}"),
                        cx,
                    );
                }
            }
            picker::Target::Method(method) => {
                if let Some(view) = self.active() {
                    view.update(cx, |view, cx| {
                        view.method = method;
                        cx.notify();
                    });
                }
            }
            picker::Target::File(path) => {
                // The file may have been deleted or broken since the scan; report rather
                // than opening an empty buffer.
                let spec = match zuno_core::collection::read(&path) {
                    Ok(spec) => spec,
                    Err(error) => {
                        self.set_status(&format!("Could not open: {error}"), cx);
                        return;
                    }
                };

                // Stored ids are always 0 (see `collection`), so a live one is assigned
                // here — the workspace is the only thing that knows which are taken.
                let spec = RequestSpec {
                    id: self.next_id(cx),
                    ..spec
                };
                self.open(spec, window, cx);
                // Remembering the file is what makes a later Ctrl+S overwrite it rather
                // than derive a fresh name beside it.
                if let Some(view) = self.active() {
                    view.update(cx, |view, cx| {
                        view.path = Some(path);
                        cx.notify();
                    });
                }
            }
        }
    }

    fn picker_next(&mut self, _: &PickerNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.picker {
            state.picker.update(cx, |picker, cx| picker.select(1, cx));
        }
    }

    fn picker_prev(&mut self, _: &PickerPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.picker {
            state.picker.update(cx, |picker, cx| picker.select(-1, cx));
        }
    }

    fn picker_confirm(&mut self, _: &PickerConfirm, _: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &self.picker else { return };
        // Nothing matched: swallow the keystroke rather than closing, so a typo doesn't
        // dismiss the picker you were halfway through using.
        if state.picker.read(cx).chosen().is_none() {
            return;
        }
        // Emitted rather than handled inline so confirm-by-key and confirm-by-click run
        // the exact same path through the subscription.
        state
            .picker
            .update(cx, |_, cx| cx.emit(picker::PickerEvent::Confirmed));
    }

    fn picker_dismiss(&mut self, _: &PickerDismiss, window: &mut Window, cx: &mut Context<Self>) {
        self.close_picker(window, cx);
    }

    /// Open the settings panel over the active buffer's `RequestSettings`.
    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(view) = self.active() else { return };

        let settings = view.read(cx).settings.clone();
        let restore = Some(view.read(cx).url_focus(cx));
        let panel = cx.new(|cx| SettingsPanel::new(settings, restore, cx));

        let subscription =
            cx.subscribe_in(&panel, window, |workspace, _, event, window, cx| match event {
                SettingsEvent::Dismissed => workspace.close_settings(window, cx),
                // Cookies live in the engine, not in `RequestSettings`, so the panel can't
                // do this itself.
                SettingsEvent::ClearCookies => workspace.clear_cookies(&ClearCookies, window, cx),
            });

        let focus = panel.read(cx).focus_handle();
        self.settings = Some(SettingsState {
            panel,
            _subscription: subscription,
        });
        window.focus(&focus);
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.settings.take() else { return };
        // Same discipline as the picker: focus is inside a panel that's about to be dropped,
        // and leaving it there kills the keymap silently.
        if let Some(handle) = state.panel.read(cx).restore_focus() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// Copy the panel's edits onto the active buffer.
    ///
    /// Written back on every change rather than on close, so dismissing with Esc keeps what
    /// you changed — there is no OK/Cancel here, and a modal that silently discards edits on
    /// Esc is worse than one that has no Esc.
    fn commit_settings(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &self.settings else { return };
        let Some(view) = self.active() else { return };
        let settings = state.panel.read(cx).settings().clone();
        view.update(cx, |view, cx| {
            view.settings = settings;
            cx.notify();
        });
    }

    fn setting_next(&mut self, _: &SettingNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.settings {
            state.panel.update(cx, |panel, cx| panel.select(1, cx));
        }
    }

    fn setting_prev(&mut self, _: &SettingPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.settings {
            state.panel.update(cx, |panel, cx| panel.select(-1, cx));
        }
    }

    fn setting_increase(&mut self, _: &SettingIncrease, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_setting(1, cx);
    }

    fn setting_decrease(&mut self, _: &SettingDecrease, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_setting(-1, cx);
    }

    fn adjust_setting(&mut self, delta: i64, cx: &mut Context<Self>) {
        let Some(state) = &self.settings else { return };
        let changed = state.panel.update(cx, |panel, cx| panel.adjust(delta, cx));
        if changed {
            self.commit_settings(cx);
        }
    }

    fn setting_confirm(&mut self, _: &SettingConfirm, _: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &self.settings else { return };
        let changed = state.panel.update(cx, |panel, cx| panel.confirm(cx));
        if changed {
            self.commit_settings(cx);
        }
    }

    fn settings_dismiss(&mut self, _: &SettingsDismiss, window: &mut Window, cx: &mut Context<Self>) {
        self.close_settings(window, cx);
    }

    /// Throw away every stored cookie.
    ///
    /// Reachable from the settings panel, the palette, and a keybinding, which is why it's
    /// an action rather than a method the panel calls.
    fn clear_cookies(&mut self, _: &ClearCookies, _: &mut Window, cx: &mut Context<Self>) {
        let Some(engine) = cx.engine() else {
            self.set_status("The HTTP engine is not running", cx);
            return;
        };
        engine.clear_cookies();
        self.set_status("Cleared stored cookies", cx);
    }

    /// Whether the active request will store and replay cookies.
    ///
    /// Surfaced in the status bar because the jar is on by default and otherwise invisible,
    /// which makes consecutive requests non-independent with nothing on screen saying so.
    pub fn cookies_enabled(&self, cx: &App) -> bool {
        self.active()
            .map(|view| view.read(cx).settings.cookie_store)
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn settings_is_open(&self) -> bool {
        self.settings.is_some()
    }

    #[cfg(test)]
    pub fn settings_rows(&self, cx: &App) -> Vec<String> {
        self.settings
            .as_ref()
            .map(|state| state.panel.read(cx).rows_for_test())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn settings_selection(&self, cx: &App) -> usize {
        self.settings
            .as_ref()
            .map(|state| state.panel.read(cx).selection())
            .unwrap_or(0)
    }

    /// The persistable state of every open buffer.
    ///
    /// Walks *all* views, not just the active one. Saving only `active()` was correct
    /// while one buffer was the only buffer; with a tab strip coming it would quietly
    /// discard every other open request on quit.
    fn session(&self, cx: &App) -> crate::session::Session {
        let tabs = self
            .views
            .iter()
            .map(|view| {
                let view = view.read(cx);
                crate::session::Tab {
                    spec: view.spec(cx),
                    path: view.path.clone(),
                }
            })
            .collect();
        crate::session::Session::new(tabs, self.active_ix, self.environment.clone())
    }

    /// `Window::focus` refreshes the whole window internally, so there's no
    /// `cx.notify()` here — the child `RequestView` repaints (and updates its focus
    /// ring) on its own. It also no-ops when the handle is already focused, so a
    /// redundant notify would cost a frame for nothing.
    fn focus_region(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        pick: impl Fn(&RequestView, &App) -> FocusHandle,
    ) {
        let Some(view) = self.active() else { return };
        let handle = pick(view.read(cx), cx);
        window.focus(&handle);
    }

    fn focus_url(&mut self, _: &FocusUrl, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view, cx| view.url_focus(cx));
    }

    fn focus_body(&mut self, _: &FocusBody, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view, cx| view.body_focus(cx));
    }

    fn focus_response(&mut self, _: &FocusResponse, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_region(window, cx, |view, _| view.response_focus.clone());
    }

    /// Tab and Shift+Tab move focus *within* a buffer, so they must do nothing while a modal
    /// owns the keyboard.
    ///
    /// **Why a guard and not a key context.** The bindings are global, and the panes behind a
    /// modal are still painted — so their `TextInput`s are still tab stops (`TextInput::new`
    /// sets `tab_stop(true)`) and `focus_next` walks straight past the scrim into them. The
    /// modal's leaf key context then stops matching, which silently kills every binding it
    /// owns: up/down to move, Enter to confirm, and Escape to dismiss. What's left is a modal
    /// on screen that only the mouse can close. Scoping the binding instead would mean encoding
    /// "not in a modal" as a context predicate, and GPUI matches only the *leaf* context, so
    /// that has to be restated for every modal that ever exists.
    ///
    /// A modal moves its own selection with up/down, so there is nothing for Tab to do inside
    /// one and swallowing it costs nothing.
    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, _: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        window.focus_next();
    }

    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, _: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        window.focus_prev();
    }

    /// Open the method picker.
    ///
    /// Replaces cycling, which needed seven presses to reach OPTIONS and gave no way at all
    /// to reach `Method::Other`. Because the picker has a filter input, typing an unknown
    /// verb offers it — closing the last of §11's non-body gaps (custom HTTP methods).
    fn open_method(&mut self, _: &OpenMethod, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(view) = self.active() else { return };
        let current = view.read(cx).method.clone();

        let items = zuno_core::Method::common()
            .into_iter()
            .map(|method| picker::Item {
                label: SharedString::from(method.as_str().to_string()),
                // Marks where you are, so the list answers "what is it now?" as well as
                // "what could it be?".
                detail: if method == current {
                    SharedString::from("current")
                } else {
                    SharedString::default()
                },
                target: picker::Target::Method(method),
            })
            .collect();

        let picker = self.show_picker(items, "No methods", window, cx);
        picker.update(cx, |picker, _| picker.set_fallback(custom_method_row));
    }

    fn add_header(&mut self, _: &AddHeader, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.add_row(RowKind::Header, window, cx));
        }
    }

    fn add_query(&mut self, _: &AddQuery, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.add_row(RowKind::Query, window, cx));
        }
    }

    /// Row actions target whichever row currently holds focus, so there's no
    /// "selected row" index to keep valid across insertions and deletions.
    fn toggle_row(&mut self, _: &ToggleRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let handled = view.update(cx, |view, cx| view.toggle_focused_row(window, cx));
        if !handled {
            self.set_status("Focus a header or query row first", cx);
        }
    }

    fn remove_row(&mut self, _: &RemoveRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let handled = view.update(cx, |view, cx| view.remove_focused_row(window, cx));
        if !handled {
            self.set_status("Focus a header or query row first", cx);
        }
    }

    /// Pick the body type.
    ///
    /// Replaces cycling, which walked `RawKind` — JSON, Text, XML, HTML — and so could never
    /// reach a form body at all. Multipart and binary are deliberately absent until their
    /// editors exist: offering a type nothing can author is worse than not offering it.
    fn open_body_type(&mut self, _: &OpenBodyType, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(view) = self.active() else { return };
        let current = view.read(cx).body_label();

        let choices: [(&str, BodyType, Option<RawKind>, &str); 8] = [
            ("None", BodyType::Empty, None, "send no body at all"),
            ("JSON", BodyType::Raw, Some(RawKind::Json), "application/json"),
            ("Form", BodyType::Form, None, "application/x-www-form-urlencoded"),
            ("Binary", BodyType::Binary, None, "the contents of a file"),
            ("Multipart", BodyType::Multipart, None, "multipart/form-data"),
            ("Text", BodyType::Raw, Some(RawKind::Text), "text/plain"),
            ("XML", BodyType::Raw, Some(RawKind::Xml), "application/xml"),
            ("HTML", BodyType::Raw, Some(RawKind::Html), "text/html"),
        ];

        let items = choices
            .into_iter()
            .map(|(label, body_type, kind, hint)| picker::Item {
                label: SharedString::from(label),
                detail: SharedString::from(if current == label {
                    format!("current · {hint}")
                } else {
                    hint.to_string()
                }),
                target: picker::Target::BodyType(body_type, kind),
            })
            .collect();

        self.show_picker(items, "No body types", window, cx);
    }

    /// Pick the file a binary body sends, switching the body type to match.
    ///
    /// Same shape as `add_form_field`: the keystroke plainly means "send this file", so it
    /// switches type rather than refusing because the body is currently something else.
    fn choose_body_file(
        &mut self,
        _: &ChooseBodyFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.active() else { return };

        // One verb, two meanings, decided by where focus is: with a multipart part focused it
        // fills that part, otherwise it sets the whole binary body. Two separate actions for
        // "pick a file" would be two keystrokes to remember for the same intent.
        let part = view.read(cx).focused_multipart_row(window, cx);

        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            // One body, one file. Multipart is where several belong.
            multiple: false,
            prompt: Some("Send as body".into()),
        });

        self.body_file_prompt = Some(cx.spawn(async move |workspace, cx| {
            // Cancelled, or the platform couldn't open a picker at all.
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };

            let _ = workspace.update(cx, |workspace, cx| {
                let shown = path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());

                match part {
                    Some(ix) => {
                        view.update(cx, |view, cx| view.set_multipart_file(ix, path, cx));
                        workspace.set_status(&format!("Attached {shown} to this part"), cx);
                    }
                    None => {
                        view.update(cx, |view, cx| view.set_binary_path(path, cx));
                        workspace.set_status(&format!("Sending {shown} as the body"), cx);
                    }
                }
            });
        }));
    }

    fn add_form_field(&mut self, _: &AddFormField, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        // Adding a field to a body that isn't a form would put a row somewhere invisible, so
        // switch first and say so — it's what the keystroke plainly means.
        if view.read(cx).body_type != BodyType::Form {
            view.update(cx, |view, cx| view.set_body_type(BodyType::Form, cx));
            self.set_status("Switched the body to a form", cx);
        }
        view.update(cx, |view, cx| view.add_row(RowKind::Form, window, cx));
    }

    fn add_multipart_field(
        &mut self,
        _: &AddMultipartField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.active() else { return };

        if view.read(cx).body_type != BodyType::Multipart {
            view.update(cx, |view, cx| view.set_body_type(BodyType::Multipart, cx));
            self.set_status("Switched the body to multipart", cx);
        }
        view.update(cx, |view, cx| view.add_row(RowKind::Multipart, window, cx));
    }

    /// Open a request parsed from a curl command on the clipboard in a **new** buffer.
    ///
    /// Reading the clipboard rather than opening a paste dialog is deliberate: the whole
    /// value of this feature is that "Copy as cURL" in devtools is one keystroke away
    /// from a request you can edit.
    ///
    /// It replaced the active buffer until tabs existed, which was only ever defensible
    /// because there was nowhere else to put the result — an import over unsaved work
    /// destroyed it with no undo. `RequestView::load` still exists for genuine in-place
    /// replacement; it just isn't what an import is.
    fn import_curl(&mut self, _: &ImportCurl, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            self.set_status("Nothing on the clipboard — copy a curl command first", cx);
            return;
        };

        let import = match zuno_core::curl::parse(&text) {
            Ok(import) => import,
            Err(error) => {
                self.set_status(&format!("Could not import: {error}"), cx);
                return;
            }
        };

        // An import never silently drops part of the command — anything skipped is named.
        let message = if import.ignored.is_empty() {
            format!("Imported {}", import.spec.method.as_str())
        } else {
            format!("Imported — ignored {}", import.ignored.join(", "))
        };

        // The parsed spec carries `RequestId::default()`, which would collide with the
        // buffer already holding id 0.
        let spec = RequestSpec {
            id: self.next_id(cx),
            ..import.spec
        };
        self.open(spec, window, cx);
        // After `open`, so the status lands on the new buffer rather than the one that
        // happened to be in front when the import ran.
        self.set_status(&message, cx);
    }

    fn fold_all(&mut self, _: &FoldAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.set_all_folded(true, cx));
        }
    }

    fn unfold_all(&mut self, _: &UnfoldAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.set_all_folded(false, cx));
        }
    }

    /// Copy the displayed response body to the clipboard.
    ///
    /// **Raw bytes, exactly as the server sent them** — not the pretty-printed outline on
    /// screen. What you paste into a test fixture or a bug report has to be what came back,
    /// and reformatting it would quietly change the thing you're reporting.
    ///
    /// Text only. A response that isn't valid UTF-8 is a normal outcome here (invariant 4),
    /// and the clipboard needs a `String`, so this points at `SaveResponse` instead of
    /// silently copying mojibake.
    fn copy_response(&mut self, _: &CopyResponse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let Some(response) = view.read(cx).displayed().cloned() else {
            self.set_status("No response to copy yet", cx);
            return;
        };

        match response.body_as_str() {
            Some(text) => {
                let size = format_bytes(response.body.len() as u64);
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                self.set_status(&format!("Copied {size} to the clipboard"), cx);
            }
            None => self.set_status(
                "This response isn't text — use Ctrl+Shift+S to save it to a file",
                cx,
            ),
        }
    }

    /// Write the displayed response body to a file the user picks.
    ///
    /// The counterpart to copying rather than a duplicate of it: this is how a binary or
    /// multi-megabyte body gets out, neither of which the clipboard handles usefully.
    fn save_response(&mut self, _: &SaveResponse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let Some(response) = view.read(cx).displayed().cloned() else {
            self.set_status("No response to save yet", cx);
            return;
        };

        let suggested = suggested_filename(&view.read(cx).label(cx), response.content_type());
        // `$HOME` rather than the collection root: a saved response is an artefact you're
        // taking elsewhere, not part of the collection you'd commit.
        let directory = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let prompt = cx.prompt_for_new_path(&directory, Some(&suggested));
        self.response_save = Some(cx.spawn(async move |workspace, cx| {
            // Cancelled, or the platform couldn't open a picker at all.
            let Ok(Ok(Some(path))) = prompt.await else {
                return;
            };

            let body = response.body.clone();
            let write = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path, &body).map(|()| path) });

            let outcome = write.await;
            let _ = workspace.update(cx, |workspace, cx| match outcome {
                Ok(path) => {
                    let shown = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    workspace.set_status(&format!("Saved the response to {shown}"), cx);
                }
                Err(error) => workspace.set_status(&format!("Could not save: {error}"), cx),
            });
        }));
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        cx.global_mut::<Theme>().toggle();
        // A theme change repaints every window, not just this view.
        cx.refresh_windows();
    }

    fn send_request(&mut self, _: &SendRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        let Some(engine) = cx.engine() else {
            self.set_status("The HTTP engine failed to start — restart Zuno", cx);
            return;
        };

        // A send is a natural checkpoint: persist here so a crash costs at most the
        // edits made since the last one. Every buffer is written, not just the one being
        // sent — the checkpoint is the window's state, not this request's.
        //
        // Assembled here and written off-thread. Reading the buffers needs the UI thread, but
        // serializing every open request and blocking on the write does not, and this is the
        // path §8 budgets at 5ms.
        self.session_save = Some(crate::session::save_in_background(self.session(cx), cx));

        // Read from disk per send rather than cached at switch time, so editing an
        // environment file takes effect on the next request. It's a couple of small files;
        // if it ever shows up in a profile, cache it and invalidate on a file watch.
        let resolver = self.resolver(cx);
        view.update(cx, |view, cx| view.send(&engine, &resolver, cx));
    }

    /// Write the active buffer into the collection as a file of its own.
    ///
    /// A buffer that already knows its file overwrites it; one that doesn't gets a name
    /// derived from its URL. That split is the whole reason `RequestView::path` exists — a
    /// derived name is not an identity, so without it a second Ctrl+S would find
    /// `posts.json` taken and write `posts-2.json`.
    ///
    /// Synchronous, like `session::save`: it's a single small write, and a person who
    /// pressed Ctrl+S wants to know it landed before they do anything else. If saving ever
    /// grows to touch a whole tree it belongs on the background executor (invariant 3).
    fn save_request(&mut self, _: &SaveRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            self.set_status("No collection directory — nothing was saved", cx);
            return;
        };

        let spec = view.read(cx).spec(cx);
        let existing = view.read(cx).path.clone();

        let path = match existing {
            Some(path) => path,
            None => {
                let label = view.read(cx).label(cx);
                match collection::allocate(&root, &label) {
                    Ok(path) => path,
                    Err(error) => {
                        self.set_status(&format!("Could not save: {error}"), cx);
                        return;
                    }
                }
            }
        };

        if let Err(error) = collection::write(&path, &spec) {
            self.set_status(&format!("Could not save: {error}"), cx);
            return;
        }

        // Report the path relative to the root: the absolute path is mostly the same
        // prefix every time, and the part that identifies the request is the tail.
        let shown = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
        view.update(cx, |view, cx| {
            view.path = Some(path);
            cx.notify();
        });
        self.set_status(&format!("Saved to {shown}"), cx);
    }

    /// `on_app_quit` does the saving, so this only has to ask the app to quit — one save
    /// path instead of one per exit route.
    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn cancel_request(&mut self, _: &CancelRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let Some(engine) = cx.engine() else { return };

        let cancelled = view.update(cx, |view, cx| view.cancel(&engine, cx));
        if cancelled {
            self.set_status("Cancelled", cx);
        }
    }

    fn set_status(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            let message = SharedString::from(message.to_string());
            view.update(cx, |view, cx| {
                view.status = Some(message);
                cx.notify();
            });
        }
    }


    fn focused_region(&self, window: &Window, cx: &App) -> SharedString {
        let Some(view) = self.views.get(self.active_ix) else {
            return SharedString::from("—");
        };
        let view = view.read(cx);

        if view.url_focus(cx).is_focused(window) {
            return SharedString::from("URL");
        }
        if view.body_focus(cx).is_focused(window) {
            return SharedString::from("Body");
        }
        if view.response_focus.is_focused(window) {
            return SharedString::from("Response");
        }
        if let Some((kind, ix)) = view.focused_row(window, cx) {
            let label = match kind {
                RowKind::Header => "header",
                RowKind::Query => "query",
                RowKind::Form => "form field",
                RowKind::Multipart => "part",
            };
            return SharedString::from(format!("{label} row {}", ix + 1));
        }
        SharedString::from("Window")
    }

    fn status_message(&self, cx: &App) -> Option<SharedString> {
        self.views.get(self.active_ix)?.read(cx).status.clone()
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let focused_region = self.focused_region(window, cx);
        let status_message = self.status_message(cx);
        let cookies = self.cookies_enabled(cx);
        let title = self
            .views
            .get(self.active_ix)
            .map(|view| view.read(cx).label(cx))
            .unwrap_or_else(|| SharedString::from("No request"));

        // Collected before building elements: the closures below borrow `cx` mutably, so
        // the labels can't be read from the views while they're alive.
        let tabs: Vec<(usize, SharedString, bool)> = self
            .views
            .iter()
            .enumerate()
            .map(|(ix, view)| (ix, view.read(cx).label(cx), ix == self.active_ix))
            .collect();

        // The window title tracks the request, so the taskbar entry is useful even
        // though we draw our own titlebar. Only written when it changes — this runs every
        // frame.
        let window_title = format!("{title} — Zuno");
        if self.window_title != window_title {
            window.set_window_title(&window_title);
            self.window_title = window_title;
        }

        div()
            .id("zuno")
            .key_context("Zuno")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_url))
            .on_action(cx.listener(Self::focus_body))
            .on_action(cx.listener(Self::focus_response))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_action(cx.listener(Self::open_request))
            .on_action(cx.listener(Self::switch_environment))
            .on_action(cx.listener(Self::show_history))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::setting_next))
            .on_action(cx.listener(Self::setting_prev))
            .on_action(cx.listener(Self::setting_increase))
            .on_action(cx.listener(Self::setting_decrease))
            .on_action(cx.listener(Self::setting_confirm))
            .on_action(cx.listener(Self::settings_dismiss))
            .on_action(cx.listener(Self::clear_cookies))
            .on_action(cx.listener(Self::picker_next))
            .on_action(cx.listener(Self::picker_prev))
            .on_action(cx.listener(Self::picker_confirm))
            .on_action(cx.listener(Self::picker_dismiss))
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::open_method))
            .on_action(cx.listener(Self::add_header))
            .on_action(cx.listener(Self::add_query))
            .on_action(cx.listener(Self::toggle_row))
            .on_action(cx.listener(Self::remove_row))
            .on_action(cx.listener(Self::open_body_type))
            .on_action(cx.listener(Self::add_form_field))
            .on_action(cx.listener(Self::choose_body_file))
            .on_action(cx.listener(Self::add_multipart_field))
            .on_action(cx.listener(Self::import_curl))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::fold_all))
            .on_action(cx.listener(Self::unfold_all))
            .on_action(cx.listener(Self::copy_response))
            .on_action(cx.listener(Self::save_response))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::save_request))
            .on_action(cx.listener(Self::send_request))
            .on_action(cx.listener(Self::cancel_request))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .text_sm()
            .relative()
            .child(crate::chrome::titlebar(title, &theme, window))
            .children(tab_strip(tabs, &theme, cx))
            .children(self.active())
            .child(status_bar(
                focused_region,
                status_message,
                cookies,
                self.environment.clone().map(SharedString::from),
                &theme,
            ))
            // Above the panes, below the resize edges.
            .children(self.picker.as_ref().map(|state| state.picker.clone()))
            .children(self.settings.as_ref().map(|state| state.panel.clone()))
            // Last, so the edge strips sit above the panes for hit-testing.
            .children(crate::chrome::resize_handles(window))
    }

}


/// The strip of open buffers.
///
/// Hidden entirely at one buffer: a single tab is a row of chrome that says nothing, and
/// the window title already names the request. It appears the moment there's a choice to
/// make, which is also the moment it starts carrying information.
fn tab_strip(
    tabs: Vec<(usize, SharedString, bool)>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> Option<impl IntoElement> {
    if tabs.len() < 2 {
        return None;
    }

    Some(
        div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .w_full()
            // Many tabs must scroll rather than squeeze every label into illegibility.
            // `overflow_x_scroll` lives on `StatefulInteractiveElement`, hence the `.id()`.
            .overflow_x_scroll()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .children(tabs.into_iter().map(|(ix, label, active)| {
                div()
                    .id(("tab", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_none()
                    .max_w(px(180.))
                    // A long label must clip, not push its neighbours off the strip.
                    .overflow_hidden()
                    .px_3()
                    .py_1()
                    .border_r_1()
                    .border_color(theme.border)
                    // The active tab is marked by a top rule in the accent colour rather
                    // than by text weight: reflowing on switch would shift every label.
                    .border_t_2()
                    .border_color(if active { theme.accent } else { theme.bg_panel })
                    .bg(if active { theme.bg } else { theme.bg_panel })
                    .text_xs()
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.bg_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                            workspace.activate(ix, window, cx);
                        }),
                    )
                    // Middle-click closes, the convention every browser and editor shares.
                    // It closes *that* tab, not the active one, so it activates first.
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                            workspace.activate(ix, window, cx);
                            workspace.close_tab(&CloseTab, window, cx);
                        }),
                    )
                    .child(label)
            })),
    )
}

/// A filename to offer in the save dialog: the request's name plus an extension matching
/// what came back.
///
/// Runs the label through `collection::slug` for the same reason saving a request does — the
/// label derives from a URL, so `https://x.test/../../.ssh/config` must not become a path.
pub fn suggested_filename(label: &str, content_type: Option<&str>) -> String {
    // Match on the essence only: `application/json; charset=utf-8` is still JSON.
    let base = content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("");

    let extension = match base {
        "application/json" | "text/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "text/html" => "html",
        "text/csv" => "csv",
        other if other.starts_with("text/") => "txt",
        // Anything else could be an image, a protobuf, a zip. `.bin` claims nothing.
        _ => "bin",
    };

    format!("{}.{extension}", collection::slug(label))
}

/// Bytes at human scale. The history picker shows sizes side by side, and `184320` next to
/// `179` reads as noise where `180 KB` next to `179 B` reads as a difference.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    match bytes {
        n if n >= MB => format!("{:.1} MB", n as f64 / MB as f64),
        n if n >= KB => format!("{:.1} KB", n as f64 / KB as f64),
        n => format!("{n} B"),
    }
}

/// Offer the typed text as a custom HTTP verb, when it could be one.
///
/// `Method::Other` has always been sendable — `build_method` hands it to
/// `http::Method::from_bytes`, and `core` has tests for it — but nothing in the UI could
/// produce one. This is that path, and it's why the method picker is a filtered list rather
/// than a fixed dropdown.
///
/// Returns `None` rather than offering a row that would fail: the engine rejects anything
/// outside RFC 9110's `tchar` set with `InvalidMethod`, and offering `Use "foo bar"` only to
/// fail at send is worse than not offering it.
fn custom_method_row(query: &str) -> Option<picker::Item> {
    let verb = query.trim();
    if verb.is_empty() || !verb.bytes().all(is_tchar) {
        return None;
    }

    // Case-sensitive on the wire, but conventionally uppercase, and nobody typing `purge`
    // means a lowercase verb.
    let verb = verb.to_ascii_uppercase();
    // A known verb already has a row; a second would set `Other("GET")` instead of `Get`,
    // which is the same request but a different value everything downstream compares.
    if zuno_core::Method::common()
        .iter()
        .any(|known| known.as_str() == verb)
    {
        return None;
    }

    Some(picker::Item {
        label: SharedString::from(format!("Use \"{verb}\"")),
        detail: SharedString::from("custom method"),
        target: picker::Target::Method(zuno_core::Method::Other(verb)),
    })
}

/// RFC 9110's `tchar`: what an HTTP method token may contain.
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// The first keybinding for an action, rendered like `ctrl-k`, or empty if it has none.
///
/// Read from the live keymap rather than a hardcoded string, so a rebinding can't leave a piece of
/// UI copy advertising a shortcut that no longer works. Shared with `response_pane`'s in-flight
/// hint, which said "Ctrl+C or Escape to cancel" for a while — `ctrl-c` has only ever been bound to
/// `text_input::Copy`, so half of that sentence was telling people to press a key that does
/// nothing to a request.
pub fn keybinding_hint(action: &dyn gpui::Action, window: &Window) -> String {
    window
        .bindings_for_action(action)
        .first()
        .map(|binding| {
            binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn status_bar(
    focused_region: SharedString,
    message: Option<SharedString>,
    cookies: bool,
    environment: Option<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    const HINTS: &str = "Ctrl+P find · Ctrl+K commands · Ctrl+E env · Ctrl+Enter send";

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .flex_none()
        .px_3()
        .py_1()
        .bg(theme.bg_panel)
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w(gpui::px(0.))
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.accent)
                        .child(format!("focus: {focused_region}")),
                )
                .children(message.map(|message| {
                    div()
                        .flex_1()
                        .min_w(gpui::px(0.))
                        .truncate()
                        .text_color(theme.text_muted)
                        .child(message)
                })),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .flex_none()
                // The cookie jar is on by default and was otherwise invisible, which made
                // consecutive requests silently non-independent — the exact thing that
                // costs an hour of debugging a phantom auth bug. Shown only when it's on:
                // a badge that's always there stops being read.
                // Which environment a request will be sent against changes where it
                // goes and what credentials it carries, so it belongs on screen rather than
                // two keystrokes away. No badge at all means no substitution — a quieter
                // way of saying it than a badge reading "None".
                .children(environment.map(|name| {
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.bg_elevated)
                        .text_color(theme.accent)
                        .child(name)
                }))
                .children(cookies.then(|| {
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.bg_elevated)
                        .text_color(theme.accent)
                        .child("cookies on")
                }))
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.text_muted)
                        .child(HINTS.to_string()),
                ),
        )
}
