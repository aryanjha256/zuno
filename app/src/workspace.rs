//! The root view. Owns the buffers, hosts every application action handler, and
//! draws the chrome around the panes.
//!
//! Action handlers live here rather than on `RequestView` on purpose: dispatch
//! travels up the focus tree, and `Workspace` is the one element guaranteed to be on
//! that path no matter which region holds focus — including when focus is inside a
//! `TextInput` nested two levels down. Handlers that need buffer state reach into the
//! active `RequestView` through its entity.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    ClipboardItem, Styled, Subscription, Task, UniformListScrollHandle, Window, div, px,
};
use zuno_core::{
    Environment, RawKind, RequestId, RequestSpec, Resolver, collection, curl, environment,
};
use zuno_core::collection::{Node, NodeKind};

use crate::actions::{
    AddFormField, AddHeader, AddMultipartField, AddQuery, CancelRequest, ChooseBodyFile,
    ClearCookies, CloseTab, CopyResponse, CopyRowPath, CopyRowValue, MenuConfirm, MenuDismiss,
    MenuNext, MenuPrev, OpenRowMenu, ResponseRowNext, ResponseRowPrev, ScrollLeft, ScrollRight,
    ScrollStart, ToggleFold, FocusBody, FocusNext, FocusPrev, FocusResponse, FocusUrl, FoldAll, ImportCurl, NewTab, NextRequestTab, NextTab,
    OpenBodyType, PrevRequestTab, OpenMethod, OpenPalette, OpenRequest, OpenSettings, PickerConfirm, PickerDismiss,
    OpenAppMenu, PickerNext, PickerPrev, PrevTab, Quit, RemoveRow, SaveRequest, SaveResponse, SendRequest,
    SettingConfirm, SettingDecrease, SettingIncrease, SettingNext, SettingPrev, SettingsDismiss,
    BodyFindNext, BodyFindPrev, CloseBodyFind, CloseFind, CopyAsCurl, FindInBody,
    FindInResponse, FindNext, FindPrev, ReplaceAll, ReplaceNext,
    ShowBodyTab, ShowHeadersTab, ShowHistory, ShowParamsTab, SwitchEnvironment, ToggleResponseView, ToggleRow, ToggleTheme, UnfoldAll,
    CollectionCollapse, CollectionConfirm, CollectionExpand, CollectionNext, CollectionPrev,
    ConfirmDeleteRequest, DeleteRequest, OpenCollectionMenu, ToggleCollectionPanel,
    CancelClose, CancelRename, CloseChoiceNext, CloseChoicePrev, CollectionCollapseAll,
    ForgetWorkspace, NewWorkspace, OpenWorkspace, OpenWorkspaceMenu, SwitchWorkspace,
    WorkspaceBrowse,
    WorkspaceConfirm, WorkspaceDismiss,
    CollectionExpandAll, CommitRename, ConfirmClose, CopyRequestPath,
    CopyRequestRelativePath, DuplicateRequest,
    ImportConfirm, ImportDismiss, ImportOpenApi, MoveRequest, NewFolder, OpenRequestExternally,
    RenameRequest, RevealRequest, TrashRequest,
};
use crate::engine::ActiveEngine;
use crate::context_menu;
use crate::picker;
use crate::settings_panel::{SettingsEvent, SettingsPanel};
use crate::request_view::{BodyType, RequestTab, RequestView, RowKind};
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
    /// The row context menu, while it's open. Owned here rather than by the view that opened
    /// it for two reasons: `modal_open` has to be able to see it, and the response pane is
    /// `overflow_hidden`, which would clip a menu near its bottom edge.
    menu: Option<MenuState>,
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

    // --- The collection panel. See `collection_panel.rs`.
    /// Every row in the collection, whatever is folded. Rebuilt by `refresh_tree`.
    pub(crate) tree: Vec<Node>,
    /// Indices into `tree` that are currently drawn. The `JsonOutline::visible` split, for
    /// the same reason: folding is view state, so the tree itself stays whole.
    pub(crate) tree_visible: Vec<usize>,
    /// Directories the reader has collapsed, by path rather than by name — two directories
    /// at different depths can share a name and must fold independently.
    pub(crate) collapsed: HashSet<PathBuf>,
    /// Whether a scan has completed. Distinguishes "nothing saved" from "still reading",
    /// which are the same empty list and want different words on screen.
    pub(crate) tree_scanned: bool,
    /// Ordinary files the last scan passed over, so the empty state can tell "this directory
    /// holds other things" from "you have not saved anything yet". Open a folder of images and
    /// the old message claimed the second while the truth was the first.
    pub(crate) tree_skipped: usize,
    pub(crate) panel_visible: bool,
    /// **An index into `tree`, not into `tree_visible`.** Folding rewrites `tree_visible`
    /// underneath the selection, so a visible index would silently retarget it at whatever
    /// row slid into that slot — the lesson the response viewer's row cursor already records
    /// (architecture.md §6). Translation happens at render and scroll, nowhere else.
    pub(crate) panel_selection: Option<usize>,
    pub(crate) panel_scroll: UniformListScrollHandle,
    /// Deliberately **not** a tab stop, unlike `response_focus`. `Tab` currently walks the
    /// active request's inputs, and a pane-level stop painted before all of them would turn
    /// the first `Tab` from "url → method" into "panel → url" for every existing user. The
    /// panel has its own binding and a click target, so it loses nothing by staying out.
    pub(crate) panel_focus: FocusHandle,
    /// Holding the task is what keeps the scan alive; dropping it cancels.
    tree_scan: Option<Task<()>>,
    /// The row being renamed in place, while it is being renamed.
    ///
    /// Inline rather than a modal, because that is what a tree does everywhere else — and it
    /// meant no new primitive: a `TextInput` drawn in the row's own place, carrying its own key
    /// context so `Enter` and `Escape` mean commit and cancel *there* without touching what they
    /// mean anywhere else.
    renaming: Option<RenameState>,
    /// The pending new folder, while its name is being typed.
    new_folder: Option<NewFolderState>,
    /// The OpenAPI import modal, while it's open.
    import: Option<ImportState>,
    /// Holding the task is what keeps a spec fetch alive; dropping it cancels.
    import_task: Option<Task<()>>,
    /// Where the panel was right-clicked, in window coordinates.
    ///
    /// Kept rather than taken when the first menu opens, because the confirmation is a *second*
    /// menu that has to appear in the same place — a "delete this?" that jumps across the
    /// window reads as a different question about something else.
    collection_menu_at: Option<gpui::Point<gpui::Pixels>>,
    /// The unsaved-changes prompt, when one is open.
    close_confirm: Option<crate::close_panel::CloseConfirm>,
    new_workspace_panel: Option<WorkspacePanelState>,
    /// The folder dialog behind New and Open. Held because dropping the task cancels it.
    workspace_prompt: Option<Task<()>>,
}

/// An in-progress inline rename.
struct RenameState {
    /// Index into `tree`, so the row can be found again after a repaint. The *path* is what the
    /// rename acts on, because a rescan can land between opening the box and committing.
    row_ix: usize,
    path: PathBuf,
    input: Entity<crate::input::TextInput>,
    /// Cancel-on-blur. Clicking elsewhere has to end the rename, or the box stays on screen
    /// with nothing focused and the next `Enter` commits an edit the user had walked away from.
    /// Cancel rather than commit, unlike VS Code: a rename is a file operation, and the safe
    /// reading of "clicked somewhere else" is that it was not meant.
    _blur: Subscription,
}

/// The import modal and the subscription that lets it report and close, paired for the reason
/// `PickerState` pairs them: either alone leaves a modal nothing can dismiss.
struct ImportState {
    panel: Entity<crate::import_panel::ImportPanel>,
    _subscription: Subscription,
}

/// Same pairing again: the panel and the subscription that lets it be closed.
struct WorkspacePanelState {
    panel: Entity<crate::workspace_panel::WorkspacePanel>,
    _subscription: Subscription,
}

/// A folder being named.
///
/// Deliberately **not** a phantom row spliced into the tree. `tree_visible` indexes into `tree`,
/// so a row that exists only in the UI would have to be threaded through the fold walk, the
/// selection clamp and every index translation to serve one transient input. This is a single
/// input drawn under the panel's header, labelled with where the folder will land — which also
/// says the destination outright instead of asking the reader to count indent levels.
struct NewFolderState {
    parent: PathBuf,
    /// Where the box sits in the list, as a *visible* index — the row it displaces.
    insert_at: usize,
    /// One level deeper than its parent, so it lines up with the folder's future contents.
    depth: u16,
    input: Entity<crate::input::TextInput>,
    _blur: Subscription,
}

/// The settings panel and the subscription that lets it close, for the same reason
/// `PickerState` pairs them: either alone leaves a modal nothing can dismiss.
struct SettingsState {
    panel: Entity<SettingsPanel>,
    _subscription: Subscription,
}

/// Same pairing as `PickerState`, for the same reason.
struct MenuState {
    menu: Entity<context_menu::ContextMenu>,
    _subscription: Subscription,
}

/// The picker plus the subscription that lets it be closed. Dropping either without the
/// other would leave a modal nothing can dismiss, so they live and die together.
struct PickerState {
    picker: Entity<picker::Picker>,
    _subscription: Subscription,
}

/// Expand a leading `~`, since the location field is typed by hand and a bare `~/code` would
/// otherwise become a directory literally named `~`.
fn shellexpand_home(input: &str) -> String {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix('~') else {
        return trimmed.to_string();
    };
    let Some(home) = std::env::var_os("HOME") else {
        return trimmed.to_string();
    };
    format!("{}{rest}", home.to_string_lossy())
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
        let session_panel = session.collection_panel;
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

        let mut workspace = Self {
            focus_handle: cx.focus_handle(),
            window_title: String::new(),
            _quit_subscription: quit_subscription,
            views,
            active_ix,
            picker: None,
            picker_scan: None,
            settings: None,
            menu: None,
            response_save: None,
            session_save: None,
            body_file_prompt: None,
            environment,
            tree: Vec::new(),
            tree_visible: Vec::new(),
            collapsed: HashSet::new(),
            tree_scanned: false,
            tree_skipped: 0,
            panel_visible: session_panel,
            panel_selection: None,
            panel_scroll: UniformListScrollHandle::new(),
            panel_focus: cx.focus_handle(),
            tree_scan: None,
            renaming: None,
            new_folder: None,
            import: None,
            import_task: None,
            collection_menu_at: None,
            close_confirm: None,
            new_workspace_panel: None,
            workspace_prompt: None,
        };

        // Off-thread and non-blocking, so a large collection cannot delay the first frame —
        // the panel opens empty and fills in, the same bargain the picker's scan makes.
        workspace.refresh_tree(cx);
        workspace.reread_baselines(cx);
        workspace
    }

    /// Re-read each restored buffer's file, so `is_dirty` has a real baseline again.
    ///
    /// The session envelope stores each buffer's *live* spec, edits included, and has never
    /// stored what the file said. So a restored buffer starts with baseline == live and reads
    /// clean; this corrects it from disk a moment later. Clean-until-corrected is the right way
    /// round — the alternative marks every tab dirty on launch until the reads land.
    ///
    /// Read rather than persisted, and that is the point rather than a saving: a stored baseline
    /// records what the file said when you *quit*, so a `git pull` while Zuno was closed would
    /// leave a buffer reading clean against a file it no longer matches. The disk is the truth.
    ///
    /// Off the UI thread per invariant 3, even though the files are small and few.
    fn reread_baselines(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<_> = self
            .views
            .iter()
            .enumerate()
            .filter_map(|(ix, view)| view.read(cx).path.clone().map(|path| (ix, path)))
            .collect();

        if paths.is_empty() {
            return;
        }

        cx.spawn(async move |workspace, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .into_iter()
                        .filter_map(|(ix, path)| {
                            // A file deleted or made unreadable while Zuno was closed leaves the
                            // buffer with its session baseline. It reads clean, which matches how
                            // a buffer with no file at all behaves.
                            collection::read(&path).ok().map(|spec| (ix, spec))
                        })
                        .collect::<Vec<_>>()
                })
                .await;

            workspace
                .update(cx, |workspace, cx| {
                    for (ix, spec) in read {
                        let Some(view) = workspace.views.get(ix) else {
                            continue;
                        };
                        view.update(cx, |view, cx| {
                            // The file's id is 0 (invariant 9) and `is_dirty` ignores it, so it
                            // is stored as read rather than patched.
                            view.baseline = spec;
                            cx.notify();
                        });
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Point the window at a different workspace.
    ///
    /// **The current session is written first, synchronously.** It has to land before the
    /// globals move — `session::save` writes to whatever `SessionFile` currently holds, so
    /// re-resolving first would file this workspace's buffers under the next one's id. That is
    /// also why switching needs no unsaved-changes prompt: every open buffer's live spec goes
    /// into the session, edits included, and comes back when you switch back.
    pub fn switch_workspace(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if crate::app_state::active_id(cx).as_deref() == Some(id) {
            return;
        }

        self.session_save = None;
        crate::session::save(&self.session(cx), cx);

        if !crate::app_state::set_active(cx, id) {
            return;
        }

        self.reload_active_workspace(window, cx);
    }

    /// Rebuild the window from whatever workspace is now active.
    ///
    /// Split from `switch_workspace` because forgetting the active workspace re-resolves the
    /// globals without going through `set_active`, and the window still has to follow.
    fn reload_active_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = crate::session::load(cx)
            .unwrap_or_else(|| crate::session::Session::single(RequestSpec::default()));

        self.views = session
            .tabs
            .into_iter()
            .map(|tab| {
                cx.new(|cx| {
                    let mut view = RequestView::new(tab.spec, cx);
                    view.path = tab.path;
                    view
                })
            })
            .collect();
        self.environment = session.environment;
        self.panel_visible = session.collection_panel;

        // Every handle into the old buffers is dead, so focus has to move or the keymap goes
        // with them. `activate` is the one funnel that does both.
        self.active_ix = session.active.min(self.views.len().saturating_sub(1));
        self.activate(self.active_ix, window, cx);

        self.collapsed.clear();
        self.panel_selection = None;
        self.tree_scanned = false;
        self.refresh_tree(cx);
        self.reread_baselines(cx);
        cx.notify();
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
        self.picker.is_some()
            || self.settings.is_some()
            || self.menu.is_some()
            || self.import.is_some()
            || self.close_confirm.is_some()
            || self.new_workspace_panel.is_some()
    }

    #[cfg(test)]
    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
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

    /// The panel's rows as `(depth, name, is_directory)`, for tests.
    ///
    /// Reads the real state rather than the last painted frame: `cx.debug_bounds` reports what
    /// was drawn *previously*, so `is_none()` proves nothing about a row that has just been
    /// folded away — four context-menu tests already made that mistake.
    #[cfg(test)]
    pub(crate) fn tree_rows(&self) -> Vec<(u16, String, bool)> {
        self.tree_visible
            .iter()
            .filter_map(|&ix| self.tree.get(ix))
            .map(|node| {
                (
                    node.depth,
                    node.name.clone(),
                    matches!(node.kind, NodeKind::Directory),
                )
            })
            .collect()
    }

    /// The selected row's name, for tests. `None` when nothing is selected.
    #[cfg(test)]
    pub(crate) fn tree_selection(&self) -> Option<String> {
        self.tree.get(self.panel_selection?).map(|node| node.name.clone())
    }

    /// The open menu's rows as `(label, keystroke)`, for tests.
    #[cfg(test)]
    pub(crate) fn menu_details(&self, cx: &App) -> Vec<(String, String)> {
        self.menu
            .as_ref()
            .map(|state| state.menu.read(cx).row_details())
            .unwrap_or_default()
    }

    /// The open context menu's rows, for tests.
    #[cfg(test)]
    pub(crate) fn menu_labels(&self, cx: &App) -> Vec<String> {
        self.menu
            .as_ref()
            .map(|state| state.menu.read(cx).row_labels())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn panel_is_visible(&self) -> bool {
        self.panel_visible
    }

    /// One `(index, label, is_active)` per open buffer. A method, not a closure in `render`, so
    /// a test can read the elided label — shaped text is not measurable headlessly.
    pub(crate) fn tab_labels(&self, cx: &App) -> Vec<(usize, SharedString, bool, bool)> {
        self.views
            .iter()
            .enumerate()
            .map(|(ix, view)| {
                let label = view.read(cx).label(cx);
                let label = match zuno_core::request::elide(&label, TAB_LABEL_CHARS) {
                    std::borrow::Cow::Borrowed(_) => label,
                    std::borrow::Cow::Owned(short) => SharedString::from(short),
                };
                (ix, label, ix == self.active_ix, view.read(cx).is_dirty(cx))
            })
            .collect()
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
    /// Set by a click, which both chooses and confirms; the selection exists for the keyboard.
    pub(crate) fn set_close_choice(&mut self, choice: crate::close_panel::Choice) {
        if let Some(state) = self.close_confirm.as_mut() {
            state.choice = choice;
        }
    }

    fn close_choice_next(&mut self, _: &CloseChoiceNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.close_confirm.as_mut() {
            state.step(1);
            cx.notify();
        }
    }

    fn close_choice_prev(&mut self, _: &CloseChoicePrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.close_confirm.as_mut() {
            state.step(-1);
            cx.notify();
        }
    }

    fn cancel_close(&mut self, _: &CancelClose, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.close_confirm.take() else {
            return;
        };
        if let Some(focus) = state.restore_focus {
            window.focus(&focus);
        }
        cx.notify();
    }

    fn confirm_close(&mut self, _: &ConfirmClose, window: &mut Window, cx: &mut Context<Self>) {
        use crate::close_panel::Choice;
        let Some(state) = self.close_confirm.take() else {
            return;
        };

        // `force_close_tab` and `save_request` both act on the *active* buffer, and the prompt
        // records the one it was opened for. A modal owns the keyboard, so the two cannot
        // diverge — this refuses rather than trusting that, since acting on the wrong buffer
        // here would discard a request nobody was asked about.
        if self.active_ix != state.ix {
            cx.notify();
            return;
        }

        match state.choice {
            Choice::Cancel => {
                if let Some(focus) = state.restore_focus {
                    window.focus(&focus);
                }
                cx.notify();
            }
            Choice::Save => {
                // Taken *before* saving: `save_request` acts on the active buffer, and the
                // prompt is modal, so that is still the one being closed.
                self.save_request(&SaveRequest, window, cx);
                // Saving can fail — no collection directory, an unwritable file — and it says
                // so in the status bar. Closing anyway would discard the work the person just
                // asked to keep, so the buffer stays and the message stands.
                let saved = self
                    .active()
                    .is_some_and(|view| !view.read(cx).is_dirty(cx));
                if saved {
                    self.force_close_tab(window, cx);
                } else {
                    cx.notify();
                }
            }
            Choice::Discard => self.force_close_tab(window, cx),
        }
    }

    /// Ask before discarding. **This closes nothing** — `force_close_tab` is the only thing
    /// that does, the same split `DeleteRequest`/`ConfirmDeleteRequest` uses and for the same
    /// reason: closing a tab is irreversible, and quitting is not, because the session envelope
    /// keeps every open buffer while `Ctrl+W` kept none.
    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() || self.modal_open() {
            return;
        }

        let dirty = self
            .active()
            .is_some_and(|view| view.read(cx).is_dirty(cx));

        if dirty {
            let label = self
                .active()
                .map(|view| view.read(cx).label(cx))
                .unwrap_or_else(|| SharedString::from("This request"));
            let restore = Some(window.focused(cx).unwrap_or_else(|| self.focus_handle.clone()));
            let state =
                crate::close_panel::CloseConfirm::new(self.active_ix, label, restore, cx);
            let focus = state.focus_handle.clone();
            self.close_confirm = Some(state);
            window.focus(&focus);
            cx.notify();
            return;
        }

        self.force_close_tab(window, cx);
    }

    fn force_close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

        // The hint names a keystroke, so it is built rather than written. `keybinding_label`
        // returns empty for an unbound action, and a sentence with a hole in it is worse than a
        // shorter sentence — hence the match rather than an interpolation.
        let save_hint = match keybinding_label(&SaveRequest, window) {
            key if key.is_empty() => "No saved requests yet".to_string(),
            key => format!("No saved requests yet — press {key} to save the one you're editing"),
        };
        let picker = self.show_picker(buffer_items, save_hint, window, cx);

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

    // --- The collection panel -------------------------------------------------------------

    /// The collection root's own directory name, for the panel's title strip.
    pub(crate) fn collection_name(&self, cx: &App) -> Option<SharedString> {
        let root = crate::collections::root(cx)?;
        root.file_name()
            .map(|name| SharedString::from(name.to_string_lossy().to_string()))
    }

    /// Re-read the collection into `tree`.
    ///
    /// Scans and builds off-thread (invariant 3): `scan` reads and parses every request file,
    /// which for a large collection is real work and must never sit on the UI thread. Called
    /// at startup, when the panel is shown, and after a save — a request you just wrote and
    /// cannot see in the tree reads as a save that failed.
    pub(crate) fn refresh_tree(&mut self, cx: &mut Context<Self>) {
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            self.tree.clear();
            self.tree_skipped = 0;
            self.tree_scanned = true;
            self.rebuild_tree_visible();
            return;
        };

        let scan = cx.background_executor().spawn(async move {
            let (entries, skipped) = collection::scan_counted(&root);
            // Two walks rather than one, and worth the second: a directory earns a row by
            // existing, so an empty folder has to come from the filesystem rather than be
            // inferred from the requests inside it.
            let folders = collection::folders(&root);
            (collection::tree(&root, &entries, &folders), skipped)
        });

        // **The selection is restored by path, not by index**, and that distinction is the
        // whole point of capturing it here. `panel_selection` indexes into `tree`, and a rescan
        // replaces `tree` wholesale — so saving a request that happens to sort earlier shifts
        // every row after it and the same index silently means a *different* request. Nothing
        // on screen would say so.
        let selected = self
            .panel_selection
            .and_then(|ix| self.tree.get(ix))
            .map(|node| node.path.clone());

        self.tree_scan = Some(cx.spawn(async move |this, cx| {
            let (nodes, skipped) = scan.await;
            this.update(cx, |this, cx| {
                this.tree = nodes;
                this.tree_skipped = skipped;
                this.tree_scanned = true;
                // Gone from disk since the last scan means gone from the panel: no selection
                // rather than a neighbouring row nobody asked for.
                this.panel_selection = selected
                    .and_then(|path| this.tree.iter().position(|node| node.path == path));
                this.rebuild_tree_visible();
                cx.notify();
            })
            .ok();
        }));
    }

    /// Recompute which rows are drawn, and drop a selection that is no longer one of them.
    ///
    /// **The one funnel**, like `BodyView::rebuild_visible`: every mutation of `tree` or
    /// `collapsed` comes through here, so the check cannot be forgotten by one caller.
    fn rebuild_tree_visible(&mut self) {
        let mut visible = Vec::with_capacity(self.tree.len());
        // While set, every row deeper than this belongs to a collapsed subtree.
        let mut hidden_below: Option<u16> = None;

        for (ix, node) in self.tree.iter().enumerate() {
            if let Some(depth) = hidden_below {
                if node.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            visible.push(ix);
            if matches!(node.kind, NodeKind::Directory) && self.collapsed.contains(&node.path) {
                hidden_below = Some(node.depth);
            }
        }
        self.tree_visible = visible;

        // **No clamp here, deliberately.** The response viewer needs one because a fold can
        // hide the row its cursor is on; this cannot. Both fold paths — the click and
        // `CollectionCollapse` — select the directory *before* folding it, and `refresh_tree`
        // re-resolves the selection by path and yields `None` when it is gone. A guard was
        // written for it and deleted, because breaking it on purpose changed no test: it was
        // unreachable. Four other guards in this codebase went the same way (architecture.md
        // §6). **A new fold path must select the directory first**, or this comment is what
        // it was supposed to warn you about.
    }

    /// Show or hide the panel.
    ///
    /// Three states rather than a bare toggle, matching every editor's sidebar binding: hidden
    /// shows and focuses, visible-but-elsewhere focuses, and visible-and-focused hides. A bare
    /// toggle would make the binding *dismiss* the panel whenever you wanted to reach it.
    fn toggle_collection_panel(
        &mut self,
        _: &ToggleCollectionPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.panel_visible {
            self.panel_visible = true;
            // Cheap, and a collection edited outside Zuno is the normal case — it is a git
            // directory, so it changes under us on every pull.
            self.refresh_tree(cx);
            window.focus(&self.panel_focus);
            cx.notify();
            return;
        }

        if !self.panel_focus.is_focused(window) {
            window.focus(&self.panel_focus);
            cx.notify();
            return;
        }

        self.panel_visible = false;
        // **Focus has to leave with it.** A `FocusHandle` stays focusable whether or not its
        // element is painted, but action dispatch walks *up the focus tree* — so leaving focus
        // on an unpainted panel means no path reaches `Workspace` and every binding in the app
        // silently stops resolving. Same failure as switching `active_ix` without moving focus.
        self.activate(self.active_ix, window, cx);
        cx.notify();
    }

    /// Step the selection by one visible row. `None` starts at the top.
    fn step_collection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.tree_visible.is_empty() {
            return;
        }

        let current = self
            .panel_selection
            .and_then(|ix| self.tree_visible.iter().position(|&v| v == ix));

        let next = match current {
            None if delta > 0 => 0,
            None => self.tree_visible.len() - 1,
            Some(pos) => pos.saturating_add_signed(delta).min(self.tree_visible.len() - 1),
        };

        self.panel_selection = self.tree_visible.get(next).copied();
        if let Some(pos) = self.panel_selection.and_then(|ix| {
            self.tree_visible.iter().position(|&v| v == ix)
        }) {
            // `uniform_list` addresses items by *visible* index, so the row index has to be
            // translated — with anything folded above the target the two diverge.
            self.panel_scroll.scroll_to_item(pos, gpui::ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn collection_next(&mut self, _: &CollectionNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_collection(1, cx);
    }

    fn collection_prev(&mut self, _: &CollectionPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step_collection(-1, cx);
    }

    fn collection_confirm(
        &mut self,
        _: &CollectionConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.panel_selection else { return };
        self.choose_collection_row(ix, window, cx);
    }

    /// `left`: close the directory you are on, or step out to the parent of the row you are on.
    ///
    /// The second half is what makes `left` useful on a request row, where there is nothing to
    /// close — without it the key is dead on every leaf, which reads as broken.
    fn collection_collapse(
        &mut self,
        _: &CollectionCollapse,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.panel_selection else { return };
        let Some(node) = self.tree.get(ix) else { return };

        let open_directory = matches!(node.kind, NodeKind::Directory)
            && !self.collapsed.contains(&node.path);

        if open_directory {
            self.collapsed.insert(node.path.clone());
            self.rebuild_tree_visible();
            cx.notify();
            return;
        }

        // Step to the parent: the nearest earlier row one level shallower.
        let depth = node.depth;
        if depth == 0 {
            return;
        }
        if let Some(parent) = self.tree[..ix]
            .iter()
            .rposition(|candidate| candidate.depth == depth - 1)
        {
            self.panel_selection = Some(parent);
            cx.notify();
        }
    }

    /// `right`: open the directory you are on, or step into it if it is already open.
    fn collection_expand(&mut self, _: &CollectionExpand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.panel_selection else { return };
        let Some(node) = self.tree.get(ix) else { return };
        if !matches!(node.kind, NodeKind::Directory) {
            return;
        }

        if self.collapsed.remove(&node.path) {
            self.rebuild_tree_visible();
        } else if self.tree.get(ix + 1).is_some_and(|next| next.depth > node.depth) {
            self.panel_selection = Some(ix + 1);
        }
        cx.notify();
    }

    /// Collapse every directory in the tree.
    ///
    /// **This is the new fold path `rebuild_tree_visible` warns about.** Every other one selects
    /// the directory before folding it, which is why the panel has no selection clamp; this one
    /// folds everything at once, so a selection sitting on a nested request would be left on a
    /// row nothing paints — and the next `down` would jump from wherever it secretly still was.
    /// The selection therefore walks up to its outermost ancestor, which is the row that remains
    /// visible and the one that now stands for where you were. Same rule the response viewer
    /// follows when you fold the container you are standing in.
    fn collection_collapse_all(
        &mut self,
        _: &CollectionCollapseAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Walk up *before* collapsing, while the depths still describe a visible tree.
        if let Some(ix) = self.panel_selection {
            self.panel_selection = self.outermost_ancestor(ix);
        }

        for node in &self.tree {
            if matches!(node.kind, NodeKind::Directory) {
                self.collapsed.insert(node.path.clone());
            }
        }

        self.rebuild_tree_visible();
        cx.notify();
    }

    /// Expand every directory in the tree.
    ///
    /// No selection work, and the asymmetry is the point: expanding only ever *adds* rows, so
    /// whatever was selected is still drawn and still at the same index into `tree`.
    fn collection_expand_all(
        &mut self,
        _: &CollectionExpandAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.collapsed.is_empty() {
            return;
        }
        self.collapsed.clear();
        self.rebuild_tree_visible();
        cx.notify();
    }

    /// The depth-0 row that `ix` sits under, or `ix` itself when it is already at the root.
    ///
    /// A scan backwards rather than a stored parent link, for the reason `ancestors_of` does it
    /// in the response viewer: `Node` records no parent, and the flat depth-tagged list makes
    /// the nearest earlier shallower row the answer by construction.
    fn outermost_ancestor(&self, ix: usize) -> Option<usize> {
        let mut best = ix;
        let mut depth = self.tree.get(ix)?.depth;

        for candidate in (0..ix).rev() {
            if depth == 0 {
                break;
            }
            let node = self.tree.get(candidate)?;
            if node.depth < depth {
                depth = node.depth;
                best = candidate;
            }
        }

        Some(best)
    }

    /// Select a row and act on it: a directory folds, a request opens.
    ///
    /// Shared by the click and by `CollectionConfirm`, so the mouse path and the keyboard path
    /// cannot become different verbs — the mistake "actions, not direct calls" exists to
    /// prevent, caught four times already in this codebase.
    pub(crate) fn choose_collection_row(
        &mut self,
        row_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.tree.get(row_ix) else { return };
        let path = node.path.clone();
        let is_directory = matches!(node.kind, NodeKind::Directory);
        self.panel_selection = Some(row_ix);

        if is_directory {
            if !self.collapsed.remove(&path) {
                self.collapsed.insert(path);
            }
            self.rebuild_tree_visible();
            cx.notify();
            return;
        }

        self.open_collection_file(path, window, cx);
        // **Focus stays in the panel**, deliberately, and this line is what makes the keyboard
        // path agree with the mouse one. `open_collection_file` routes through `activate`,
        // which focuses the URL bar; on a click the panel's own `track_focus` listener fires
        // afterwards and takes it back, so without this Enter and click would leave focus in
        // different places. Staying is also the more useful of the two — browsing a collection
        // means opening several in a row, and that needs the arrow keys to keep working.
        window.focus(&self.panel_focus);
        cx.notify();
    }

    /// Place the selection without acting on the row. The right-click path needs this so
    /// `DeleteRequest` can carry no index and still be unambiguous.
    pub(crate) fn select_collection_row(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        if row_ix < self.tree.len() {
            self.panel_selection = Some(row_ix);
            cx.notify();
        }
    }

    pub(crate) fn set_collection_menu_anchor(&mut self, at: gpui::Point<gpui::Pixels>) {
        self.collection_menu_at = Some(at);
    }

    /// Where a menu for the panel should appear.
    ///
    /// A right-click supplies the point. The `delete` key does not, so it falls back to a spot
    /// inside the panel — near enough to the tree to read as belonging to it, and `anchored()`
    /// flips the corner near a window edge on its own.
    fn collection_menu_anchor(&self) -> gpui::Point<gpui::Pixels> {
        self.collection_menu_at
            .unwrap_or_else(|| gpui::point(px(crate::collection_panel::WIDTH * 0.5), px(160.)))
    }

    /// The selected row, when it is a request. Directories are excluded deliberately:
    /// `collection::remove` refuses a directory, and offering a verb that always fails is worse
    /// than not offering it.
    fn selected_request(&self) -> Option<&Node> {
        let node = self.tree.get(self.panel_selection?)?;
        matches!(node.kind, NodeKind::Request { .. }).then_some(node)
    }

    /// The selected row, whichever kind it is.
    ///
    /// The verbs that act on both — rename, trash, delete, and the paths — read this and branch,
    /// rather than gaining a parallel `…Folder` action each. One `Rename` that renames what is
    /// selected is what `f2` means in a tree, and a second action would be two ways to say it.
    fn selected_node(&self) -> Option<&Node> {
        self.tree.get(self.panel_selection?)
    }

    fn selection_is_directory(&self) -> bool {
        self.selected_node()
            .is_some_and(|node| matches!(node.kind, NodeKind::Directory))
    }

    /// Rewrite or clear `path` on every buffer inside `prefix`.
    ///
    /// The prefix form of `forget_path`, and the reason folder verbs are not just the request
    /// ones pointed at a directory: renaming `billing/` moves every request under it, so a
    /// buffer holding `billing/invoices.json` has to become `finance/invoices.json` or the next
    /// Ctrl+S recreates the folder that was just renamed away.
    fn retarget_prefix(&mut self, prefix: &Path, moved_to: Option<&Path>, cx: &mut Context<Self>) {
        for view in &self.views {
            let Some(path) = view.read(cx).path.clone() else {
                continue;
            };
            let Ok(rest) = path.strip_prefix(prefix) else {
                continue;
            };
            let next = moved_to.map(|root| root.join(rest));
            view.update(cx, |view, cx| {
                view.path = next;
                cx.notify();
            });
        }
    }

    fn open_collection_menu(
        &mut self,
        _: &OpenCollectionMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() || self.selected_node().is_none() {
            return;
        }
        // Right-clicking a *folder* opened nothing for several slices: this guard read
        // `selected_request`, so the gesture a tree most invites was inert — and "New folder"
        // sat in a menu you could not reach with a folder selected.
        let directory = self.selection_is_directory();
        let at = self.collection_menu_anchor();
        let focus = self.panel_focus.clone();
        let restore = Some(focus.clone());

        // Grouped by consequence, which is also roughly by risk: leaving Zuno, then making a
        // copy, then reading something out, then changing or removing the file. The two
        // destructive rows sit last and together, so the pointer never passes over them on the
        // way to something harmless.
        use context_menu::{MenuItem, MenuRow};

        // Duplicate and Move to… both take a *file*; the recursive versions are their own
        // decision, so a folder's menu leaves them out rather than offering a control that can
        // only fail — the rule the request guard already followed from the other side.
        if directory {
            let rows = vec![
                MenuItem::new("Reveal in file manager", RevealRequest, &focus, window).into(),
                MenuItem::new("Open in default app", OpenRequestExternally, &focus, window).into(),
                MenuRow::Separator,
                MenuItem::new("New folder", NewFolder, &focus, window).into(),
                MenuRow::Separator,
                MenuItem::new("Copy path", CopyRequestPath, &focus, window).into(),
                MenuItem::new("Copy relative path", CopyRequestRelativePath, &focus, window)
                    .into(),
                MenuRow::Separator,
                MenuItem::new("Rename", RenameRequest, &focus, window).into(),
                MenuItem::new("Move to trash", TrashRequest, &focus, window).into(),
                MenuItem::new("Delete…", DeleteRequest, &focus, window).into(),
            ];
            self.show_menu(rows, at, restore, window, cx);
            return;
        }

        let rows = vec![
            MenuItem::new("Reveal in file manager", RevealRequest, &focus, window).into(),
            MenuItem::new("Open in default app", OpenRequestExternally, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::new("Duplicate", DuplicateRequest, &focus, window).into(),
            MenuItem::new("New folder", NewFolder, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::new("Copy path", CopyRequestPath, &focus, window).into(),
            MenuItem::new("Copy relative path", CopyRequestRelativePath, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::new("Rename", RenameRequest, &focus, window).into(),
            // With Rename rather than with Duplicate: both answer "what and where is this
            // request", and neither changes what it sends.
            MenuItem::new("Move to…", MoveRequest, &focus, window).into(),
            MenuItem::new("Move to trash", TrashRequest, &focus, window).into(),
            // The ellipsis is load-bearing: this row asks, the one above it acts. Trash is
            // recoverable and delete is not, which is the whole reason only one of them stops
            // to check.
            MenuItem::new("Delete…", DeleteRequest, &focus, window).into(),
        ];
        self.show_menu(rows, at, restore, window, cx);
    }

    /// Ask. **This removes nothing** — `ConfirmDeleteRequest` is the only thing that does.
    ///
    /// The confirmation is a second menu rather than a modal of its own, which is what keeps it
    /// cheap: it inherits the primitive's keyboard handling, its `Escape`, and its occlusion.
    /// The destructive row names the file, because "are you sure?" without a subject is how the
    /// wrong thing gets deleted confidently.
    fn delete_request(&mut self, _: &DeleteRequest, window: &mut Window, cx: &mut Context<Self>) {
        // Reachable from the `delete` key with a menu already open, and from the menu row that
        // opened one — where `show_menu` would otherwise refuse as a stacked modal.
        self.close_row_menu(window, cx);

        let Some(node) = self.selected_node() else { return };
        let name = node.name.clone();
        let directory = matches!(node.kind, NodeKind::Directory);
        let path = node.path.clone();
        let at = self.collection_menu_anchor();
        let restore = Some(self.panel_focus.clone());

        // **A folder's prompt names the count.** "Delete billing?" with no number is how a
        // folder of forty requests goes missing — and a folder can hold work the panel never
        // showed, since an unreadable request is skipped by `scan` and has no row.
        let label = if directory {
            match collection::request_count(&path) {
                0 => format!("Delete {name} and everything in it"),
                1 => format!("Delete {name} and 1 request"),
                n => format!("Delete {name} and {n} requests"),
            }
        } else {
            format!("Delete {name}")
        };

        let rows = vec![
            // Not `MenuItem::new`: this row is reached by choosing the one above it, never by a
            // keystroke, so a keymap lookup would draw an empty column implying one exists.
            context_menu::MenuItem::plain(label, ConfirmDeleteRequest).into(),
            context_menu::MenuItem::dismiss("Keep it").into(),
        ];
        self.show_menu(rows, at, restore, window, cx);
    }

    /// Delete the selected request's file.
    fn confirm_delete_request(
        &mut self,
        _: &ConfirmDeleteRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.collection_menu_at = None;
        let Some(node) = self.selected_node() else { return };
        let (path, name) = (node.path.clone(), node.name.clone());
        let directory = matches!(node.kind, NodeKind::Directory);

        let removed = if directory {
            collection::remove_folder(&path)
        } else {
            collection::remove(&path)
        };
        if let Err(error) = removed {
            self.set_status(&format!("Could not delete: {error}"), cx);
            return;
        }

        // **Any buffer open on it forgets its path.** `save_request` writes to a remembered
        // `path` with no existence check, so leaving it set means the next Ctrl+S silently
        // recreates what was just deleted — and for a folder that means recreating the folder
        // too. The buffers stay open, which is right: the requests are still in front of you,
        // they simply have no file any more.
        if directory {
            self.retarget_prefix(&path, None, cx);
        } else {
            self.forget_path(&path, cx);
        }

        self.refresh_tree(cx);
        // Focus goes back to the panel: `close_menu` restored it there, and `refresh_tree` does
        // not move it, but a delete that leaves the tree unfocused would strand the next key.
        window.focus(&self.panel_focus);
        self.set_status(&format!("Deleted {name}"), cx);
    }

    /// The selected request's path relative to the collection root, with `/` separators.
    ///
    /// Derived rather than stored on `Node`: it is wanted by exactly one verb, and a second
    /// copy of the string on every row of a large collection is memory spent on a menu item.
    fn selected_relative(&self, cx: &App) -> Option<String> {
        let path = self.selected_node()?.path.clone();
        let root = crate::collections::root(cx)?;
        Some(
            path.strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        )
    }

    /// Hand the file to the desktop's file manager, selecting it.
    ///
    /// One call, deliberately: `reveal_path` is `unimplemented!()` in gpui's test platform, so
    /// nothing here can be driven headlessly. What *is* testable is that the menu offers the
    /// row against the right selection, which is where the mistake would be.
    fn reveal_request(&mut self, _: &RevealRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected_node() else { return };
        let path = node.path.clone();
        cx.reveal_path(&path);
    }

    /// Open the file in whatever the desktop associates with `.json`. Same testability note as
    /// `reveal_request` — `open_with_system` is `unimplemented!()` headlessly.
    fn open_request_externally(
        &mut self,
        _: &OpenRequestExternally,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.selected_node() else { return };
        let path = node.path.clone();
        cx.open_with_system(&path);
    }

    fn copy_request_path(&mut self, _: &CopyRequestPath, _: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected_node() else { return };
        let path = node.path.display().to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
        self.set_status(&format!("Copied {path}"), cx);
    }

    fn copy_request_relative_path(
        &mut self,
        _: &CopyRequestRelativePath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(relative) = self.selected_relative(cx) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(relative.clone()));
        self.set_status(&format!("Copied {relative}"), cx);
    }

    /// Copy the selected request to a fresh name beside it.
    ///
    /// The copy is **not** opened as a buffer. Duplicating is how you start a variant of a
    /// request you are about to edit, so landing on it would be defensible — but it is also how
    /// you take a backup before a risky change, and opening a tab you did not ask for is the
    /// worse failure of the two. The status bar names what appeared instead.
    fn duplicate_request(&mut self, _: &DuplicateRequest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected_request() else { return };
        let path = node.path.clone();
        match collection::duplicate(&path) {
            Ok(copy) => {
                let name = copy
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.refresh_tree(cx);
                self.set_status(&format!("Duplicated as {name}"), cx);
            }
            Err(error) => self.set_status(&format!("Could not duplicate: {error}"), cx),
        }
    }

    /// Move the selected request to the desktop trash.
    ///
    /// Unlike `Delete` this asks nothing, and that asymmetry is the point: the confirmation on
    /// delete exists because it cannot be undone, and trashing can. A dialog in front of a
    /// recoverable action trains people to dismiss dialogs.
    fn trash_request(&mut self, _: &TrashRequest, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected_node() else { return };
        let (path, name) = (node.path.clone(), node.name.clone());
        let directory = matches!(node.kind, NodeKind::Directory);

        let trashed = if directory {
            collection::trash_folder(&path)
        } else {
            collection::trash(&path)
        };
        if let Err(error) = trashed {
            self.set_status(&format!("Could not trash: {error}"), cx);
            return;
        }

        if directory {
            self.retarget_prefix(&path, None, cx);
        } else {
            self.forget_path(&path, cx);
        }
        self.refresh_tree(cx);
        window.focus(&self.panel_focus);
        self.set_status(&format!("Moved {name} to the trash"), cx);
    }

    /// Any buffer open on `path` forgets it, keeping its contents.
    ///
    /// `save_request` writes to a remembered `path` with **no existence check**, so a buffer
    /// still holding a deleted or trashed file's path silently recreates it on the next Ctrl+S.
    /// The buffer stays open on purpose: the request is still in front of you, it simply has no
    /// file any more.
    fn forget_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        for view in &self.views {
            if view.read(cx).path.as_deref() == Some(path) {
                view.update(cx, |view, cx| {
                    view.path = None;
                    cx.notify();
                });
            }
        }
    }

    /// Open the picker over every directory a request can be moved into.
    ///
    /// A picker rather than drag-and-drop, deliberately. Drag is a gesture nothing else in Zuno
    /// uses, the headless platform cannot observe it, and it needs a drop-target hit test per
    /// row; the picker is keyboard-first, already built, and is how every other "choose one of
    /// these" in the app works.
    fn move_request(&mut self, _: &MoveRequest, window: &mut Window, cx: &mut Context<Self>) {
        // A menu row dispatched this and is still open; `show_picker` refuses to stack.
        self.close_row_menu(window, cx);

        if self.modal_open() || self.selected_request().is_none() {
            return;
        }
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };

        // The request's own directory is offered and marked rather than filtered out. Removing
        // it would renumber the list depending on where the request happens to live, so the same
        // collection would present a different set of rows for each request in it.
        let current = self
            .selected_request()
            .and_then(|node| node.path.parent().map(Path::to_path_buf));

        let items: Vec<picker::Item> = collection::destinations(&root, &collection::folders(&root))
            .into_iter()
            .map(|(path, label)| {
                let is_current = Some(path.as_path()) == current.as_deref();
                picker::Item {
                    label: SharedString::from(label),
                    detail: SharedString::from(if is_current { "current folder" } else { "" }),
                    target: picker::Target::Folder(path),
                }
            })
            .collect();

        self.show_picker(items, "No folders yet — add one with New folder", window, cx);
    }

    /// Move the selected request into `directory`.
    fn move_selected_into(
        &mut self,
        directory: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.selected_request() else { return };
        let (path, name) = (node.path.clone(), node.name.clone());

        let moved = match collection::move_to(&path, &directory) {
            Ok(moved) => moved,
            Err(error) => {
                self.set_status(&format!("Could not move: {error}"), cx);
                return;
            }
        };

        // The buffer follows, as it does for a rename and for the same reason: the request still
        // exists, so `Ctrl+S` must still overwrite *it* rather than recreate it where it was.
        for view in &self.views {
            if view.read(cx).path.as_deref() == Some(path.as_path()) {
                let moved = moved.clone();
                view.update(cx, |view, cx| {
                    view.path = Some(moved);
                    cx.notify();
                });
            }
        }

        self.refresh_tree(cx);
        window.focus(&self.panel_focus);
        let shown = moved
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "the collection".to_string());
        self.set_status(&format!("Moved {name} to {shown}"), cx);
    }

    /// Open the new-folder box.
    ///
    /// The parent follows the selection, the file-tree convention: inside a selected directory,
    /// beside a selected request, at the root when nothing is selected.
    fn new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.close_row_menu(window, cx);

        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            self.set_status("No collection directory — nowhere to put a folder", cx);
            return;
        };

        let parent = match self.panel_selection.and_then(|ix| self.tree.get(ix)) {
            Some(node) if matches!(node.kind, NodeKind::Directory) => node.path.clone(),
            Some(node) => node.path.parent().unwrap_or(&root).to_path_buf(),
            None => root.clone(),
        };

        // A collapsed parent has no visible children, so the box would have nowhere to appear.
        // Expanding first is also what the reader means by "new folder in here".
        if self.collapsed.remove(&parent) {
            self.rebuild_tree_visible();
        }
        let (insert_at, depth) = self.new_folder_position(&parent, &root);

        let input = cx.new(|cx| {
            crate::input::TextInput::new("", "folder name", "CollectionRename", cx)
        });
        let handle = input.read(cx).focus_handle(cx);
        let blur = window.on_focus_out(&handle, cx, |_, window, cx| {
            window.dispatch_action(Box::new(CancelRename), cx);
        });

        self.new_folder = Some(NewFolderState {
            parent,
            insert_at,
            depth,
            input,
            _blur: blur,
        });
        // The box is a row in the list now, so it can be off screen. Nothing else scrolls it.
        self.panel_scroll
            .scroll_to_item(insert_at, gpui::ScrollStrategy::Center);
        window.focus(&handle);
        cx.notify();
    }

    /// Where the new-folder box goes, as a *visible* index, and how deep to indent it.
    ///
    /// **First child of its parent**, so the box appears immediately under the row you invoked it
    /// on rather than after however many requests that folder already holds. Last-child was the
    /// first version and put the box off screen in any folder with a screenful of requests, which
    /// is precisely the folder you are most likely to be reorganising.
    ///
    /// Sorted position is the third option and is worse than both: the row would jump as you
    /// typed, and the name is not final until Enter anyway. The tree re-sorts on the rescan that
    /// follows, which is the moment the name *is* final.
    fn new_folder_position(&self, parent: &Path, root: &Path) -> (usize, u16) {
        if parent == root {
            return (0, 0);
        }

        let Some(parent_visible) = self
            .tree_visible
            .iter()
            .position(|&ix| self.tree.get(ix).is_some_and(|node| node.path == parent))
        else {
            // The parent is not drawn — it has just been created, or a rescan lost it. The top of
            // the list is somewhere the box can at least be seen.
            return (0, 0);
        };

        let depth = self
            .tree_visible
            .get(parent_visible)
            .and_then(|&ix| self.tree.get(ix))
            .map(|node| node.depth)
            .unwrap_or(0);

        (parent_visible + 1, depth + 1)
    }

    /// The pending new folder's position, indent and input, if one is open.
    pub(crate) fn new_folder_row(&self) -> Option<(usize, u16, Entity<crate::input::TextInput>)> {
        let state = self.new_folder.as_ref()?;
        Some((state.insert_at, state.depth, state.input.clone()))
    }

    /// Open the OpenAPI import modal.
    fn new_workspace(&mut self, _: &NewWorkspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        let location = crate::app_state::default_new_location()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let panel = cx.new(|cx| {
            crate::workspace_panel::WorkspacePanel::new(location, restore, window, cx)
        });

        let subscription =
            cx.subscribe_in(&panel, window, |workspace, panel, event, window, cx| {
                let crate::workspace_panel::WorkspaceEvent::Confirmed { name, location } = event;
                workspace.create_workspace(
                    panel.clone(),
                    name.clone(),
                    location.clone(),
                    window,
                    cx,
                );
            });

        self.new_workspace_panel = Some(WorkspacePanelState {
            panel,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Create the directory, register it, and switch to it.
    ///
    /// The name goes through `slug` for the reason every other typed name does: it becomes a
    /// path segment, so `../../evil` must not walk out of the location.
    fn create_workspace(
        &mut self,
        panel: Entity<crate::workspace_panel::WorkspacePanel>,
        name: String,
        location: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stem = collection::slug(&name);
        if stem.is_empty() {
            panel.update(cx, |panel, cx| panel.report("That name has no usable characters", cx));
            return;
        }
        if location.trim().is_empty() {
            panel.update(cx, |panel, cx| panel.report("Choose where the folder goes", cx));
            return;
        }

        let path = PathBuf::from(shellexpand_home(&location)).join(&stem);
        if path.exists() {
            panel.update(cx, |panel, cx| {
                panel.report(format!("{} already exists", path.display()), cx)
            });
            return;
        }
        if let Err(error) = std::fs::create_dir_all(&path) {
            panel.update(cx, |panel, cx| {
                panel.report(format!("Could not create it: {error}"), cx)
            });
            return;
        }

        let Some(id) = crate::app_state::add_workspace(cx, path.clone()) else {
            panel.update(cx, |panel, cx| panel.report("Workspaces are not being saved", cx));
            return;
        };

        self.close_workspace_panel(window, cx);
        self.switch_workspace(&id, window, cx);
        self.set_status(&format!("Created {}", path.display()), cx);
    }

    /// Register a directory that already exists — a collection someone cloned, or one built by
    /// hand. The other half of `NewWorkspace`, and the reason the location field is not the only
    /// way to put a workspace somewhere unusual.
    fn open_workspace(&mut self, _: &OpenWorkspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open workspace".into()),
        });

        self.workspace_prompt = Some(cx.spawn_in(window, async move |workspace, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };

            workspace
                .update_in(cx, |workspace, window, cx| {
                    let Some(id) = crate::app_state::add_workspace(cx, path.clone()) else {
                        workspace.set_status("Workspaces are not being saved", cx);
                        return;
                    };
                    workspace.switch_workspace(&id, window, cx);
                    workspace.set_status(&format!("Opened {}", path.display()), cx);
                })
                .ok();
        }));
    }

    fn workspace_confirm(&mut self, _: &WorkspaceConfirm, _: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.new_workspace_panel.as_ref() else { return };
        state.panel.clone().update(cx, |panel, cx| panel.confirm(cx));
    }

    fn workspace_dismiss(&mut self, _: &WorkspaceDismiss, window: &mut Window, cx: &mut Context<Self>) {
        self.close_workspace_panel(window, cx);
    }

    fn close_workspace_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.new_workspace_panel.take() else { return };
        if let Some(focus) = state.panel.read(cx).restore_focus() {
            window.focus(&focus);
        }
        cx.notify();
    }

    /// The folder dialog behind the location field.
    ///
    /// One call, like `RevealRequest` and `ChooseBodyFile`: `prompt_for_paths` is
    /// `unimplemented!()` in the test platform, so the handler is kept as small as the untestable
    /// part has to be.
    fn workspace_browse(&mut self, _: &WorkspaceBrowse, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.new_workspace_panel.as_ref() else { return };
        let panel = state.panel.clone();
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a folder".into()),
        });

        self.workspace_prompt = Some(cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            panel
                .update_in(cx, |panel, window, cx| {
                    panel.set_location(path.display().to_string(), window, cx)
                })
                .ok();
        }));
    }

    fn import_openapi(&mut self, _: &ImportOpenApi, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        let panel = cx.new(|cx| crate::import_panel::ImportPanel::new(restore, window, cx));

        let subscription = cx.subscribe_in(&panel, window, |workspace, panel, event, window, cx| {
            match event {
                crate::import_panel::ImportEvent::Dismissed => {
                    workspace.import = None;
                    cx.notify();
                }
                crate::import_panel::ImportEvent::Confirmed(source) => {
                    workspace.run_import(panel.clone(), source.clone(), window, cx);
                }
            }
        });

        self.import = Some(ImportState {
            panel,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn import_confirm(&mut self, _: &ImportConfirm, _: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.import.as_ref() else { return };
        state.panel.clone().update(cx, |panel, cx| panel.confirm(cx));
    }

    fn import_dismiss(&mut self, _: &ImportDismiss, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.import.as_ref() else { return };
        state.panel.clone().update(cx, |panel, cx| panel.dismiss(window, cx));
    }

    /// Read the source, parse it, and write what it yields into the collection.
    ///
    /// **A URL goes through the engine rather than a fresh HTTP client.** Zuno already owns one
    /// on a tokio thread, with the TLS, redirect and timeout behaviour the rest of the app uses —
    /// a second client would be a second set of those decisions, silently different.
    fn run_import(
        &mut self,
        panel: Entity<crate::import_panel::ImportPanel>,
        source: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            panel.update(cx, |panel, cx| {
                panel.report("No collection directory — nowhere to import into", cx)
            });
            return;
        };

        let source = source.trim().to_string();
        // The one distinction between the two sources, made from the text rather than from a
        // mode the user has to choose first.
        let is_url = source.starts_with("http://") || source.starts_with("https://");

        if !is_url {
            let bytes = match std::fs::read(&source) {
                Ok(bytes) => bytes,
                Err(error) => {
                    panel.update(cx, |panel, cx| {
                        panel.report(format!("Could not read {source}: {error}"), cx)
                    });
                    return;
                }
            };
            self.finish_import(&panel, &root, &bytes, cx);
            return;
        }

        let Some(engine) = cx.engine() else {
            panel.update(cx, |panel, cx| {
                panel.report("The HTTP engine failed to start — restart Zuno", cx)
            });
            return;
        };

        let spec = RequestSpec {
            id: RequestId(0),
            url: source.clone(),
            method: zuno_core::Method::Get,
            ..RequestSpec::default()
        };
        let (_job, events) = engine.send(spec);

        self.import_task = Some(cx.spawn(async move |this, cx| {
            let mut fetched: Option<Result<bytes::Bytes, String>> = None;
            while let Ok(event) = events.recv().await {
                match event {
                    zuno_core::engine::Event::Done { response, .. } => {
                        fetched = Some(Ok(response.body));
                        break;
                    }
                    zuno_core::engine::Event::Failed { error, .. } => {
                        fetched = Some(Err(error.to_string()));
                        break;
                    }
                    _ => {}
                }
            }

            let _ = this.update(cx, |workspace, cx| match fetched {
                Some(Ok(body)) => workspace.finish_import(&panel, &root, &body, cx),
                Some(Err(error)) => {
                    panel.update(cx, |panel, cx| panel.report(format!("Could not fetch: {error}"), cx))
                }
                // The channel closed with neither a body nor an error, which means the engine
                // thread went away mid-fetch.
                None => panel.update(cx, |panel, cx| panel.report("The fetch was interrupted", cx)),
            });
        }));
    }

    /// Parse a fetched or read spec and write its requests into the collection.
    fn finish_import(
        &mut self,
        panel: &Entity<crate::import_panel::ImportPanel>,
        root: &Path,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) {
        let import = match zuno_core::openapi::parse(bytes) {
            Ok(import) => import,
            Err(error) => {
                panel.update(cx, |panel, cx| panel.report(error.to_string(), cx));
                return;
            }
        };
        if import.requests.is_empty() {
            panel.update(cx, |panel, cx| {
                panel.report("The document has no operations to import", cx)
            });
            return;
        }

        // Everything lands under one folder named for the spec, so an import is a thing you can
        // find and a thing you can delete. Without it a hundred requests scatter through a
        // collection someone had already organised.
        let title = import.title.clone().unwrap_or_else(|| "imported".to_string());
        let base = root.join(collection::slug(&title));

        let mut written = 0usize;
        let mut failures = 0usize;
        for request in &import.requests {
            // The operation's tag becomes a folder inside the spec's own, so an API arrives
            // grouped the way its author grouped it.
            let directory = match &request.folder {
                Some(tag) => base.join(collection::slug(tag)),
                None => base.clone(),
            };
            // `allocate` creates the directory and picks a free name, so re-importing the same
            // spec adds `-2` files rather than overwriting a request someone has since edited.
            match collection::allocate(&directory, &request.spec.name)
                .and_then(|path| collection::write(&path, &request.spec).map(|()| path))
            {
                Ok(_) => written += 1,
                Err(_) => failures += 1,
            }
        }

        self.refresh_tree(cx);
        self.import = None;

        let mut message = format!("Imported {written} requests into {}", collection::slug(&title));
        if failures > 0 {
            message.push_str(&format!(" — {failures} could not be written"));
        }
        if !import.skipped.is_empty() {
            message.push_str(&format!(" — {} skipped", import.skipped.len()));
            for note in &import.skipped {
                eprintln!("[zuno] import: {note}");
            }
        }
        self.set_status(&message, cx);
        cx.notify();
    }

    /// Open the rename box on the selected request.
    fn rename_request(&mut self, _: &RenameRequest, window: &mut Window, cx: &mut Context<Self>) {
        // A menu row dispatched this, and it is still open until closed.
        self.close_row_menu(window, cx);

        let Some(node) = self.selected_node() else { return };
        let row_ix = self.panel_selection.unwrap_or_default();
        let (path, name) = (node.path.clone(), node.name.clone());

        // Seeded with the current name and fully selected, so typing replaces it while `End`
        // keeps it — what every rename box does.
        let input = cx.new(|cx| {
            let mut input = crate::input::TextInput::new(name, "name", "CollectionRename", cx);
            input.select_all_text(cx);
            input
        });
        let handle = input.read(cx).focus_handle(cx);

        // **Cancel on blur.** Clicking elsewhere has to end the rename, or the box stays on
        // screen unfocused and a later `Enter` commits an edit the user walked away from.
        // Cancel rather than commit, unlike VS Code: a rename is a file operation, and the safe
        // reading of "clicked somewhere else" is that it was not meant.
        let blur = window.on_focus_out(&handle, cx, |_, window, cx| {
            window.dispatch_action(Box::new(CancelRename), cx);
        });

        self.renaming = Some(RenameState {
            row_ix,
            path,
            input,
            _blur: blur,
        });
        window.focus(&handle);
        cx.notify();
    }

    /// Close whichever naming box is open. Shared, because `Escape` and a blur mean the same
    /// thing to both and a second action would be two ways to say one word.
    fn cancel_rename(&mut self, _: &CancelRename, window: &mut Window, cx: &mut Context<Self>) {
        // Idempotent: committing drops the state and then moves focus, which fires the blur
        // listener, which dispatches this. Without the `take` that would be a second cancel
        // racing the commit it followed.
        let closed = self.renaming.take().is_some() | self.new_folder.take().is_some();
        if closed {
            window.focus(&self.panel_focus);
            cx.notify();
        }
    }

    fn commit_rename(&mut self, _: &CommitRename, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.new_folder.take() {
            let typed = state.input.read(cx).text().to_string();
            window.focus(&self.panel_focus);
            if typed.trim().is_empty() {
                self.set_status("A folder needs a name", cx);
            } else {
                match collection::create_folder(&state.parent, &typed) {
                    Ok(made) => {
                        let name = made
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default();
                        self.refresh_tree(cx);
                        self.set_status(&format!("Created {name}"), cx);
                    }
                    Err(error) => self.set_status(&format!("Could not create: {error}"), cx),
                }
            }
            cx.notify();
            return;
        }

        let Some(state) = self.renaming.take() else { return };
        let typed = state.input.read(cx).text().to_string();
        // Focus first: the box is gone from the next frame either way, and leaving focus on a
        // dropped entity is the "keymap goes dead with nothing on screen" failure.
        window.focus(&self.panel_focus);

        if typed.trim().is_empty() {
            self.set_status("A request needs a name", cx);
            cx.notify();
            return;
        }

        // `rename` appends the `.json` extension unconditionally, which on a directory would
        // produce `billing.json` — hence the separate folder verb rather than a flag.
        let directory = state.path.is_dir();
        let renamed = if directory {
            collection::rename_folder(&state.path, &typed)
        } else {
            collection::rename(&state.path, &typed)
        };

        match renamed {
            Ok(renamed) => {
                // The buffers follow rather than forgetting: unlike a delete, the requests still
                // exist and Ctrl+S should still overwrite *them*. For a folder that is every
                // buffer underneath it, which is what `retarget_prefix` is for.
                if directory {
                    self.retarget_prefix(&state.path.clone(), Some(&renamed), cx);
                } else {
                    for view in &self.views {
                        if view.read(cx).path.as_deref() == Some(state.path.as_path()) {
                            let renamed = renamed.clone();
                            view.update(cx, |view, cx| {
                                view.path = Some(renamed);
                                cx.notify();
                            });
                        }
                    }
                }
                self.refresh_tree(cx);
            }
            Err(error) => self.set_status(&format!("Could not rename: {error}"), cx),
        }
        cx.notify();
    }

    /// The row being renamed and its input, if one is.
    pub(crate) fn renaming_row(&self) -> Option<(usize, Entity<crate::input::TextInput>)> {
        let state = self.renaming.as_ref()?;
        Some((state.row_ix, state.input.clone()))
    }

    /// Open a request file as a buffer, remembering where it came from.
    ///
    /// **A file already open is activated, not opened again.** `Ctrl+P` gets this by filtering
    /// open paths out of its list, which the panel cannot do — a tree that hides the requests
    /// you have open would be worse than the duplicate. So the rule lives here instead, where
    /// both paths inherit it, and the picker gains the case its filter cannot cover: the filter
    /// is only as fresh as the scan behind it.
    ///
    /// Shared by the panel and by the picker's `Target::File`. **The caller decides focus**:
    /// the picker leaves it in the new buffer (`choosing_a_buffer_leaves_focus_in_that_buffer`),
    /// the panel keeps it in the tree.
    fn open_collection_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self
            .views
            .iter()
            .position(|view| view.read(cx).path.as_deref() == Some(path.as_path()))
        {
            self.activate(ix, window, cx);
            return;
        }

        // The file may have been deleted or broken since the scan; report rather than
        // opening an empty buffer.
        let spec = match collection::read(&path) {
            Ok(spec) => spec,
            Err(error) => {
                self.set_status(&format!("Could not open: {error}"), cx);
                return;
            }
        };

        // Stored ids are always 0 (see `collection`), so a live one is assigned here — the
        // workspace is the only thing that knows which are taken.
        let spec = RequestSpec {
            id: self.next_id(cx),
            ..spec
        };
        self.open(spec, window, cx);
        // Remembering the file is what makes a later Ctrl+S overwrite it rather than derive a
        // fresh name beside it.
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| {
                view.path = Some(path);
                cx.notify();
            });
        }
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
        empty_hint: impl Into<SharedString>,
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

    /// The workspace switcher, and the same list again for forgetting.
    ///
    /// One builder rather than two: the rows are identical and only the target differs, so a
    /// second copy would be the place the two drift.
    fn workspace_items(&self, forget: bool, cx: &App) -> Vec<picker::Item> {
        let active = crate::app_state::active_id(cx);
        crate::app_state::workspaces(cx)
            .into_iter()
            .filter(|entry| !forget || Some(&entry.id) != active.as_ref())
            .map(|entry| {
                let is_active = Some(&entry.id) == active.as_ref();
                // A directory that has gone — deleted, unmounted, a different machine — is
                // *marked*, not dropped. The fix is usually to reconnect it, not to start over.
                let detail = match (is_active, entry.path.is_dir()) {
                    (_, false) => format!("missing — {}", entry.path.display()),
                    (true, _) => format!("current — {}", entry.path.display()),
                    (false, _) => entry.path.display().to_string(),
                };
                picker::Item {
                    label: SharedString::from(crate::app_state::label(&entry.path)),
                    detail: SharedString::from(detail),
                    target: if forget {
                        picker::Target::ForgetWorkspace(entry.id)
                    } else {
                        picker::Target::Workspace(entry.id)
                    },
                }
            })
            .collect()
    }

    fn switch_workspace_action(
        &mut self,
        _: &SwitchWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        let items = self.workspace_items(false, cx);
        self.show_picker(items, "No workspaces", window, cx);
    }

    fn forget_workspace_action(
        &mut self,
        _: &ForgetWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        // The active one is filtered out: forgetting what you are looking at would have to
        // switch you somewhere else as a side effect of a verb that does not say so.
        let items = self.workspace_items(true, cx);
        self.show_picker(
            items,
            "No other workspaces — the one you are in cannot be forgotten",
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
                        view.set_body_kind(kind, cx);
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
            picker::Target::Folder(directory) => {
                self.move_selected_into(directory, window, cx);
            }
            picker::Target::Workspace(id) => self.switch_workspace(&id, window, cx),
            picker::Target::ForgetWorkspace(id) => {
                let name = crate::app_state::workspaces(cx)
                    .into_iter()
                    .find(|entry| entry.id == id)
                    .map(|entry| crate::app_state::label(&entry.path))
                    .unwrap_or_else(|| id.clone());

                if crate::app_state::forget_workspace(cx, &id) {
                    // Forgetting the active one re-resolves onto another, so the window has to
                    // follow — otherwise the buffers on screen belong to a workspace that is no
                    // longer registered.
                    if crate::app_state::active_id(cx).is_some() {
                        self.reload_active_workspace(window, cx);
                    }
                    self.set_status(&format!("Forgot {name} — its files were left alone"), cx);
                } else {
                    self.set_status("The last workspace cannot be forgotten", cx);
                }
            }
            picker::Target::File(path) => {
                // Focus is left where `activate` put it — inside the new buffer. The panel
                // shares this method and re-focuses itself afterwards; see
                // `choose_collection_row`.
                self.open_collection_file(path, window, cx);
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
        crate::session::Session::new(
            tabs,
            self.active_ix,
            self.environment.clone(),
            self.panel_visible,
        )
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

    /// Reveals the Body tab, then focuses whatever that body type actually paints.
    ///
    /// **Not `body_focus`**, which is the editor's handle and is only on screen for a raw body.
    /// Targeting it on a form focused an element that did not exist, and because dispatch walks up
    /// the focus tree that severed the path to `Workspace` — every binding died, `Ctrl+L`
    /// included, with nothing on screen saying why. See `RequestView::body_focus_target`.
    ///
    /// A body with nothing focusable says so rather than moving focus somewhere useless; a silent
    /// no-op here reads as the keystroke being broken.
    fn focus_body(&mut self, _: &FocusBody, window: &mut Window, cx: &mut Context<Self>) {
        self.show_request_tab(RequestTab::Body, cx);
        let Some(view) = self.active() else { return };
        match view.read(cx).body_focus_target(cx) {
            Some(handle) => window.focus(&handle),
            None => self.set_status("This body has nothing to type into", cx),
        }
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
        picker.update(cx, |picker, cx| picker.set_fallback(custom_method_row, cx));
    }

    fn add_header(&mut self, _: &AddHeader, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| {
                view.show_request_tab(RequestTab::Headers, cx);
                view.add_row(RowKind::Header, window, cx);
            });
        }
    }

    fn add_query(&mut self, _: &AddQuery, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| {
                view.show_request_tab(RequestTab::Query, cx);
                view.add_row(RowKind::Query, window, cx);
            });
        }
    }

    fn next_request_tab(&mut self, _: &NextRequestTab, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.cycle_request_tab(1, cx));
        }
    }

    fn prev_request_tab(&mut self, _: &PrevRequestTab, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.cycle_request_tab(-1, cx));
        }
    }

    fn show_headers_tab(&mut self, _: &ShowHeadersTab, _: &mut Window, cx: &mut Context<Self>) {
        self.show_request_tab(RequestTab::Headers, cx);
    }

    fn show_params_tab(&mut self, _: &ShowParamsTab, _: &mut Window, cx: &mut Context<Self>) {
        self.show_request_tab(RequestTab::Query, cx);
    }

    fn show_body_tab(&mut self, _: &ShowBodyTab, _: &mut Window, cx: &mut Context<Self>) {
        self.show_request_tab(RequestTab::Body, cx);
    }

    fn show_request_tab(&mut self, tab: RequestTab, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.show_request_tab(tab, cx));
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
        view.update(cx, |view, cx| view.show_request_tab(RequestTab::Body, cx));
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
        view.update(cx, |view, cx| view.show_request_tab(RequestTab::Body, cx));

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
        view.update(cx, |view, cx| view.show_request_tab(RequestTab::Body, cx));
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

        view.update(cx, |view, cx| view.show_request_tab(RequestTab::Body, cx));
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

    /// Swap the response pane between the body and the headers.
    ///
    /// On `Workspace` like every other handler, but the *state* is on the buffer — two
    /// requests open for different reasons shouldn't share a pane preference.
    fn toggle_response_view(
        &mut self,
        _: &ToggleResponseView,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.toggle_response_view(cx));
        }
    }

    /// Open the find bar over the response body.
    ///
    /// Guarded by `modal_open` like every other opener: a find bar takes focus, and taking it
    /// from behind a modal's scrim is how the modal's leaf key context stops matching and its
    /// whole keymap — `Escape` included — silently dies.
    fn find_in_body(&mut self, _: &FindInBody, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.open_body_search(window, cx));
        }
    }

    fn body_find_next(&mut self, _: &BodyFindNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.step_body_search(1, cx));
        }
    }

    fn body_find_prev(&mut self, _: &BodyFindPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.step_body_search(-1, cx));
        }
    }

    fn close_body_find(&mut self, _: &CloseBodyFind, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.close_body_search(window, cx));
        }
    }

    /// Replace the current match, and say what happened.
    ///
    /// A replace that matched nothing is silent otherwise, and indistinguishable from a
    /// keystroke that did not register.
    fn replace_next(&mut self, _: &ReplaceNext, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let replaced = view.update(cx, |view, cx| view.replace_current(window, cx));
        if replaced == 0 {
            self.set_status("Nothing to replace", cx);
        }
    }

    fn replace_all(&mut self, _: &ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let replaced = view.update(cx, |view, cx| view.replace_all(window, cx));
        match replaced {
            0 => self.set_status("Nothing to replace", cx),
            1 => self.set_status("Replaced 1 match", cx),
            n => self.set_status(&format!("Replaced {n} matches"), cx),
        }
    }

    fn find_in_response(&mut self, _: &FindInResponse, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.open_search(window, cx));
        }
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.step_search(1, cx));
        }
    }

    fn find_prev(&mut self, _: &FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.step_search(-1, cx));
        }
    }

    fn close_find(&mut self, _: &CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.close_search(window, cx));
        }
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

    fn response_row_next(&mut self, _: &ResponseRowNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.move_body_selection(1, cx));
        }
    }

    fn response_row_prev(&mut self, _: &ResponseRowPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.move_body_selection(-1, cx));
        }
    }

    fn scroll_left(&mut self, _: &ScrollLeft, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.scroll_body_horizontally(-1., cx));
        }
    }

    fn scroll_right(&mut self, _: &ScrollRight, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.scroll_body_horizontally(1., cx));
        }
    }

    fn scroll_start(&mut self, _: &ScrollStart, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.scroll_body_to_start(cx));
        }
    }

    fn toggle_fold(&mut self, _: &ToggleFold, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.toggle_selected_fold(cx));
        }
    }

    /// Open the row menu where the right-click landed.
    ///
    /// The items are built here rather than by the pane because only `Workspace` can read the
    /// keymap for each keystroke *and* owns the modal slot. They **adapt rather than disable**:
    /// no path on a raw body, no fold on a scalar. A greyed-out row that can never apply is
    /// noise in a menu this short, and the same rule the removed toolbar labels followed.
    fn open_row_menu(&mut self, _: &OpenRowMenu, window: &mut Window, cx: &mut Context<Self>) {
        // The anchor is consumed either way: leaving it set after a refused open would place
        // the *next* menu where this click was.
        let Some(view) = self.active() else { return };
        let at = view.update(cx, |view, _| view.take_menu_anchor());

        if self.modal_open() {
            return;
        }
        let Some(at) = at else { return };
        if view.read(cx).selected_body_row().is_none() {
            return;
        }

        // The verbs act on the response pane, so their keystrokes are the ones that mean
        // something *there* — all three are scoped to it.
        let focus = view.read(cx).response_focus.clone();
        let mut items = vec![context_menu::MenuItem::new(
            "Copy value",
            CopyRowValue,
            &focus,
            window,
        )];
        if view.read(cx).selected_body_path().is_some() {
            items.push(context_menu::MenuItem::new("Copy path", CopyRowPath, &focus, window));
        }
        if view.read(cx).selected_is_container() {
            let label = if view.read(cx).selected_is_folded() {
                "Unfold"
            } else {
                "Fold"
            };
            items.push(context_menu::MenuItem::new(label, ToggleFold, &focus, window));
        }

        let restore = Some(focus);
        self.show_menu(items.into_iter().map(Into::into).collect(), at, restore, window, cx);
    }

    /// The application menu: the things that have nowhere else to live.
    ///
    /// Deliberately **not** a mouse copy of `Ctrl+K`. Every verb in the app already has an icon
    /// button or a palette row, so a menu repeating them would be a second command list to keep
    /// in step with no drift test watching it. What it carries instead is what had no home at
    /// all — the version, the links, and quitting — plus three ways *in* for someone who has
    /// just opened Zuno and does not yet know the palette exists.
    fn app_menu_rows(&self, window: &Window) -> Vec<context_menu::MenuRow> {
        use context_menu::{MenuItem, MenuRow};

        // Every verb here is bound globally, so the context this resolves against does not
        // matter — but it still has to be *a* handle, and `Workspace`'s is the honest one: the
        // app menu belongs to the window rather than to any pane.
        let focus = self.focus_handle.clone();
        let repo = env!("CARGO_PKG_REPOSITORY");
        vec![
            MenuItem::new("Find request", OpenRequest, &focus, window).into(),
            MenuItem::new("Command palette", OpenPalette, &focus, window).into(),
            MenuItem::new("Request settings", OpenSettings, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::url("Documentation", "", repo).into(),
            // Prefilled with the version and platform, because the two facts every bug report
            // needs are the two the reporter is least likely to include.
            MenuItem::url("Report an issue", "", issue_url(repo)).into(),
            MenuRow::Separator,
            MenuItem::url("About Zuno", env!("CARGO_PKG_VERSION"), format!("{repo}/releases"))
                .into(),
            MenuRow::Separator,
            // Last, and behind a rule: it is the one item here that loses work, and putting it
            // next to a link is how a misclick happens.
            MenuItem::new("Quit", Quit, &focus, window).into(),
        ]
    }

    /// The workspace menu, hung off the panel header.
    ///
    /// Four verbs that had a palette row each and, between them, one mouse path — which is §2's
    /// discoverability failure recurring exactly as it describes it: a binding and a palette row
    /// both satisfy the convention checklist and neither can be seen.
    fn open_workspace_menu(
        &mut self,
        _: &OpenWorkspaceMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        use context_menu::{MenuItem, MenuRow};
        let focus = self.panel_focus.clone();
        // Under the header it belongs to, not at the cursor — the same rule the app menu
        // follows, and for the same reason: this menu belongs to a button.
        let at = gpui::point(
            gpui::px(8.),
            gpui::px(crate::chrome::TITLEBAR_HEIGHT + crate::collection_panel::HEADER_HEIGHT),
        );
        let rows = vec![
            MenuItem::new("Switch workspace", SwitchWorkspace, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::new("New workspace", NewWorkspace, &focus, window).into(),
            MenuItem::new("Open workspace…", OpenWorkspace, &focus, window).into(),
            MenuRow::Separator,
            // Last and behind a rule, like Quit in the app menu: it is the only row here that
            // takes something away.
            MenuItem::new("Forget workspace", ForgetWorkspace, &focus, window).into(),
        ];
        self.show_menu(rows, at, Some(focus), window, cx);
    }

    fn open_app_menu(&mut self, _: &OpenAppMenu, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // A fixed point under the app name, not the cursor: this menu belongs to a button, and
        // a menu that opens wherever you happened to click reads as a context menu instead.
        let at = gpui::point(gpui::px(8.), gpui::px(crate::chrome::TITLEBAR_HEIGHT));
        let rows = self.app_menu_rows(window);
        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        self.show_menu(rows, at, restore, window, cx);
    }

    /// Put a menu on screen and wire it up. Shared by the row menu and the application menu,
    /// which differ only in their rows and where they are anchored.
    fn show_menu(
        &mut self,
        rows: Vec<context_menu::MenuRow>,
        at: gpui::Point<gpui::Pixels>,
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = cx.new(|cx| context_menu::ContextMenu::new(rows, at, restore, cx));

        let subscription =
            cx.subscribe_in(&menu, window, |workspace, _, event, window, cx| match event {
                context_menu::ContextMenuEvent::Dismissed => workspace.close_row_menu(window, cx),
                // Close *then* act. `Window::dispatch_action` defers, so the two orders are
                // indistinguishable for actions (§12) — but closing first is what puts focus
                // back where it was before anything runs.
                context_menu::ContextMenuEvent::Chose(command) => {
                    let command = command.clone();
                    workspace.close_row_menu(window, cx);
                    match command {
                        context_menu::MenuCommand::Dispatch(action) => {
                            window.dispatch_action(action, cx)
                        }
                        context_menu::MenuCommand::OpenUrl(url) => cx.open_url(&url),
                        // The opener has already closed the menu, which is the whole effect.
                        context_menu::MenuCommand::Dismiss => {}
                    }
                }
            });

        let focus = menu.read(cx).focus_handle();
        self.menu = Some(MenuState {
            menu,
            _subscription: subscription,
        });
        window.focus(&focus);
        cx.notify();
    }

    fn close_row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.menu.take() else { return };
        if let Some(handle) = state.menu.read(cx).restore_focus() {
            window.focus(&handle);
        }
        cx.notify();
    }

    fn menu_next(&mut self, _: &MenuNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.menu {
            state.menu.update(cx, |menu, cx| menu.select(1, cx));
        }
    }

    fn menu_prev(&mut self, _: &MenuPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.menu {
            state.menu.update(cx, |menu, cx| menu.select(-1, cx));
        }
    }

    fn menu_confirm(&mut self, _: &MenuConfirm, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.menu {
            state.menu.update(cx, |menu, cx| menu.confirm(cx));
        }
    }

    fn menu_dismiss(&mut self, _: &MenuDismiss, window: &mut Window, cx: &mut Context<Self>) {
        self.close_row_menu(window, cx);
    }

    /// Copy the selected row's value.
    ///
    /// The counterpart to `copy_response` at a finer grain: that verb answers "give me the
    /// response", this one answers "give me *that*". A JSON string arrives decoded and a
    /// container arrives as its own source text — see `BodyView::selected_value`.
    ///
    /// Every failure says which one it is. "Nothing selected" and "this row has no value" are
    /// different problems with different fixes, and a single silent no-op for both is how a
    /// working control comes to look broken.
    fn copy_row_value(&mut self, _: &CopyRowValue, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        if view.read(cx).selected_body_row().is_none() {
            self.set_status(&self.select_a_row_hint(window), cx);
            return;
        }

        match view.read(cx).selected_body_value() {
            Some(value) => {
                let size = format_bytes(value.len() as u64);
                cx.write_to_clipboard(ClipboardItem::new_string(value));
                self.set_status(&format!("Copied {size} to the clipboard"), cx);
            }
            None => self.set_status("That row has no value to copy", cx),
        }
    }

    /// Copy the selected row's path, as JSONPath.
    ///
    /// Raw bodies have no path — there is no structure to name a position within — so this
    /// says so rather than falling back to a line number, which no tool downstream accepts.
    fn copy_row_path(&mut self, _: &CopyRowPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        if view.read(cx).selected_body_row().is_none() {
            self.set_status(&self.select_a_row_hint(window), cx);
            return;
        }

        match view.read(cx).selected_body_path() {
            Some(path) => {
                cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                self.set_status(&format!("Copied {path}"), cx);
            }
            None => self.set_status("Paths need a JSON body", cx),
        }
    }

    /// Told from the keymap, so it can't advertise a key that isn't bound — the same rule the
    /// in-flight pane's cancel hint follows after it spent several milestones naming `Ctrl+C`.
    fn select_a_row_hint(&self, window: &mut Window) -> String {
        match keybinding_label(&FocusResponse, window) {
            key if key.is_empty() => "Select a row in the response first".to_string(),
            key => format!("Select a row first — {key} focuses the response, then ↑/↓"),
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
    fn copy_response(&mut self, _: &CopyResponse, window: &mut Window, cx: &mut Context<Self>) {
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
            None => {
                let hint = match keybinding_label(&SaveResponse, window) {
                    key if key.is_empty() => "This response isn't text — save it to a file instead".to_string(),
                    key => format!("This response isn't text — use {key} to save it to a file"),
                };
                self.set_status(&hint, cx)
            }
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

    /// Copy the active request to the clipboard as a runnable curl command.
    ///
    /// **Variables are resolved, except the secret ones.** `Resolver::without_secrets` substitutes
    /// `dev.json` values and leaves `dev.local.json` ones as `{{token}}`, so the command runs
    /// against your dev box while a credential never reaches the clipboard — and therefore never
    /// reaches the issue or the chat message the command is being pasted into. That split is the
    /// same one invariant 10 protects in the collection files; the point of it being a *file*
    /// distinction rather than a per-variable flag is that this gets it right for free.
    ///
    /// Nothing here can fail: a request too incomplete to send still exports, because "here's what
    /// I have" is exactly when you reach for this. `to_command` falls back to the raw URL when the
    /// engine's URL resolution refuses it.
    fn copy_as_curl(&mut self, _: &CopyAsCurl, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        let spec = view.read(cx).spec(cx);
        let resolver = self.resolver(cx).without_secrets();
        let command = curl::to_command(&resolver.apply(&spec));

        let withheld = resolver.withheld_in(&spec);
        cx.write_to_clipboard(ClipboardItem::new_string(command));

        // Say when a placeholder was left in, or the command looks broken rather than careful.
        let message = match withheld.as_slice() {
            [] => "Copied as a curl command".to_string(),
            [one] => format!("Copied as curl — {{{{{one}}}}} left for you to fill in"),
            many => format!("Copied as curl — {} secrets left as placeholders", many.len()),
        };
        self.set_status(&message, cx);
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        cx.global_mut::<Theme>().toggle();
        // Persisted, or the choice lasts until the next launch and no further — `main` used to
        // hardcode `Appearance::Dark` on every boot.
        let appearance = cx.global::<Theme>().appearance;
        crate::app_state::set_theme(cx, appearance);
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
            // The file now says what the buffer says, so this is the new clean state.
            view.baseline = spec;
            cx.notify();
        });
        // A request you just saved and cannot find in the tree reads as a save that failed.
        // Rescans on every save rather than splicing the one row in: a save can also *move*
        // a request into a directory that doesn't exist yet, and a splice would have to
        // reproduce `tree`'s ordering rules to put it in the right place.
        self.refresh_tree(cx);
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
        let tabs = self.tab_labels(cx);

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
            .on_action(cx.listener(Self::toggle_collection_panel))
            .on_action(cx.listener(Self::collection_next))
            .on_action(cx.listener(Self::collection_prev))
            .on_action(cx.listener(Self::collection_confirm))
            .on_action(cx.listener(Self::collection_collapse))
            .on_action(cx.listener(Self::collection_expand))
            .on_action(cx.listener(Self::open_workspace_menu))
            .on_action(cx.listener(Self::new_workspace))
            .on_action(cx.listener(Self::open_workspace))
            .on_action(cx.listener(Self::workspace_confirm))
            .on_action(cx.listener(Self::workspace_dismiss))
            .on_action(cx.listener(Self::workspace_browse))
            .on_action(cx.listener(Self::switch_workspace_action))
            .on_action(cx.listener(Self::forget_workspace_action))
            .on_action(cx.listener(Self::confirm_close))
            .on_action(cx.listener(Self::cancel_close))
            .on_action(cx.listener(Self::close_choice_next))
            .on_action(cx.listener(Self::close_choice_prev))
            .on_action(cx.listener(Self::collection_collapse_all))
            .on_action(cx.listener(Self::collection_expand_all))
            .on_action(cx.listener(Self::open_collection_menu))
            .on_action(cx.listener(Self::delete_request))
            .on_action(cx.listener(Self::confirm_delete_request))
            .on_action(cx.listener(Self::trash_request))
            .on_action(cx.listener(Self::duplicate_request))
            .on_action(cx.listener(Self::reveal_request))
            .on_action(cx.listener(Self::open_request_externally))
            .on_action(cx.listener(Self::copy_request_path))
            .on_action(cx.listener(Self::copy_request_relative_path))
            .on_action(cx.listener(Self::rename_request))
            .on_action(cx.listener(Self::commit_rename))
            .on_action(cx.listener(Self::cancel_rename))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::move_request))
            .on_action(cx.listener(Self::import_openapi))
            .on_action(cx.listener(Self::import_confirm))
            .on_action(cx.listener(Self::import_dismiss))
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
            .on_action(cx.listener(Self::toggle_response_view))
            .on_action(cx.listener(Self::find_in_body))
            .on_action(cx.listener(Self::body_find_next))
            .on_action(cx.listener(Self::body_find_prev))
            .on_action(cx.listener(Self::close_body_find))
            .on_action(cx.listener(Self::replace_next))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::find_in_response))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_prev))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::fold_all))
            .on_action(cx.listener(Self::unfold_all))
            .on_action(cx.listener(Self::response_row_next))
            .on_action(cx.listener(Self::response_row_prev))
            .on_action(cx.listener(Self::toggle_fold))
            .on_action(cx.listener(Self::scroll_left))
            .on_action(cx.listener(Self::scroll_right))
            .on_action(cx.listener(Self::scroll_start))
            .on_action(cx.listener(Self::open_row_menu))
            .on_action(cx.listener(Self::open_app_menu))
            .on_action(cx.listener(Self::menu_next))
            .on_action(cx.listener(Self::menu_prev))
            .on_action(cx.listener(Self::menu_confirm))
            .on_action(cx.listener(Self::menu_dismiss))
            .on_action(cx.listener(Self::copy_row_value))
            .on_action(cx.listener(Self::copy_row_path))
            .on_action(cx.listener(Self::copy_response))
            .on_action(cx.listener(Self::save_response))
            .on_action(cx.listener(Self::copy_as_curl))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::save_request))
            .on_action(cx.listener(Self::send_request))
            .on_action(cx.listener(Self::cancel_request))
            .on_action(cx.listener(Self::next_request_tab))
            .on_action(cx.listener(Self::prev_request_tab))
            .on_action(cx.listener(Self::show_headers_tab))
            .on_action(cx.listener(Self::show_params_tab))
            .on_action(cx.listener(Self::show_body_tab))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .text_sm()
            .relative()
            .child(crate::chrome::titlebar(
                title,
                self.panel_visible,
                &theme,
                window,
            ))
            // **The tab strip belongs to the editor area, not to the window.** It spanned the
            // full width for one slice, which put tabs above the collection panel — a strip of
            // open *buffers* drawn over a tree of saved *files*, describing something the panel
            // has nothing to do with. So the panel is a full-height column between the titlebar
            // and the status bar, and the strip sits inside the column to its right, the layout
            // every editor with a sidebar uses.
            //
            // The status bar still spans the window, which is the same convention rather than an
            // inconsistency: it describes the application, the strip describes one pane.
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    // Panel first: it is leftmost, and paint order decides hit-testing between
                    // siblings.
                    .children(
                        self.panel_visible
                            .then(|| crate::collection_panel::render(self, &theme, window, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            // The panel is a fixed width, so the panes are what must give when
                            // the window narrows. Without this the column's content sets a floor
                            // and the two together overflow instead.
                            .min_w(px(0.))
                            .overflow_hidden()
                            .children(tab_strip(tabs, &theme, cx))
                            .children(self.active()),
                    ),
            )
            .child(status_bar(
                focused_region,
                status_message,
                cookies,
                self.environment.clone().map(SharedString::from),
                &theme,
                window,
            ))
            // Above the panes, below the resize edges.
            .children(self.picker.as_ref().map(|state| state.picker.clone()))
            .children(self.settings.as_ref().map(|state| state.panel.clone()))
            .children(self.import.as_ref().map(|state| state.panel.clone()))
            .children(self.new_workspace_panel.as_ref().map(|state| state.panel.clone()))
            .children(self.menu.as_ref().map(|state| state.menu.clone()))
            // Built here rather than held as an `Entity`: it owns no input and no state beyond
            // which button is selected, so it is plain workspace state like `RenameState`.
            .children(
                self.close_confirm
                    .as_ref()
                    .map(|state| crate::close_panel::render(state, &theme, cx)),
            )
            // Last, so the edge strips sit above the panes for hit-testing.
            .children(crate::chrome::resize_handles(window))
    }

}


/// Everything else derives from this; architecture.md §12 has why it is explicit.
const TAB_LABEL_WIDTH: f32 = 131.;
/// What fits in `TAB_LABEL_WIDTH` in the UI font at `text_xs`. Change one, revisit the other.
const TAB_LABEL_CHARS: usize = 22;

#[cfg(test)]
pub(crate) fn tab_label_width() -> f32 {
    TAB_LABEL_WIDTH
}
/// `size(px(16.))` on the close button below.
const TAB_CLOSE_WIDTH: f32 = 16.;
/// `px_3` either side, `gap_1` between label and button, `border_x_2` either side on the inner
/// element, and the 1px `border_r_1` dividing one tab from the next.
const TAB_CHROME_WIDTH: f32 = 12. * 2. + 4. + 2. * 2. + 1.;
const TAB_WIDTH: f32 = TAB_LABEL_WIDTH + TAB_CLOSE_WIDTH + TAB_CHROME_WIDTH;

/// The strip of open buffers.
///
/// Hidden entirely at one buffer: a single tab is a row of chrome that says nothing, and
/// the window title already names the request. It appears the moment there's a choice to
/// make, which is also the moment it starts carrying information.
fn tab_strip(
    tabs: Vec<(usize, SharedString, bool, bool)>,
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
            .children(tabs.into_iter().map(|(ix, label, active, dirty)| {
                div()
                    .id(("tab", ix))
                    // So a test can click a real tab. The click handler sits out here while the
                    // label sits in a child, and an ancestor's Bubble-phase handler does fire for
                    // a click on its child — but that is worth pinning rather than assuming.
                    .debug_selector(move || format!("tab-{ix}"))
                    // The group is the whole tab, not the button: hovering anywhere reveals the
                    // ×, and an `svg()` cannot be reached by an ancestor's `hover`.
                    .group(crate::ui::ICON_GROUP)
                    .flex_none()
                    // Fixed, so a tab doesn't move under the cursor when a URL is edited.
                    .w(px(TAB_WIDTH))
                    // A long label must clip, not push its neighbours off the strip.
                    .overflow_hidden()
                    // The 1px rule dividing one tab from the next.
                    .border_r_1()
                    .border_color(theme.border)
                    .cursor_pointer()
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
                    // The active marker is a *nested* element, and it has to be. A div carries
                    // one `border_color` for all four sides — widths are per-side, colour is
                    // not — so the accent bracket and the neutral divider above cannot share an
                    // element. They did, and the second call silently won: the active tab drew
                    // its right divider in accent, and every inactive tab drew its divider in
                    // `bg_panel`, which is to say not at all. Two colours, two elements.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .size_full()
                            .px_3()
                            .py_1()
                            // Accent down both edges rather than a top rule. Inactive tabs
                            // keep the width and paint it in the strip's own background, so
                            // switching never reflows the label by 4px.
                            .border_x_2()
                            .border_color(if active { theme.accent } else { theme.bg_panel })
                            .bg(if active { theme.bg } else { theme.bg_panel })
                            .text_xs()
                            .text_color(if active { theme.text } else { theme.text_muted })
                            .hover(|style| style.bg(theme.bg_hover))
                            // Drives the dot/× swap below. `GroupBounds::get` takes the
                            // innermost open group of that name and sibling tabs push and pop
                            // separately, so one shared constant still resolves per tab —
                            // hovering one does not light up the rest.
                            .group(crate::ui::ICON_GROUP)
                            .child(
                                div()
                                    .w(px(TAB_LABEL_WIDTH))
                                    .debug_selector(move || format!("tab-label-{ix}"))
                                    // Backstop for wide glyphs only; `elide` does the real work.
                                    // `truncate()` alone cannot be relied on — see CLAUDE.md.
                                    .truncate()
                                    .child(label),
                            )
                            .child(
                                div()
                                    .id(("tab-close", ix))
                                    .debug_selector(move || format!("tab-close-{ix}"))
                                    // Always painted, so revealing the × never reflows the label.
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(TAB_CLOSE_WIDTH))
                                    .rounded_md()
                                    .relative()
                                    .hover(|style| style.bg(theme.bg_hover))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(
                                            move |workspace, _: &MouseDownEvent, window, cx| {
                                                // A clickable inside a clickable. Currently
                                                // unobservable — the suite passes without it.
                                                cx.stop_propagation();
                                                // Closes *that* tab, not the active one.
                                                workspace.activate(ix, window, cx);
                                                workspace.close_tab(&CloseTab, window, cx);
                                            },
                                        ),
                                    )
                                    // The dirty dot sits *in* the close slot and hovering the
                                    // tab trades it for the ×, which is what every editor does.
                                    // Stacked rather than swapped in Rust: hover is a paint-time
                                    // style, so both are always painted and only the colours
                                    // move — which is also what keeps the label from shifting.
                                    .child(
                                        div()
                                            .debug_selector(move || format!("tab-dirty-{ix}"))
                                            .absolute()
                                            .inset_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                div()
                                                    .size(px(8.))
                                                    .rounded_full()
                                                    .bg(if dirty {
                                                        theme.text_muted
                                                    } else {
                                                        gpui::transparent_black()
                                                    })
                                                    .group_hover(
                                                        crate::ui::ICON_GROUP,
                                                        |style| {
                                                            style.bg(gpui::transparent_black())
                                                        },
                                                    ),
                                            ),
                                    )
                                    .child(crate::ui::glyph(
                                        crate::ui::Icon::Close,
                                        // Transparent, not absent — the slot must keep its size.
                                        // A dirty tab hides it so the dot shows through; a clean
                                        // inactive one hides it until the tab is hovered.
                                        if active && !dirty {
                                            theme.text_muted
                                        } else {
                                            gpui::transparent_black()
                                        },
                                        theme.text,
                                        crate::ui::GLYPH,
                                    )),
                            ),
                    )
            })),
    )
}

/// A `new issue` link carrying the two facts a bug report almost never includes.
///
/// Pure and separate from the menu so it can be tested: the body has to survive
/// percent-encoding, and a broken query string produces a GitHub page with an empty form
/// rather than an error anyone would notice.
pub(crate) fn issue_url(repo: &str) -> String {
    let body = format!(
        "**Zuno:** {}\n**Platform:** {} {}\n\n**What happened**\n\n\n**What you expected**\n\n\n**Steps to reproduce**\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("body", &body)
        .finish();
    format!("{repo}/issues/new?{query}")
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
///
/// gpui's own spelling, so it matches the command palette's trailing column. For prose that reads
/// "press X to do Y", use [`keybinding_label`].
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

/// The same binding spelled `Ctrl+Shift+H`, for hints written into a sentence.
///
/// **Why a second form rather than one.** gpui renders `ctrl-shift-h`, which is right in the
/// palette's shortcut column and wrong inside "No headers — press … to add". Every such hint in the
/// app was a hardcoded literal in that conventional spelling, and the docs told the story of the
/// in-flight pane's stale `Ctrl+C` as though the class were closed — while `keybinding_hint` had
/// exactly one caller and ten literals sat beside it. Changing them all to gpui's spelling would
/// have been a visible regression for the sake of the fix, so the fix brings its own formatter.
///
/// Built from the `Keystroke` rather than by reformatting `to_string()`: parsing that output means
/// splitting on `-`, which a binding on the `-` key itself would break.
///
/// Empty when the action is unbound, and **callers must check** — a sentence with a hole where the
/// key should be is worse than one that never offered a key. See `request_pane::hint_row`.
/// The keystroke for an action *as reached from `focus`*, spelled for display.
///
/// **`keybinding_label` cannot answer this, and the difference is not a nuance.**
/// `Window::bindings_for_action` matches against `rendered_frame.dispatch_tree.context_stack`,
/// which is a **build-time stack**: `push_node` pushes a context and `pop_node` pops it, so by
/// the time a frame is finished it is empty. An empty stack matches only bindings registered
/// with `None` context — so every *scoped* binding looks unbound.
///
/// That is why the row menus have advertised nothing since they shipped. `Copy value` is
/// `ctrl-c` in `ResponsePane`, `Copy path` is `alt-c`, `Rename` is `f2` in `CollectionPanel`:
/// all scoped, all resolving to an empty column, in a menu whose stated purpose is to teach the
/// keystroke. `bindings_for_action_in` rebuilds the stack from a focus handle instead, which is
/// exactly the question a menu is asking — "what does this key mean *in the pane these verbs
/// act on*". A global binding still resolves, because a `None` predicate matches any stack, so
/// this is one path rather than a special case.
pub fn keybinding_label_in(
    action: &dyn gpui::Action,
    focus: &FocusHandle,
    window: &Window,
) -> String {
    match window.bindings_for_action_in(action, focus).first() {
        Some(binding) => spell(binding),
        None => String::new(),
    }
}

pub fn keybinding_label(action: &dyn gpui::Action, window: &Window) -> String {
    match window.bindings_for_action(action).first() {
        Some(binding) => spell(binding),
        None => String::new(),
    }
}

/// Spell one binding the way UI copy does — `Ctrl+Shift+H`, not gpui's `ctrl-shift-h`.
fn spell(binding: &gpui::KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|keystroke| {
            let modifiers = keystroke.modifiers();
            let mut parts: Vec<String> = Vec::new();
            if modifiers.control {
                parts.push("Ctrl".into());
            }
            if modifiers.alt {
                parts.push("Alt".into());
            }
            // control, alt, platform, shift — gpui's own order in `display_modifiers`, matched by
            // inspection of the vendored source.
            //
            // **Not covered by a test, and it's worth knowing why.** The round-trip check in
            // `keybinding_label_matches_the_keymap` lowercases this back into gpui's spelling and
            // compares, but a swap here is invisible to it: telling the two orders apart needs a
            // binding with *both* platform and shift, and Zuno has none. Worse, one could never be
            // compared that way anyway — gpui renders the platform modifier as the glyph `❖` on
            // Linux, which does not lowercase into `super`. Reordering these lines was tried
            // deliberately and the suite stayed green.
            if modifiers.platform {
                parts.push("Super".into());
            }
            if modifiers.shift {
                parts.push("Shift".into());
            }
            parts.push(capitalize(keystroke.key()));
            parts.join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `"No headers — Ctrl+Shift+H to add"`, with every key read from the keymap.
///
/// **The `is_empty` filter is the whole reason this is one function and not four.** An unbound
/// action makes `keybinding_label` return empty, and interpolating that yields
/// `"No headers —  to add"` — a keymap-derived hint failing uglier than the literal it replaced.
/// The guard was written out at two call sites before this existed, which is exactly how the
/// `modal_open` checks drifted: repeated logic diverges, and one forgotten copy is the bug.
///
/// A clause whose action is unbound is dropped; if none survive, so is the dash.
pub fn hint_sentence(lead: &str, clauses: &[(&dyn gpui::Action, &str)], window: &Window) -> String {
    let rendered: Vec<String> = clauses
        .iter()
        .filter_map(|(action, verb)| {
            let key = keybinding_label(*action, window);
            (!key.is_empty()).then(|| format!("{key} {verb}"))
        })
        .collect();

    if rendered.is_empty() {
        lead.to_string()
    } else {
        format!("{lead} — {}", rendered.join(", "))
    }
}

/// `h` -> `H`, `enter` -> `Enter`, `,` -> `,`.
///
/// Only the first character, and only if it has an uppercase form — so punctuation keys like
/// `ctrl-,` come through untouched rather than being mangled.
fn capitalize(key: &str) -> String {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn status_bar(
    focused_region: SharedString,
    message: Option<SharedString>,
    cookies: bool,
    environment: Option<SharedString>,
    theme: &Theme,
    window: &Window,
) -> impl IntoElement {
    // Each hint names a key the keymap actually holds, and each is now *clickable* — it was a
    // single dead string advertising four shortcuts, which is a strange thing for the one strip
    // whose whole job is telling you what you can do. Unbound actions drop out rather than
    // printing an empty slot, so this shrinks instead of lying.
    // A local generic fn rather than a `Vec<Box<dyn Action>>`: `text_action` needs `A: Clone`, and
    // a boxed trait object isn't `Clone` — `boxed_clone` is the trait's own answer to that, but it
    // hands back another box, not an `A`. Four monomorphised calls is the simpler shape.
    fn hint<A: gpui::Action + Clone + 'static>(
        id: &'static str,
        what: &'static str,
        label: &'static str,
        action: A,
        theme: &Theme,
        window: &Window,
    ) -> Option<gpui::AnyElement> {
        let key = keybinding_label(&action, window);
        (!key.is_empty()).then(|| {
            crate::ui::text_action(id, format!("{key} {what}").into(), label, action, theme)
                .into_any_element()
        })
    }

    let hints = [
        hint("hint-find", "find", "Find request", OpenRequest, theme, window),
        hint("hint-commands", "commands", "Command palette", OpenPalette, theme, window),
        hint("hint-env", "env", "Switch environment", SwitchEnvironment, theme, window),
        hint("hint-send", "send", "Send request", SendRequest, theme, window),
    ];

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
                // Clickable in both states, which is the point: the badge was information only,
                // so with no environment selected there was nothing on screen leading to the
                // switcher at all. Named `environment-badge` either way so a test doesn't have to
                // know which branch it's in.
                .child(match environment {
                    Some(name) => crate::ui::text_action(
                        "environment-badge",
                        name,
                        "Switch environment",
                        SwitchEnvironment,
                        theme,
                    )
                    .into_any_element(),
                    None => crate::ui::icon_button(
                        "environment-badge",
                        crate::ui::Icon::Globe,
                        "Switch environment",
                        SwitchEnvironment,
                        theme,
                    )
                    .into_any_element(),
                })
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
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .flex_none()
                        .children(hints.into_iter().flatten()),
                ),
        )
}
