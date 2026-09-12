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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    ClipboardItem, ScrollHandle, Styled, Subscription, Task, UniformListScrollHandle, Window,
    div, point, px,
};
use zuno_core::{
    Environment, ProxyMode, RawKind, RequestId, RequestSpec, Resolver, collection, curl,
    environment,
};
use zuno_core::collection::{Node, NodeKind};

use crate::actions::{
    CopyInstallCommand, DismissUpdate, OpenUpdateMenu,
    SuggestConfirm, SuggestDismiss, SuggestNext, SuggestPrev,
    AddFormField, AddHeader, AddMultipartField, AddQuery, CancelRequest, ChooseBodyFile,
    AddAssertion, AddCapture, AssertValue, CaptureValue, CycleAssertOp, EditEnvironments,
    EnvConfirm, EnvDismiss, EnvNewEnvironment, ShowAssertTab,
    AddToFlow, EditFlows, FlowConfirm, FlowDismiss, FlowNew, FlowNext, FlowPrev, FlowRename,
    FlowStepDown, FlowStepNext, FlowStepPrev,
    FlowStepRemove, FlowStepUp, FlowTrash, OpenDefaults, RunDismiss, RunFlow, RunFolder,
    EnvNewVariable, EnvNext, ShowCaptureTab, ToggleCaptureSecret,
    EnvPrev, EnvRemoveVariable, EnvRenameEnvironment, EnvToggleSecret, EnvTrashEnvironment,
    ClearCookies, CloseTab, CopyResponse, CopyRowPath, CopyRowValue, MenuConfirm, MenuDismiss,
    MenuNext, MenuPrev, OpenRowMenu, ResponseRowNext, ResponseRowPrev, ScrollLeft, ScrollRight,
    ScrollStart, ToggleFold, FocusBody, FocusNext, FocusPrev, FocusResponse, FocusUrl, FoldAll, ImportCurl, NewTab, NextRequestTab, NextTab,
    OpenBodyType, PrevRequestTab, OpenMethod, OpenPalette, OpenRequest, OpenSettings, PickerConfirm, PickerDismiss,
    OpenAppMenu, PickerNext, PickerPrev, PrevTab, Quit, RemoveRow, SaveRequest, SaveResponse, SendRequest,
    SettingConfirm, SettingDecrease, SettingIncrease, SettingNext, SettingPrev, SettingsDismiss,
    BodyFindNext, BodyFindPrev, CloseBodyFind, CloseFind, CopyAsCurl, FindInBody,
    FindInResponse, FindNext, FindPrev, ReplaceAll, ReplaceNext,
    CertsConfirm, CertsDismiss, CertsNext, CertsPrev, CertsRemove, ChooseClientCert,
    ChooseRootCa, OpenCertificates,
    CloseAllTabs, CloseOtherTabs, CloseTabsToTheRight, OpenTabMenu, RemoveProxy, SetProxy, ShowBodyTab, ShowHeadersTab, ShowHistory, ShowParamsTab, SwitchEnvironment, ToggleRow, ToggleTheme, UnfoldAll,
    NextResponseTab, PrevResponseTab, ShowResponseBody, ShowResponseDiff, ShowResponseHeaders,
    ShowResponseTiming, ToggleHtmlView,
    CollectionCollapse, CollectionConfirm, CollectionExpand, CollectionNext, CollectionPrev,
    ConfirmDeleteRequest, DeleteRequest, OpenCollectionMenu, ToggleCollectionPanel,
    CancelClose, CancelRename, CloseChoiceNext, CloseChoicePrev, CollectionCollapseAll,
    ForgetWorkspace, NewWorkspace, OpenWorkspace, OpenWorkspaceMenu, SwitchWorkspace,
    WorkspaceBrowse,
    WorkspaceConfirm, WorkspaceDismiss,
    CollectionExpandAll, CommitRename, ConfirmClose, CopyRequestPath,
    CopyRequestRelativePath, DuplicateRequest,
    FormatBody, ImportConfirm, ImportDismiss, ImportBrowse, OpenPartKindMenu, ImportDocument, MinifyBody, MoveRequest,
    NewFolder, NewRequest, OpenRequestExternally,
    RenameRequest, RevealRequest, TrashRequest,
};
use crate::engine::ActiveEngine;
use crate::context_menu;
use crate::picker;
use crate::settings_panel::{Scope, SettingsEvent, SettingsPanel};
use crate::request_view::{BodyType, RequestTab, RequestView, ResponseView, RowKind};
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
    /// Whether `globals.json` has anything in it.
    ///
    /// Cached rather than read in `render`: the badge has to say whether *anything* is
    /// substituting, and answering that means opening and parsing a file — invariant 3. Refreshed
    /// wherever it can change, which since the editor exists is three places: boot, a workspace
    /// swap, and the editor closing. An external edit while Zuno is open leaves it stale, the
    /// same way every other read-at-switch does.
    globals_active: bool,

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
    /// The panel's width as the user left it, **unclamped**.
    ///
    /// Stored raw and clamped at read through `clamped_panel_width`, because the ceiling
    /// depends on the window: clamping on the way in would permanently shrink a width just
    /// because the window happened to be narrow when it was set, and the user would find it
    /// changed after maximizing.
    panel_width: f32,
    /// **An index into `tree`, not into `tree_visible`.** Folding rewrites `tree_visible`
    /// underneath the selection, so a visible index would silently retarget it at whatever
    /// row slid into that slot — the lesson the response viewer's row cursor already records
    /// (architecture.md §6). Translation happens at render and scroll, nowhere else.
    pub(crate) panel_selection: Option<usize>,
    pub(crate) panel_scroll: UniformListScrollHandle,
    /// The tab strip's horizontal scroll, held so the chevrons can drive it.
    ///
    /// The strip has scrolled since it was built, but only by wheel — with nothing on screen
    /// saying so, a tab past the right edge was unreachable by mouse. A handle is the only way
    /// in: `set_offset` needs one, and gpui re-clamps `offset.x` to `[-max, 0]` in its own
    /// prepaint, so a caller writing to it needs no clamp of its own.
    /// The proxy the environment names, read **once at boot**.
    ///
    /// Cached rather than read in `render` — env vars cannot change under a running process,
    /// and the status bar asks this every frame. Same shape as `globals_active`.
    /// Where a tab was right-clicked. Taken by `OpenTabMenu`, so a stale anchor cannot place
    /// a later menu.
    /// Held for the same reason `workspace_prompt` is: dropping the task cancels the dialog.
    cert_prompt: Option<Task<()>>,
    certs: Option<crate::cert_panel::CertPanel>,
    tab_menu_anchor: Option<gpui::Point<gpui::Pixels>>,
    system_proxy: Option<String>,
    pub(crate) tab_scroll: ScrollHandle,
    /// The running chevron animation. Held so a second click **replaces** it — dropping a
    /// `Task` cancels it, so two animations can never fight over the same offset.
    tab_scroll_anim: Option<Task<()>>,
    /// Where the running animation is heading, so rapid clicks accumulate rather than restart.
    /// Three quick presses travel three tabs; reading the live offset instead would make each
    /// click re-aim at wherever the last one had got to.
    tab_scroll_target: Option<f32>,
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
    new_node: Option<NewNodeState>,
    /// The OpenAPI import modal, while it's open.
    import: Option<ImportState>,
    /// Holding the task is what keeps a spec fetch alive; dropping it cancels.
    import_task: Option<Task<()>>,
    /// Held for the same reason every other task field is: dropping a `Task` cancels it, so a
    /// format that outlived its own local would silently never land.
    body_format: Option<Task<()>>,
    /// Where the panel was right-clicked, in window coordinates.
    ///
    /// Kept rather than taken when the first menu opens, because the confirmation is a *second*
    /// menu that has to appear in the same place — a "delete this?" that jumps across the
    /// window reads as a different question about something else.
    collection_menu_at: Option<gpui::Point<gpui::Pixels>>,
    /// The unsaved-changes prompt, when one is open.
    close_confirm: Option<crate::close_panel::CloseConfirm>,
    new_workspace_panel: Option<WorkspacePanelState>,
    environment_panel: Option<EnvPanelState>,
    run: Option<RunState>,
    flows: Option<FlowPanelState>,
    /// The folder dialog behind New and Open. Held because dropping the task cancels it.
    workspace_prompt: Option<Task<()>>,
    /// What the release check found. See `update.rs` — it is a notice, never an installer.
    update: crate::update::Update,
    /// Held because dropping a `Task` cancels it.
    update_task: Option<Task<()>>,
    /// The header-name suggestion list: which row it belongs to, and which entry the user has
    /// explicitly moved to.
    ///
    /// **`None` for the highlight is the whole safety rule.** Typing never highlights anything,
    /// so `Enter` on a half-typed custom header does nothing rather than silently replacing it
    /// with whatever ranked first. Only `up`/`down` set it.
    ///
    /// The list itself is *derived* in render from the focused cell's text rather than stored —
    /// `headers::suggestions` is pure and cheap, and a stored copy is a mirror that can
    /// disagree with the box it describes.
    suggest: Option<(usize, Option<usize>)>,
    /// The row whose list was dismissed with `escape`, so it stays shut until focus moves.
    suggest_dismissed: Option<usize>,
    /// The open multipart type select: which row, and where its chip was.
    part_select: Option<(usize, gpui::Point<gpui::Pixels>)>,
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

/// The flow editor and the subscription that lets a rename or a removal reach the workspace.
struct FlowPanelState {
    panel: Entity<crate::flow_panel::FlowPanel>,
    _subscription: Subscription,
}

/// The run report, its subscription, and the flag that stops the run behind it.
struct RunState {
    panel: Entity<crate::run_panel::RunPanel>,
    /// Read by the runner between steps *and* while a request is in flight, so a stuck request
    /// does not hold the stop button hostage.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    _task: Task<()>,
    _subscription: Subscription,
}

/// Same pairing again, for the environment editor.
struct EnvPanelState {
    panel: Entity<crate::environment_panel::EnvironmentPanel>,
    _subscription: Subscription,
}

/// A folder being named.
///
/// Deliberately **not** a phantom row spliced into the tree. `tree_visible` indexes into `tree`,
/// so a row that exists only in the UI would have to be threaded through the fold walk, the
/// selection clamp and every index translation to serve one transient input. This is a single
/// input drawn under the panel's header, labelled with where the folder will land — which also
/// says the destination outright instead of asking the reader to count indent levels.
struct NewNodeState {
    /// Which verb opened the box. One state and one row for both, rather than a second field
    /// and a second copy of the row-placement rules — those are the fiddly half (a collapsed
    /// parent, a visible index, the depth) and having them twice is having them drift.
    kind: NewNode,
    parent: PathBuf,
    /// Where the box sits in the list, as a *visible* index — the row it displaces.
    insert_at: usize,
    /// One level deeper than its parent, so it lines up with the folder's future contents.
    depth: u16,
    input: Entity<crate::input::TextInput>,
    _blur: Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NewNode {
    Request,
    Folder,
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
        let session_width = session.panel_width;
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
            body_format: None,
            session_save: None,
            body_file_prompt: None,
            globals_active: globals_has_values(cx),
            environment,
            tree: Vec::new(),
            tree_visible: Vec::new(),
            collapsed: HashSet::new(),
            tree_scanned: false,
            tree_skipped: 0,
            panel_visible: session_panel,
            panel_width: session_width,
            panel_selection: None,
            panel_scroll: UniformListScrollHandle::new(),
            cert_prompt: None,
            certs: None,
            tab_menu_anchor: None,
            system_proxy: [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
            ]
            .into_iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty()),
            tab_scroll: ScrollHandle::new(),
            tab_scroll_anim: None,
            tab_scroll_target: None,
            panel_focus: cx.focus_handle(),
            tree_scan: None,
            renaming: None,
            new_node: None,
            import: None,
            import_task: None,
            collection_menu_at: None,
            close_confirm: None,
            new_workspace_panel: None,
            environment_panel: None,
            run: None,
            flows: None,
            workspace_prompt: None,
            update: crate::update::Update::Unknown,
            update_task: None,
            suggest: None,
            suggest_dismissed: None,
            part_select: None,
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
        self.globals_active = globals_has_values(cx);
        self.panel_visible = session.collection_panel;
        self.panel_width = session.panel_width;

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
            || self.certs.is_some()
            || self.new_workspace_panel.is_some()
            || self.environment_panel.is_some()
            || self.run.is_some()
            || self.flows.is_some()
    }

    #[cfg(test)]
    pub fn env_panel_for_test(
        &self,
    ) -> Option<Entity<crate::environment_panel::EnvironmentPanel>> {
        self.env_panel()
    }

    #[cfg(test)]
    pub fn badge_for_test(&self) -> SharedString {
        environment_badge(self.environment.as_deref(), self.globals_active).0
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
        // Captured here rather than re-read below, so the borrow of `self.views` ends before
        // the two `&mut self` calls.
        let file = view.read(cx).path.clone();
        window.focus(&url_focus);

        // **Both of these live here because `activate` is the one funnel.** Every switch —
        // the four verbs, a tab click, middle-click, the picker, the collection panel — already
        // comes through it (§12), so a switching path added later inherits them instead of
        // having to remember.
        self.reveal_active_tab(window, cx);
        self.follow_active_in_panel(file, cx);

        // Unlike `focus_region`, this needs the notify: the strip and the title change
        // even when `window.focus` finds the handle already focused.
        cx.notify();
    }

    /// Bring the active tab into the strip's viewport.
    ///
    /// **Why it is not enough that the strip scrolls.** A new buffer is appended, so once the
    /// tabs overflow, `Ctrl+T` puts the new tab past the right edge and nothing on screen
    /// changes — pressing it repeatedly looks like a dead key. The same applies to `Ctrl+Tab`
    /// onto a tab that is already off-screen, which is why this is on every activation rather
    /// than on tab creation.
    ///
    /// Animated through the same easing the chevrons use: noticing that something happened is
    /// the entire point, and a jump is easier to miss than a slide.
    fn reveal_active_tab(&mut self, window: &Window, cx: &mut Context<Self>) {
        // The strip is not drawn at one buffer, so there is nothing to reveal.
        if self.views.len() < 2 {
            return;
        }

        let panel = if self.panel_visible {
            self.clamped_panel_width(window)
        } else {
            0.
        };
        let available = f32::from(window.viewport_size().width) - panel;

        if let Some(target) = reveal_offset(
            self.active_ix,
            self.views.len(),
            available,
            f32::from(self.tab_scroll.offset().x),
        ) {
            self.animate_tabs_to(target, cx);
        }
    }

    /// Move the panel's selection onto the active buffer's file.
    ///
    /// The panel already drives the strip — clicking a row activates that buffer — and this is
    /// the other direction, which was missing: switching tabs left the tree highlighting
    /// whatever had last been clicked in it.
    fn follow_active_in_panel(&mut self, file: Option<PathBuf>, cx: &mut Context<Self>) {
        // **A buffer with no file leaves the selection alone rather than clearing it.** The
        // panel keeps your place in a large tree, and — the reason that matters — New request
        // and New folder both read this selection to decide where they create things, so
        // clearing it on every scratch tab would silently move them to the collection root.
        let Some(file) = file else { return };
        let Some(ix) = self.tree.iter().position(|node| node.path == file) else {
            return;
        };

        // Expand first, then select. A selection on a row nothing paints is a cursor the reader
        // has lost — `rebuild_tree_visible` says so, and collapse-all had to learn it. Ancestors
        // rather than the file itself: `skip(1)` drops the file, whose own path is never a fold
        // key.
        let mut unfolded = false;
        for ancestor in file.ancestors().skip(1) {
            unfolded |= self.collapsed.remove(ancestor);
        }
        if unfolded {
            self.rebuild_tree_visible();
        }

        self.panel_selection = Some(ix);
        // `uniform_list` addresses items by *visible* index, so the row index has to be
        // translated — with anything folded above the target the two diverge.
        if let Some(pos) = self.tree_visible.iter().position(|&v| v == ix) {
            self.panel_scroll
                .scroll_to_item(pos, gpui::ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// The panel's rows as `(depth, name, is_directory)`, for tests.
    ///
    /// Reads the real state rather than the last painted frame: `cx.debug_bounds` reports what
    /// was drawn *previously*, so `is_none()` proves nothing about a row that has just been
    /// folded away — four context-menu tests already made that mistake.
    #[cfg(test)]
    pub(crate) fn certs_open(&self) -> bool {
        self.certs.is_some()
    }

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

    /// Whether the active tab is inside the strip's viewport, for tests.
    ///
    /// Asks `reveal_offset` the same question `activate` does, from live state — so it pins the
    /// *wiring*, that `activate` actually reveals, while `reveal_offset`'s own unit tests pin
    /// the arithmetic. Neither would catch the other's failure alone.
    #[cfg(test)]
    pub(crate) fn active_tab_in_view(&self, window: &Window) -> bool {
        let panel = if self.panel_visible {
            self.clamped_panel_width(window)
        } else {
            0.
        };
        reveal_offset(
            self.active_ix,
            self.views.len(),
            f32::from(window.viewport_size().width) - panel,
            f32::from(self.tab_scroll.offset().x),
        )
        .is_none()
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
            settings: crate::app_state::defaults(cx),
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

        match state.choice {
            Choice::Cancel => {
                if let Some(focus) = state.restore_focus {
                    window.focus(&focus);
                }
                cx.notify();
            }
            // The old `active_ix != state.ix` guard is gone because it cannot be needed any
            // more: `close_targets` resolves each buffer by id, so acting on the wrong one is
            // not expressible rather than merely checked for.
            Choice::Save => self.close_targets(state.targets, true, window, cx),
            Choice::Discard => self.close_targets(state.targets, false, window, cx),
        }
    }

    /// Close each target, optionally saving the dirty ones first.
    ///
    /// A failed save **keeps that buffer open** — the rule the single-buffer prompt already
    /// followed, and it matters more in a batch: closing anyway would discard exactly the work
    /// the person asked to keep, for the one request whose write failed.
    fn close_targets(
        &mut self,
        targets: Vec<gpui::EntityId>,
        save_dirty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut kept = 0;
        for id in targets {
            let Some(ix) = self.views.iter().position(|view| view.entity_id() == id) else {
                continue;
            };
            // `save_request` and `force_close_tab` both act on the active buffer, so each
            // target becomes active in turn.
            self.activate(ix, window, cx);

            if save_dirty && self.views[ix].read(cx).is_dirty(cx) {
                self.save_request(&SaveRequest, window, cx);
                let saved = self
                    .active()
                    .is_some_and(|view| !view.read(cx).is_dirty(cx));
                if !saved {
                    kept += 1;
                    continue;
                }
            }
            self.force_close_tab(window, cx);
        }

        if kept > 0 {
            let what = if kept == 1 { "request" } else { "requests" };
            self.set_status(&format!("Could not save {kept} {what}; left open"), cx);
        }
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.is_empty() || self.modal_open() {
            return;
        }
        // One funnel with the batch verbs, so the prompt cannot behave differently depending on
        // how many tabs you asked to close.
        let Some(id) = self.active().map(|view| view.entity_id()) else {
            return;
        };
        self.close_many(vec![id], window, cx);
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
            let spec = RequestSpec {
                settings: crate::app_state::defaults(cx),
                ..RequestSpec::default()
            };
            self.open(spec, window, cx);
            return;
        }

        // Closing the last tab in the strip moves left; anything else keeps the index,
        // which now points at what was to the right — the behaviour every editor has.
        let next = self.active_ix.min(self.views.len() - 1);
        self.activate(next, window, cx);
    }

    /// Close every target, asking once first if any of them have unsaved changes.
    ///
    /// **One prompt for the whole batch**, not one per dirty buffer: closing ten tabs with four
    /// unsaved would otherwise stack four modals with no way to see how many were coming. The
    /// prompt names the request when exactly one is unsaved and counts them otherwise.
    ///
    /// Targets are entity ids, not indices — every close renumbers `views`, so a list of indices
    /// would aim at whatever slid into each slot. Resolving the position fresh per target also
    /// makes the order irrelevant.
    fn close_many(
        &mut self,
        targets: Vec<gpui::EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty: Vec<gpui::EntityId> = targets
            .iter()
            .copied()
            .filter(|id| {
                self.views
                    .iter()
                    .find(|view| view.entity_id() == *id)
                    .is_some_and(|view| view.read(cx).is_dirty(cx))
            })
            .collect();

        if dirty.is_empty() {
            self.close_targets(targets, false, window, cx);
            return;
        }

        let label = match dirty.as_slice() {
            [only] => self
                .views
                .iter()
                .find(|view| view.entity_id() == *only)
                .map(|view| view.read(cx).label(cx))
                .unwrap_or_else(|| SharedString::from("This request")),
            // Unused when more than one is unsaved: the panel counts them instead.
            _ => SharedString::from("These requests"),
        };

        let restore = Some(window.focused(cx).unwrap_or_else(|| self.focus_handle.clone()));
        let state = crate::close_panel::CloseConfirm::new(
            targets,
            dirty.len(),
            label,
            restore,
            cx,
        );
        let focus = state.focus_handle.clone();
        self.close_confirm = Some(state);
        window.focus(&focus);
        cx.notify();
    }

    /// The ids of every buffer except the active one.
    fn other_tab_ids(&self) -> Vec<gpui::EntityId> {
        self.views
            .iter()
            .enumerate()
            .filter(|(ix, _)| *ix != self.active_ix)
            .map(|(_, view)| view.entity_id())
            .collect()
    }

    fn close_other_tabs(&mut self, _: &CloseOtherTabs, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let targets = self.other_tab_ids();
        self.close_many(targets, window, cx);
    }

    fn close_tabs_to_the_right(
        &mut self,
        _: &CloseTabsToTheRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        let targets = self
            .views
            .iter()
            .skip(self.active_ix + 1)
            .map(|view| view.entity_id())
            .collect();
        self.close_many(targets, window, cx);
    }

    fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // No special case for emptiness: `force_close_tab` opens a fresh buffer when the last
        // one goes.
        let targets = self.views.iter().map(|view| view.entity_id()).collect();
        self.close_many(targets, window, cx);
    }

    /// The tab strip's context menu.
    ///
    /// Right-click activates the tab first, so every row here acts on the active buffer and
    /// `Close` needs no action of its own — the same two steps the `×` and middle-click take.
    fn open_tab_menu(&mut self, _: &OpenTabMenu, window: &mut Window, cx: &mut Context<Self>) {
        // Consumed either way, or a refused open would place the next menu at this click.
        let at = self.tab_menu_anchor.take();
        if self.modal_open() {
            return;
        }
        let Some(at) = at else { return };

        let focus = self.focus_handle.clone();
        let mut items = vec![context_menu::MenuItem::new("Close", CloseTab, &focus, window)];

        // Rows adapt rather than grey out, the rule the response row menu follows: neither of
        // these means anything at one buffer, and "to the right" means nothing on the last tab.
        if self.views.len() > 1 {
            items.push(context_menu::MenuItem::new(
                "Close others",
                CloseOtherTabs,
                &focus,
                window,
            ));
        }
        if self.active_ix + 1 < self.views.len() {
            items.push(context_menu::MenuItem::new(
                "Close to the right",
                CloseTabsToTheRight,
                &focus,
                window,
            ));
        }
        items.push(context_menu::MenuItem::new(
            "Close all",
            CloseAllTabs,
            &focus,
            window,
        ));

        let mut rows: Vec<context_menu::MenuRow> =
            items.into_iter().map(Into::into).collect();
        rows.push(context_menu::MenuRow::Separator);
        rows.push(
            context_menu::MenuItem::new("Copy as curl", CopyAsCurl, &focus, window).into(),
        );

        self.show_menu(rows, at, Some(focus), window, cx);
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

    /// The panel's width as it should actually be drawn this frame.
    ///
    /// The clamp lives here rather than at the drag, so a stored width that no longer fits —
    /// restored onto a smaller screen, or a window since dragged narrow — is reined in on the
    /// frame that draws it instead of eating the request pane until someone resizes the panel
    /// again. Every reader goes through this: the panel, the handle, and the menu anchor.
    /// Scroll the tab strip by `delta`, eased rather than jumped.
    ///
    /// **Why animate at all:** a wheel scroll is already continuous, so a chevron that teleports
    /// the strip reads as a different mechanism from the one it is standing in for. The chevrons
    /// are triggers for the same scroll, so they should feel like it.
    ///
    /// Interpolation runs from the offset captured *now* toward an absolute target, rather than
    /// adding a small delta each tick. Stepping incrementally would drift: gpui re-clamps
    /// `offset.x` to `[-max, 0]` during its own prepaint, so at either end the increments would
    /// be silently eaten and the animation would never reach a fixed point.
    fn nudge_tabs(&mut self, delta: f32, cx: &mut Context<Self>) {
        // Accumulate onto the in-flight target, not onto the live offset, so a second click
        // mid-animation adds a tab instead of re-aiming at wherever this one had reached.
        let base = self
            .tab_scroll_target
            .unwrap_or_else(|| f32::from(self.tab_scroll.offset().x));
        self.animate_tabs_to(base + delta, cx);
    }

    /// Ease the strip's offset to `target`. Shared by the chevrons and by `reveal_active_tab`,
    /// so both feel like the same mechanism.
    fn animate_tabs_to(&mut self, target: f32, cx: &mut Context<Self>) {
        /// Long enough to read as motion, short enough not to sit between you and the tab you
        /// are aiming at.
        const TRAVEL: Duration = Duration::from_millis(140);
        /// Roughly a frame. `timer` is not a frame clock, so the eased position is computed from
        /// elapsed wall time and this only decides how often it is recomputed — a slow tick
        /// makes the motion coarse, never wrong or longer.
        const TICK: Duration = Duration::from_millis(8);

        let from = f32::from(self.tab_scroll.offset().x);
        self.tab_scroll_target = Some(target);

        let scroll = self.tab_scroll.clone();
        self.tab_scroll_anim = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();

            loop {
                let elapsed = started.elapsed().as_secs_f32() / TRAVEL.as_secs_f32();
                let t = elapsed.min(1.);
                // Ease-out cubic: leaves immediately and settles, which is what makes a short
                // travel feel deliberate rather than clipped.
                let eased = 1. - (1. - t).powi(3);
                let at = scroll.offset();
                scroll.set_offset(point(px(from + (target - from) * eased), at.y));

                // The offset is not view state, so nothing repaints on its own.
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
                if t >= 1. {
                    break;
                }
                cx.background_executor().timer(TICK).await;
            }

            // Cleared only on completion. A cancelled task leaves the target in place, which is
            // right: the click that cancelled it has already set its own.
            this.update(cx, |this, _| this.tab_scroll_target = None).ok();
        }));
    }

    pub(crate) fn clamped_panel_width(&self, window: &Window) -> f32 {
        crate::collection_panel::clamp_width(
            self.panel_width,
            f32::from(window.viewport_size().width),
        )
    }

    /// Set the panel's width from a drag, or reset it from a double-click.
    ///
    /// Stores the value **unclamped** — see the field's comment. The clamp is a function of the
    /// window, and baking one window's ceiling into the stored number is how a width silently
    /// shrinks for good.
    pub(crate) fn set_panel_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if self.panel_width == width {
            return;
        }
        self.panel_width = width;
        cx.notify();
    }

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
    fn collection_menu_anchor(&self, window: &Window) -> gpui::Point<gpui::Pixels> {
        self.collection_menu_at.unwrap_or_else(|| {
            gpui::point(px(self.clamped_panel_width(window) * 0.5), px(160.))
        })
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
        let at = self.collection_menu_anchor(window);
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
            // Named with its count for the delete prompt's reason, pointed the other way: "Run
            // billing" with no number is how forty requests reach production.
            let path = self.selected_node().map(|node| node.path.clone());
            let run = match path.as_deref().map(collection::request_count) {
                Some(1) => "Run 1 request".to_string(),
                Some(n) if n > 1 => format!("Run {n} requests"),
                // Nothing to run, and `RunFolder` says so rather than opening an empty report.
                _ => "Run folder".to_string(),
            };

            let rows = vec![
                MenuItem::new(run, RunFolder, &focus, window).into(),
                MenuRow::Separator,
                MenuItem::new("Reveal in file manager", RevealRequest, &focus, window).into(),
                MenuItem::new("Open in default app", OpenRequestExternally, &focus, window).into(),
                MenuRow::Separator,
                // Before New folder: a folder is a thing you put requests in, so the verb the
                // gesture most invites is the one that puts a request in it.
                MenuItem::new("New request", NewRequest, &focus, window).into(),
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
            // Both mean "beside this one": `begin_new_node` takes a request row's *parent*, the
            // way New folder already did, so right-clicking a request is a way to add a sibling.
            MenuItem::new("New request", NewRequest, &focus, window).into(),
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
        let at = self.collection_menu_anchor(window);
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
        self.begin_new_node(NewNode::Folder, window, cx);
    }

    /// Create a request in the folder the panel has selected.
    ///
    /// **The gesture is New folder's, exactly**, down to the inline box and where it sits. The
    /// difference someone feels is that the request is *born* with a path, so `Ctrl+S` overwrites
    /// it instead of deriving a name at the root — which is why this needed no change to saving.
    /// `NewTab` remains the scratch-buffer verb for when you do not yet know where it belongs.
    fn new_request(&mut self, _: &NewRequest, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_new_node(NewNode::Request, window, cx);
    }

    fn begin_new_node(&mut self, kind: NewNode, window: &mut Window, cx: &mut Context<Self>) {
        self.close_row_menu(window, cx);

        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            self.set_status("No collection directory — nowhere to put it", cx);
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

        let placeholder = match kind {
            NewNode::Request => "request name",
            NewNode::Folder => "folder name",
        };
        let input = cx.new(|cx| {
            crate::input::TextInput::new("", placeholder, "CollectionRename", cx)
        });
        let handle = input.read(cx).focus_handle(cx);
        let blur = window.on_focus_out(&handle, cx, |_, window, cx| {
            window.dispatch_action(Box::new(CancelRename), cx);
        });

        self.new_node = Some(NewNodeState {
            kind,
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
    /// The inline box: where it sits, how deep, what it will become, and the input itself.
    ///
    /// **The kind is in here because the row has to *look* like what it is about to be.** It was
    /// left out at first, so creating a request drew a folder glyph — the placement was
    /// generalised and the glyph was not, which is a bug no amount of looking at the placement
    /// would have found.
    pub(crate) fn new_node_row(
        &self,
    ) -> Option<(usize, u16, NewNode, Entity<crate::input::TextInput>)> {
        let state = self.new_node.as_ref()?;
        Some((state.insert_at, state.depth, state.kind, state.input.clone()))
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

    /// Open the environment editor.
    fn edit_environments(
        &mut self,
        _: &EditEnvironments,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            self.set_status("No collection directory to hold environments", cx);
            return;
        };

        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        let active = self.environment.clone();
        let panel = cx.new(|cx| {
            crate::environment_panel::EnvironmentPanel::new(root, active, restore, window, cx)
        });

        let subscription = cx.subscribe_in(&panel, window, |workspace, _, event, _, cx| {
            use crate::environment_panel::EnvEvent;
            match event {
                // The selected environment is stored by name, in the session, so a rename or a
                // removal that isn't followed here leaves the app pointed at a file that no
                // longer exists — and `load` answers that with an empty map rather than an
                // error, so every `{{var}}` would quietly stop resolving.
                EnvEvent::Renamed { from, to } => {
                    if workspace.environment.as_deref() == Some(from.as_str()) {
                        workspace.environment = Some(to.clone());
                        crate::session::save(&workspace.session(cx), cx);
                    }
                }
                EnvEvent::Removed(name) => {
                    if workspace.environment.as_deref() == Some(name.as_str()) {
                        workspace.environment = None;
                        crate::session::save(&workspace.session(cx), cx);
                    }
                }
                // The editor has *just* written one, so the "is there anything to protect"
                // half of `protect_secrets` is already answered.
                EnvEvent::SecretWritten => workspace.ensure_gitignored(cx),
            }
        });

        self.environment_panel = Some(EnvPanelState {
            panel,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn env_panel(&self) -> Option<Entity<crate::environment_panel::EnvironmentPanel>> {
        self.environment_panel.as_ref().map(|state| state.panel.clone())
    }

    fn env_next(&mut self, _: &EnvNext, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.env_panel() else { return };
        panel.update(cx, |panel, cx| panel.select(1, cx));
    }

    fn env_prev(&mut self, _: &EnvPrev, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.env_panel() else { return };
        panel.update(cx, |panel, cx| panel.select(-1, cx));
    }

    fn env_new_variable(
        &mut self,
        _: &EnvNewVariable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.env_panel() else { return };
        panel.update(cx, |panel, cx| panel.add_variable(window, cx));
    }

    /// Both row verbs act on the *focused* row, the way `RemoveRow` does in the request pane —
    /// gpui 0.2.2 has no parameterised actions, and the per-row buttons call the same panel
    /// methods directly with their own index.
    fn env_remove_variable(
        &mut self,
        _: &EnvRemoveVariable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.env_panel() else { return };
        let Some(ix) = panel.read(cx).focused_row(window, cx) else { return };
        panel.update(cx, |panel, cx| panel.remove_variable(ix, cx));
    }

    fn env_toggle_secret(
        &mut self,
        _: &EnvToggleSecret,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.env_panel() else { return };
        let Some(ix) = panel.read(cx).focused_row(window, cx) else { return };
        panel.update(cx, |panel, cx| panel.toggle_secret(ix, cx));
    }

    fn env_new_environment(
        &mut self,
        _: &EnvNewEnvironment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.env_panel() else { return };
        panel.update(cx, |panel, cx| panel.start_new(window, cx));
    }

    fn env_rename_environment(
        &mut self,
        _: &EnvRenameEnvironment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.env_panel() else { return };
        panel.update(cx, |panel, cx| panel.start_rename(window, cx));
    }

    fn env_trash_environment(
        &mut self,
        _: &EnvTrashEnvironment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.env_panel() else { return };
        panel.update(cx, |panel, cx| panel.trash_selected(window, cx));
    }

    fn env_confirm(&mut self, _: &EnvConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.env_panel() else { return };
        panel.update(cx, |panel, cx| panel.confirm(window, cx));
    }

    /// `escape` backs out of the innermost thing: a name box if one is open, otherwise the
    /// panel. One action rather than two, because "cancel the rename" and "close the editor"
    /// are the same gesture at different depths and a second binding would have to guess.
    fn env_dismiss(&mut self, _: &EnvDismiss, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.env_panel() else { return };
        if panel.update(cx, |panel, cx| panel.cancel(window, cx)) {
            return;
        }
        self.close_environment_panel(window, cx);
    }

    fn close_environment_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.environment_panel.take() else { return };
        state.panel.update(cx, |panel, cx| panel.commit(cx));
        self.globals_active = globals_has_values(cx);
        if let Some(focus) = state.panel.read(cx).restore_focus() {
            window.focus(&focus);
        }
        cx.notify();
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

    /// Fill the import field from the file dialog.
    ///
    /// The **field stays editable** and the browsed path lands in it rather than starting the
    /// import: the same field also takes a URL, so a dialog that imported on selection would
    /// make browsing a different verb from typing. It is the `WorkspaceBrowse` shape exactly —
    /// pick, see the path, then confirm.
    #[cfg(test)]
    pub fn import_panel_for_test(&self) -> Option<Entity<crate::import_panel::ImportPanel>> {
        self.import.as_ref().map(|state| state.panel.clone())
    }

    fn import_browse(&mut self, _: &ImportBrowse, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.import.as_ref() else { return };
        let panel = state.panel.clone();
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a document to import".into()),
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
                    panel.set_source(path.display().to_string(), window, cx)
                })
                .ok();
        }));
    }

    fn import_document(&mut self, _: &ImportDocument, window: &mut Window, cx: &mut Context<Self>) {
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

        // A spec served from the same self-signed box as the API cannot be fetched without the
        // same TLS setting, which is arguably where this layer earns most.
        let spec = RequestSpec {
            id: RequestId(0),
            url: source.clone(),
            method: zuno_core::Method::Get,
            settings: crate::app_state::defaults(cx),
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

    /// Parse a fetched or read document and write what it yields into the collection.
    ///
    /// The document decides which parser reads it — `import::parse` sniffs the shape — so this
    /// is written once for OpenAPI and Postman alike, and a third format adds nothing here.
    fn finish_import(
        &mut self,
        panel: &Entity<crate::import_panel::ImportPanel>,
        root: &Path,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) {
        let import = match zuno_core::import::parse(bytes) {
            Ok(zuno_core::import::Parsed::Collection(import)) => import,
            Ok(zuno_core::import::Parsed::Environment(import)) => {
                self.finish_environment_import(panel, root, import, cx);
                return;
            }
            Err(error) => {
                panel.update(cx, |panel, cx| panel.report(error.to_string(), cx));
                return;
            }
        };
        if import.requests.is_empty() {
            panel.update(cx, |panel, cx| {
                panel.report("The document has no requests to import", cx)
            });
            return;
        }

        // Everything lands under one folder named for the spec, so an import is a thing you can
        // find and a thing you can delete. Without it a hundred requests scatter through a
        // collection someone had already organised.
        // Stamped into the written files: fixing TLS on forty imported requests one at a time
        // is the papercut this layer exists to remove.
        let defaults = crate::app_state::defaults(cx);
        let title = import.title.clone().unwrap_or_else(|| "imported".to_string());
        let base = root.join(collection::slug(&title));

        let mut written = 0usize;
        let mut failures = 0usize;
        for request in &import.requests {
            // The document's own grouping — an OpenAPI tag, a Postman folder tree — becomes
            // directories inside the import's, so an API arrives filed the way its author filed
            // it. The parser has already capped the nesting at what `collection::scan` walks.
            let directory = request
                .folders
                .iter()
                .fold(base.clone(), |directory, folder| {
                    directory.join(collection::slug(folder))
                });
            // `allocate` creates the directory and picks a free name, so re-importing the same
            // spec adds `-2` files rather than overwriting a request someone has since edited.
            let spec = RequestSpec {
                settings: defaults.clone(),
                ..request.spec.clone()
            };
            match collection::allocate(&directory, &spec.name)
                .and_then(|path| collection::write(&path, &spec).map(|()| path))
            {
                Ok(_) => written += 1,
                Err(_) => failures += 1,
            }
        }

        let variables = self.import_variables(root, &title, &import, cx);

        self.refresh_tree(cx);
        self.import = None;

        let mut message = format!("Imported {written} requests into {}", collection::slug(&title));
        if failures > 0 {
            message.push_str(&format!(" — {failures} could not be written"));
        }
        if let Some(note) = variables {
            message.push_str(&note);
        }
        // Said before the skipped count, because otherwise "12 skipped" reads as "the scripts
        // were lost" when most of what mattered in them is now on the requests.
        if import.recovered > 0 {
            message.push_str(&format!(
                " — recovered {} rules from test scripts",
                import.recovered
            ));
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

    /// Write an import's variables into an environment and select it.
    ///
    /// **Selected, not merely written.** A Postman collection whose every URL begins
    /// `{{baseUrl}}` imports as a folder of requests that cannot be sent until an environment is
    /// active, and asking someone to find the switcher first is the friction this whole feature
    /// exists to remove. It overrides a selection that was already there, which is why the
    /// status line names it — a silent switch is the failure mode, not the switch.
    fn import_variables(
        &mut self,
        root: &Path,
        title: &str,
        import: &zuno_core::Import,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if import.variables.is_empty() {
            return None;
        }

        let merged = match environment::merge_imported(
            root,
            environment::Target::Named(title),
            &import.variables,
        ) {
            Ok(merged) => merged,
            // The requests are already on disk, so this is a note on a successful import rather
            // than a failure of one.
            Err(error) => return Some(format!(" — variables: {error}")),
        };

        self.environment = Some(merged.name.clone());
        // Persisted here for `Target::Environment`'s reason: choosing an environment and then
        // closing the window should not forget which one.
        crate::session::save(&self.session(cx), cx);
        // An import can bring secrets across, and they must not be committable before the next
        // send happens to notice.
        self.protect_secrets(cx);

        let mut note = format!(
            " — {} variables in the {} environment, now selected",
            merged.added, merged.name
        );
        if merged.kept > 0 {
            note.push_str(&format!(" ({} already set were left alone)", merged.kept));
        }
        Some(note)
    }

    /// Write a Postman environment export into `environments/`.
    ///
    /// The other half of `finish_import`, and a genuinely different outcome rather than a
    /// collection with no requests: nothing is written into the tree, so there is no folder to
    /// name and nothing to refresh there.
    fn finish_environment_import(
        &mut self,
        panel: &Entity<crate::import_panel::ImportPanel>,
        root: &Path,
        import: zuno_core::import::EnvironmentImport,
        cx: &mut Context<Self>,
    ) {
        // `None` is a globals export. Postman's globals are the layer every environment resolves
        // over, which is exactly what Zuno's are, so the mapping is the whole feature here.
        let target = match import.name.as_deref() {
            Some(name) => environment::Target::Named(name),
            None => environment::Target::Globals,
        };

        let merged = match environment::merge_imported(root, target, &import.variables) {
            Ok(merged) => merged,
            Err(error) => {
                panel.update(cx, |panel, cx| panel.report(error.to_string(), cx));
                return;
            }
        };

        // Checked from what was imported rather than through `protect_secrets`, which reads the
        // environment *currently selected* — nothing is selected yet here, and a globals import
        // never selects anything at all, so that route writes no rule and leaves a
        // `.local.json` full of tokens sitting there committable. Break-tested.
        if import.variables.iter().any(|variable| variable.secret) {
            self.ensure_gitignored(cx);
        }

        let mut message = format!("Imported {} variables into {}", merged.added, merged.name);
        if merged.kept > 0 {
            message.push_str(&format!(" ({} already set were left alone)", merged.kept));
        }

        if import.name.is_some() {
            // Selected for the collection import's reason: someone who imported `staging` meant
            // to use it, and the switch is named because a silent one is the failure mode.
            self.environment = Some(merged.name.clone());
            crate::session::save(&self.session(cx), cx);
            message.push_str(" — now selected");
        } else {
            // Globals need no selecting; they are always in effect, which is worth saying so
            // nobody goes looking for the new environment in the switcher.
            message.push_str(" — globals are always active");
        }

        // A file read, so it belongs at a state change: an import is one.
        self.globals_active = globals_has_values(cx);
        self.import = None;
        self.set_status(&message, cx);
        cx.notify();
    }

    fn format_body(&mut self, _: &FormatBody, window: &mut Window, cx: &mut Context<Self>) {
        self.rewrite_body(true, window, cx);
    }

    fn minify_body(&mut self, _: &MinifyBody, window: &mut Window, cx: &mut Context<Self>) {
        self.rewrite_body(false, window, cx);
    }

    /// Re-emit the request body as formatted or minified JSON.
    ///
    /// **Gated on the body *kind*, not on whether the text happens to parse.** The chip on screen
    /// says JSON or XML, and a verb that quietly works on a body labelled XML — or refuses one
    /// labelled Text that holds JSON — is a verb whose behaviour you cannot read off the screen.
    /// "This body is XML" points at what to change; "not valid JSON" about an XML body does not.
    ///
    /// The rewrite goes through `Editor::replace_range`, so it lands on the ordinary edit path
    /// and **`Ctrl+Z` undoes it** — which is what makes reformatting someone's body a safe verb
    /// rather than one that needs a confirmation.
    fn rewrite_body(&mut self, indent: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        if view.read(cx).body_type != crate::request_view::BodyType::Raw {
            self.set_status("Formatting needs a raw body", cx);
            return;
        }
        if view.read(cx).body_kind() != zuno_core::RawKind::Json {
            self.set_status(
                &format!(
                    "Formatting is JSON only — this body is {}",
                    view.read(cx).body_label()
                ),
                cx,
            );
            return;
        }

        let before = view.read(cx).body_editor.read(cx).text().to_string();
        if before.trim().is_empty() {
            self.set_status("The body is empty", cx);
            return;
        }

        // Invariant 3: the parse goes to the background executor. A pasted body is usually small,
        // but "usually" is not a size the invariant admits, and a 10MB paste is exactly the case
        // where a frozen window would be noticed.
        let source = bytes::Bytes::from(before.clone());
        let parse = cx.background_executor().spawn(async move {
            zuno_core::JsonOutline::parse(source).map(|outline| {
                if indent {
                    zuno_core::json::format::pretty(&outline)
                } else {
                    zuno_core::json::format::minify(&outline)
                }
            })
        });

        // `window.spawn` rather than `cx.spawn`, because `Editor::replace_range` needs a real
        // `&mut Window` — it goes through the ordinary edit path, which is what buys the undo.
        let workspace = cx.entity();
        self.body_format = Some(window.spawn(cx, async move |cx| {
            let formatted = parse.await;

            let _ = cx.update(|window, cx| workspace.update(cx, |workspace, cx| match formatted {
                Ok(text) => {
                    // Typing continued while this was in flight, so the result describes a buffer
                    // that no longer exists. Replacing it would discard those keystrokes.
                    if view.read(cx).body_editor.read(cx).text() != before {
                        return;
                    }
                    if text == before {
                        workspace.set_status("The body is already formatted", cx);
                        return;
                    }

                    let end = before.len();
                    let size = format_bytes(text.len() as u64);
                    view.update(cx, |view, cx| {
                        view.body_editor.update(cx, |editor, cx| {
                            editor.replace_range(0..end, &text, window, cx);
                        });
                    });
                    let verb = if indent { "Formatted" } else { "Minified" };
                    workspace.set_status(&format!("{verb} the body — {size}"), cx);
                }
                // `flatten` names the byte offset, which is the only thing that makes a syntax
                // error actionable in a body you did not write.
                Err(error) => {
                    let (line, col) = zuno_core::json::line_col(before.as_bytes(), error.offset);
                    workspace.set_status(
                        &format!("Not valid JSON — {} at line {line}, column {col}", error.message),
                        cx,
                    );
                }
            }));
        }));
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
        let closed = self.renaming.take().is_some() | self.new_node.take().is_some();
        if closed {
            window.focus(&self.panel_focus);
            cx.notify();
        }
    }

    /// Write an empty request into `parent` and open it.
    ///
    /// **The defaults are stamped in**, the way an import stamps them: a request created here is
    /// a real file from the first moment, so it has to start where `Ctrl+T` starts rather than
    /// from `RequestSpec::default()`'s bare state.
    ///
    /// `allocate` picks the filename, so naming two requests `Login` gives `Login-2.json` rather
    /// than overwriting the first — a derived name is not an identity.
    ///
    /// Opening it through `open_collection_file` rather than `open` is what makes the whole slice
    /// work: that path is where a buffer *remembers its file*, and remembering is what makes the
    /// next `Ctrl+S` overwrite this request instead of deriving a fresh name at the root.
    fn create_request(
        &mut self,
        parent: &Path,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = RequestSpec {
            id: RequestId(0),
            name: label.to_string(),
            settings: crate::app_state::defaults(cx),
            ..RequestSpec::default()
        };

        let written = collection::allocate(parent, label)
            .and_then(|path| collection::write(&path, &spec).map(|()| path));

        match written {
            Ok(path) => {
                let shown = crate::collections::root(cx)
                    .and_then(|root| path.strip_prefix(root).ok())
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                // The tree first: it is how you see the request landed, and
                // `open_collection_file` reads a path that has to already be scanned.
                self.refresh_tree(cx);
                self.open_collection_file(path, window, cx);
                self.set_status(&format!("Created {shown}"), cx);
            }
            Err(error) => self.set_status(&format!("Could not create: {error}"), cx),
        }
    }

    fn commit_rename(&mut self, _: &CommitRename, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.new_node.take() {
            let typed = state.input.read(cx).text().to_string();
            window.focus(&self.panel_focus);

            if typed.trim().is_empty() {
                let what = match state.kind {
                    NewNode::Request => "A request needs a name",
                    NewNode::Folder => "A folder needs a name",
                };
                self.set_status(what, cx);
                cx.notify();
                return;
            }

            match state.kind {
                NewNode::Folder => match collection::create_folder(&state.parent, &typed) {
                    Ok(made) => {
                        let name = made
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default();
                        self.refresh_tree(cx);
                        self.set_status(&format!("Created {name}"), cx);
                    }
                    Err(error) => self.set_status(&format!("Could not create: {error}"), cx),
                },
                NewNode::Request => self.create_request(&state.parent, &typed, window, cx),
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

        self.ensure_gitignored(cx);
    }

    fn ensure_gitignored(&mut self, cx: &mut Context<Self>) {
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };

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
        // "None" means no *selected* environment, never "no substitution": globals is the bottom
        // layer either way. Both of the strings this row used to carry said otherwise.
        let mut items = vec![picker::Item {
            label: SharedString::from("None"),
            detail: SharedString::from(match (current.is_none(), self.globals_active) {
                (true, true) => "current — globals still substitutes",
                (true, false) => "current — variables are left unresolved",
                (false, true) => "fall back to globals alone",
                (false, false) => "send requests without substitution",
            }),
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

        // Last, under the environments themselves: the switcher is the surface you are already
        // on when you want to change one, so it is where the editor is discoverable — the badge
        // stays a one-click switch rather than becoming a menu.
        items.push(picker::Item {
            label: SharedString::from("Edit environments…"),
            detail: SharedString::from("add, rename or remove variables"),
            target: picker::Target::Action(Box::new(EditEnvironments)),
        });

        self.show_picker(
            items,
            "No environments yet — choose \"Edit environments\" to make one",
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
            picker::Target::Proxy(mode) => {
                let label = mode.label().to_string();
                crate::app_state::set_proxy(cx, mode);
                // Named rather than silent, for the reason the environment switch is: a change
                // to where every request goes is the one thing that must not happen quietly.
                self.set_status(&format!("Proxy: {label}"), cx);
            }
            picker::Target::RemoveProxy(url) => {
                crate::app_state::remove_proxy(cx, &url);
                self.set_status(&format!("Removed proxy {url}"), cx);
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
            picker::Target::Flow(name) => self.start_flow(&name, window, cx),
            picker::Target::AddToFlow(name) => self.add_selection_to_flow(&name, cx),
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
        self.show_settings(Scope::Request, settings, restore, window, cx);
    }

    /// The titlebar's gear: what a *new* request starts from.
    ///
    /// A separate trigger rather than a scope row inside `Ctrl+,`, because where a gear lives is
    /// what says what it changes — one in the request pane that could also rewrite app state
    /// needed both the header and a row to say so, which is a design arguing with itself.
    fn open_defaults(&mut self, _: &OpenDefaults, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let settings = crate::app_state::defaults(cx);
        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        self.show_settings(Scope::Defaults, settings, restore, window, cx);
    }

    fn show_settings(
        &mut self,
        scope: Scope,
        settings: zuno_core::RequestSettings,
        restore: Option<gpui::FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.new(|cx| SettingsPanel::new(scope, settings, restore, cx));

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
    /// Write back whichever scope was edited.
    ///
    /// Dispatched rather than writing both, because the defaults live in `app.json` and every
    /// left/right press comes through here — committing both would put a file write behind each
    /// keystroke to change a per-request timeout.
    fn commit_settings(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &self.settings else { return };
        let panel = state.panel.clone();

        let settings = panel.read(cx).settings().clone();
        match panel.read(cx).scope() {
            Scope::Request => {
                let Some(view) = self.active() else { return };
                view.update(cx, |view, cx| {
                    view.settings = settings;
                    cx.notify();
                });
            }
            Scope::Defaults => crate::app_state::set_defaults(cx, settings),
        }
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
    pub fn setting_selection(&self, cx: &App) -> usize {
        self.settings
            .as_ref()
            .map(|state| state.panel.read(cx).selection())
            .unwrap_or(0)
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
            self.panel_width,
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

    /// Choose where requests are routed.
    ///
    /// The picker rather than the settings panel because that panel holds no `TextInput` at all
    /// and a proxy needs a URL. Its placeholder is what says you can type one.
    fn set_proxy(&mut self, _: &SetProxy, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let current = crate::app_state::proxy(cx);

        let detail = |is_current: bool, note: &str| {
            SharedString::from(if is_current {
                format!("current · {note}")
            } else {
                note.to_string()
            })
        };

        let mut items = vec![
            picker::Item {
                label: SharedString::from("System"),
                detail: detail(
                    current == ProxyMode::System,
                    "reads HTTP_PROXY and NO_PROXY",
                ),
                target: picker::Target::Proxy(ProxyMode::System),
            },
            picker::Item {
                label: SharedString::from("Off"),
                detail: detail(current == ProxyMode::Off, "ignores the environment"),
                target: picker::Target::Proxy(ProxyMode::Off),
            },
        ];

        // Every saved proxy, not just the one in use — switching to System or Off used to
        // discard the URL entirely, so the only way back was retyping it.
        items.extend(crate::app_state::proxies(cx).into_iter().map(|url| {
            let is_current = current == ProxyMode::Url(url.clone());
            picker::Item {
                label: SharedString::from(url.clone()),
                detail: detail(is_current, "saved"),
                target: picker::Target::Proxy(ProxyMode::Url(url)),
            }
        }));

        let picker = self.show_picker(items, "Type a proxy URL, or pick one", window, cx);
        picker.update(cx, |picker, cx| picker.set_fallback(typed_proxy_row, cx));
    }

    /// Pick a certificate file.
    ///
    /// A native dialog rather than the picker's typed-text row the proxy uses: a path is
    /// something you browse to, and `prompt_for_paths` already backs binary bodies and
    /// multipart parts. The cost is that the *selection* cannot be driven headlessly —
    /// `prompt_for_paths` is `unimplemented!()` in the test platform — so what is asserted is
    /// the layer below, that a chosen path reaches the client and a bad one is reported.
    fn choose_cert(&mut self, identity: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(if identity {
                "Choose a client certificate".into()
            } else {
                "Choose a root CA certificate".into()
            }),
        });

        self.cert_prompt = Some(cx.spawn_in(window, async move |workspace, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            workspace
                .update_in(cx, |workspace, _, cx| {
                    let mut files = crate::app_state::tls(cx);
                    if identity {
                        // Chosen means active, and remembered so switching back needs no
                        // second trip through the dialog.
                        if !files.identities.contains(&path) {
                            files.identities.push(path.clone());
                        }
                        files.identity = Some(path.clone());
                    } else if !files.root_cas.contains(&path) {
                        files.root_cas.push(path.clone());
                    }
                    crate::app_state::set_tls(cx, files);
                    let name = cert_name(&path);
                    workspace.set_status(&format!("Using {name}"), cx);
                })
                .ok();
        }));
    }

    fn choose_client_cert(&mut self, _: &ChooseClientCert, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_cert(true, window, cx);
    }

    fn choose_root_ca(&mut self, _: &ChooseRootCa, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_cert(false, window, cx);
    }

    fn open_certificates(&mut self, _: &OpenCertificates, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let restore = Some(window.focused(cx).unwrap_or_else(|| self.focus_handle.clone()));
        let panel = crate::cert_panel::CertPanel::new(restore, cx);
        let focus = panel.focus_handle.clone();
        self.certs = Some(panel);
        window.focus(&focus);
        cx.notify();
    }

    fn certs_dismiss(&mut self, _: &CertsDismiss, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.certs.take() else { return };
        if let Some(focus) = panel.restore_focus {
            window.focus(&focus);
        }
        cx.notify();
    }

    fn certs_next(&mut self, _: &CertsNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_certs(1, cx);
    }

    fn certs_prev(&mut self, _: &CertsPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step_certs(-1, cx);
    }

    fn step_certs(&mut self, delta: isize, cx: &mut Context<Self>) {
        let files = crate::app_state::tls(cx);
        if let Some(panel) = self.certs.as_mut() {
            panel.step(delta, &files);
            cx.notify();
        }
    }

    pub(crate) fn select_cert_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(panel) = self.certs.as_mut() {
            panel.selected = ix;
            cx.notify();
        }
    }

    fn certs_confirm(&mut self, _: &CertsConfirm, window: &mut Window, cx: &mut Context<Self>) {
        use crate::cert_panel::Row;
        let files = crate::app_state::tls(cx);
        let Some(row) = self.certs.as_ref().and_then(|panel| panel.row(&files)) else {
            return;
        };

        match row {
            // Switching identity keeps the panel open: choosing is the thing you came to do and
            // you may well want to look at the issuer list next.
            Row::UseNoIdentity => {
                let mut files = files;
                files.identity = None;
                crate::app_state::set_tls(cx, files);
                cx.notify();
            }
            Row::UseIdentity(path) => {
                let mut files = files;
                files.identity = Some(path);
                crate::app_state::set_tls(cx, files);
                cx.notify();
            }
            // The dialog is modal to the OS, so the panel closes first rather than sitting
            // behind it catching keys it can no longer see.
            Row::ChooseIdentity => {
                self.certs = None;
                self.choose_cert(true, window, cx);
            }
            Row::ChooseRootCa => {
                self.certs = None;
                self.choose_cert(false, window, cx);
            }
            // Every issuer in the list is already in force, so there is nothing to confirm.
            Row::RootCa(_) => {}
        }
    }

    fn certs_remove(&mut self, _: &CertsRemove, _: &mut Window, cx: &mut Context<Self>) {
        use crate::cert_panel::Row;
        let files = crate::app_state::tls(cx);
        let Some(row) = self.certs.as_ref().and_then(|panel| panel.row(&files)) else {
            return;
        };

        let mut next = files;
        match row {
            Row::UseIdentity(path) => {
                next.identities.retain(|saved| *saved != path);
                // Removing the one being presented stops presenting it — leaving `identity`
                // naming a file no longer in the list is a state with no way back to it.
                if next.identity.as_deref() == Some(path.as_path()) {
                    next.identity = None;
                }
            }
            Row::RootCa(path) => next.root_cas.retain(|saved| *saved != path),
            // Nothing to remove on a chooser or on "None".
            Row::UseNoIdentity | Row::ChooseIdentity | Row::ChooseRootCa => return,
        }

        crate::app_state::set_tls(cx, next);
        // Clamp: the list just got shorter.
        let shorter = crate::app_state::tls(cx);
        if let Some(panel) = self.certs.as_mut() {
            let len = crate::cert_panel::CertPanel::rows(&shorter).len();
            panel.selected = panel.selected.min(len.saturating_sub(1));
        }
        cx.notify();
    }

    /// Forget a saved proxy. Mirrors `Forget workspace`: a verb of its own over a list of what
    /// can be removed, rather than a delete gesture the picker would have to learn.
    fn remove_proxy(&mut self, _: &RemoveProxy, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let items = crate::app_state::proxies(cx)
            .into_iter()
            .map(|url| picker::Item {
                label: SharedString::from(url.clone()),
                detail: SharedString::default(),
                target: picker::Target::RemoveProxy(url),
            })
            .collect();

        self.show_picker(items, "No saved proxies", window, cx);
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

    fn show_capture_tab(&mut self, _: &ShowCaptureTab, _: &mut Window, cx: &mut Context<Self>) {
        self.show_request_tab(RequestTab::Capture, cx);
    }

    /// Run the folder the panel's selection sits in.
    ///
    /// **A folder, resolved from the selection rather than asked for.** A directory row runs
    /// itself; a request row runs the folder holding it, because "run the thing next to what I
    /// am looking at" is the gesture, and offering a request row a run of *one* request is a
    /// worse version of `Ctrl+Enter`. With nothing selected it runs the whole collection, and the
    /// report's title says which — so the answer to "what did that just do" is on screen.
    fn run_folder(&mut self, _: &RunFolder, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            self.set_status("No collection directory to run", cx);
            return;
        };
        let Some(engine) = cx.engine() else { return };

        let folder = match self.selected_node() {
            Some(node) if matches!(node.kind, NodeKind::Directory) => node.path.clone(),
            Some(node) => node.path.parent().map(Path::to_path_buf).unwrap_or(root.clone()),
            None => root.clone(),
        };

        let steps = zuno_core::runner::steps_in_folder(&root, &folder);
        if steps.is_empty() {
            self.set_status("Nothing to run in there", cx);
            return;
        }

        let subject = if folder == root {
            crate::app_state::active_id(cx).unwrap_or_else(|| "collection".to_string())
        } else {
            crate::app_state::label(&folder)
        };
        self.begin_run(subject, root, engine, steps, window, cx);
    }

    /// Open the report and start the run behind it. Shared by the two producers, which differ
    /// only in where their step list came from.
    fn begin_run(
        &mut self,
        subject: String,
        root: PathBuf,
        engine: Arc<zuno_core::Engine>,
        steps: Vec<zuno_core::runner::Step>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let total = steps.len();
        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        let panel = cx.new(|cx| {
            crate::run_panel::RunPanel::new(subject, root.clone(), total, restore, window, cx)
        });

        let subscription = cx.subscribe_in(&panel, window, |workspace, _, event, window, cx| {
            let crate::run_panel::RunEvent::Open(path) = event;
            // Opening the request is what a failed row is *for*: the report says what broke and
            // this is how you go and look at it.
            let path = path.clone();
            workspace.close_run(window, cx);
            workspace.open_collection_file(path, window, cx);
        });

        let cancel = Arc::new(AtomicBool::new(false));
        let task = self.spawn_run(&panel, engine, steps, root, &cancel, window, cx);

        self.run = Some(RunState {
            panel,
            cancel,
            _task: task,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn edit_flows(&mut self, _: &EditFlows, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };

        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        let panel = cx.new(|cx| crate::flow_panel::FlowPanel::new(root, restore, window, cx));
        let subscription = cx.subscribe_in(&panel, window, |_, _, event, _, _| {
            let crate::flow_panel::FlowEvent::Changed = event;
            // Nothing in the workspace holds a flow by name — a run resolves one at the moment
            // it starts — so this exists for the next thing that does, and to keep the panel
            // from having to know that.
        });

        self.flows = Some(FlowPanelState {
            panel,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn flow_panel(&self) -> Option<Entity<crate::flow_panel::FlowPanel>> {
        self.flows.as_ref().map(|state| state.panel.clone())
    }

    fn flow_new(&mut self, _: &FlowNew, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.start_new(window, cx));
    }

    fn flow_rename(&mut self, _: &FlowRename, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.start_rename(window, cx));
    }

    fn flow_trash(&mut self, _: &FlowTrash, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.trash_selected(window, cx));
    }

    fn flow_next(&mut self, _: &FlowNext, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.select(1, cx));
    }

    fn flow_prev(&mut self, _: &FlowPrev, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.select(-1, cx));
    }

    fn flow_step_next(&mut self, _: &FlowStepNext, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.step_selection(1, cx));
    }

    fn flow_step_prev(&mut self, _: &FlowStepPrev, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.step_selection(-1, cx));
    }

    fn flow_step_up(&mut self, _: &FlowStepUp, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.move_step(-1, cx));
    }

    fn flow_step_down(&mut self, _: &FlowStepDown, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.move_step(1, cx));
    }

    fn flow_step_remove(&mut self, _: &FlowStepRemove, _: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.remove_step(cx));
    }

    fn flow_confirm(&mut self, _: &FlowConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        panel.update(cx, |panel, cx| panel.confirm(window, cx));
    }

    /// `escape` backs out of a name box if one is open, otherwise the panel — the same two-stage
    /// rule the environment editor follows.
    fn flow_dismiss(&mut self, _: &FlowDismiss, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.flow_panel() else { return };
        if panel.update(cx, |panel, cx| panel.cancel(window, cx)) {
            return;
        }
        let Some(state) = self.flows.take() else { return };
        if let Some(focus) = state.panel.read(cx).restore_focus() {
            window.focus(&focus);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn flow_panel_for_test(&self) -> Option<Entity<crate::flow_panel::FlowPanel>> {
        self.flow_panel()
    }

    /// Choose a flow to run.
    ///
    /// A picker rather than a tree entry: flows are not requests, and putting them in the panel
    /// would mean the tree stops being "the files in your collection" — the same argument that
    /// keeps environments out of it.
    fn run_flow(&mut self, _: &RunFlow, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };

        let mut items: Vec<picker::Item> = zuno_core::flow::scan(&root)
            .into_iter()
            .map(|flow| picker::Item {
                label: SharedString::from(flow.name.clone()),
                detail: SharedString::from(match flow.steps.len() {
                    1 => "1 request".to_string(),
                    n => format!("{n} requests"),
                }),
                target: picker::Target::Flow(flow.name),
            })
            .collect();

        // Last, the way "Edit environments…" sits under the environments: the switcher is the
        // surface you are already on when you want to change one.
        items.push(picker::Item {
            label: SharedString::from("Edit flows…"),
            detail: SharedString::from("create, reorder or remove"),
            target: picker::Target::Action(Box::new(EditFlows)),
        });

        self.show_picker(items, "No flows yet — choose \"Edit flows\" to make one", window, cx);
    }

    fn start_flow(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };
        let Some(engine) = cx.engine() else { return };
        let Ok(flow) = zuno_core::flow::read(&root, name) else {
            self.set_status(&format!("Could not read the flow {name:?}"), cx);
            return;
        };

        let steps = zuno_core::runner::steps_for_flow(&root, &flow);
        if steps.is_empty() {
            self.set_status(&format!("{name} has no steps yet"), cx);
            return;
        }
        self.begin_run(flow.name, root, engine, steps, window, cx);
    }

    /// Append the panel's selected request to a flow.
    ///
    /// Appended, never inserted: where a step goes is the flow editor's job, and a picker that
    /// also asked "at which position" would be two questions in one gesture.
    fn add_selection_to_flow(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };
        let Some(node) = self.selected_node() else { return };
        let Ok(relative) = node.path.strip_prefix(&root) else { return };
        let relative = relative.display().to_string();

        let mut flow = match zuno_core::flow::read(&root, name) {
            Ok(flow) => flow,
            Err(error) => {
                self.set_status(&format!("{error}"), cx);
                return;
            }
        };
        flow.steps.push(relative.clone());

        match zuno_core::flow::save(&root, &flow) {
            Ok(()) => self.set_status(&format!("Added {relative} to {name}"), cx),
            Err(error) => self.set_status(&format!("{error}"), cx),
        }
    }

    /// Offer the flows to add the selected request to.
    fn add_to_flow(&mut self, _: &AddToFlow, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() || self.selected_request().is_none() {
            return;
        }
        let Some(root) = crate::collections::root(cx).map(Path::to_path_buf) else {
            return;
        };

        let items: Vec<picker::Item> = zuno_core::flow::scan(&root)
            .into_iter()
            .map(|flow| picker::Item {
                label: SharedString::from(flow.name.clone()),
                detail: SharedString::from(format!("{} requests", flow.steps.len())),
                target: picker::Target::AddToFlow(flow.name),
            })
            .collect();

        self.show_picker(items, "No flows yet — make one with Ctrl+Alt+R", window, cx);
    }

    /// Drive the run off-thread, handing each outcome back as it lands.
    ///
    /// `runner::run_with_progress` blocks — that is the whole design, and invariant 3 is why it
    /// cannot be called here. The channel exists so the report fills in as the run goes: forty
    /// requests showing nothing until they finish is indistinguishable from a hang.
    fn spawn_run(
        &self,
        panel: &Entity<crate::run_panel::RunPanel>,
        engine: Arc<zuno_core::Engine>,
        steps: Vec<zuno_core::runner::Step>,
        root: PathBuf,
        cancel: &Arc<AtomicBool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let environment = self.environment.clone();
        let (sender, receiver) = async_channel::unbounded();
        let flag = Arc::clone(cancel);

        let work = cx.background_executor().spawn(async move {
            zuno_core::runner::run_with_progress(
                &engine,
                steps,
                &root,
                environment.as_deref(),
                &flag,
                |outcome| {
                    let _ = sender.send_blocking(zuno_core::runner::Outcome {
                        label: outcome.label.clone(),
                        status: outcome.status,
                        duration: outcome.duration,
                        error: None,
                        unresolved: outcome.unresolved.clone(),
                        failures: outcome.failures.clone(),
                        captured: outcome.captured.clone(),
                    });
                },
            )
            .cancelled
        });

        let panel = panel.clone();
        cx.spawn_in(window, async move |_, cx| {
            while let Ok(outcome) = receiver.recv().await {
                let _ = panel.update(cx, |panel, cx| panel.push(outcome, cx));
            }
            let cancelled = work.await;
            let _ = panel.update(cx, |panel, cx| panel.finish(cancelled, cx));
        })
    }

    /// `escape` stops a run in flight, and closes a finished report. One action, because it means
    /// "back out of this" at whichever stage you are at.
    fn run_dismiss(&mut self, _: &RunDismiss, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.run.as_ref() else { return };
        if state.panel.read(cx).running() {
            state.cancel.store(true, Ordering::Relaxed);
            return;
        }
        self.close_run(window, cx);
    }

    fn close_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.run.take() else { return };
        state.cancel.store(true, Ordering::Relaxed);
        if let Some(focus) = state.panel.read(cx).restore_focus() {
            window.focus(&focus);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn run_panel_for_test(&self) -> Option<Entity<crate::run_panel::RunPanel>> {
        self.run.as_ref().map(|state| state.panel.clone())
    }

    fn show_assert_tab(&mut self, _: &ShowAssertTab, _: &mut Window, cx: &mut Context<Self>) {
        self.show_request_tab(RequestTab::Assert, cx);
    }

    fn add_assertion(&mut self, _: &AddAssertion, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| {
                view.add_row(RowKind::Assert, window, cx);
            });
        }
    }

    fn cycle_assert_op(&mut self, _: &CycleAssertOp, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        match view.read(cx).focused_row(window, cx) {
            Some((RowKind::Assert, ix)) => {
                view.update(cx, |view, cx| view.cycle_assert_op(ix, cx))
            }
            _ => self.set_status("Focus an assertion row first", cx),
        }
    }

    /// Turn the selected response row into an assertion.
    ///
    /// The same gesture as `CaptureValue` and for the same reason: the path comes from `path_to`
    /// rather than being typed twice, so a rule cannot quietly check a path that never matches.
    fn assert_value(&mut self, _: &AssertValue, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        if view.read(cx).selected_body_row().is_none() {
            self.set_status(&self.select_a_row_hint(window), cx);
            return;
        }
        let Some(path) = view.read(cx).selected_body_path() else {
            self.set_status("That row has no path to assert on", cx);
            return;
        };

        view.update(cx, |view, cx| view.push_assertion(path, window, cx));
    }

    fn add_capture(&mut self, _: &AddCapture, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| {
                view.add_row(RowKind::Capture, window, cx);
            });
        }
    }

    fn toggle_capture_secret(
        &mut self,
        _: &ToggleCaptureSecret,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.active() else { return };
        let focused = view.read(cx).focused_row(window, cx);
        match focused {
            Some((RowKind::Capture, ix)) => {
                view.update(cx, |view, cx| view.toggle_capture_secret(ix, cx))
            }
            _ => self.set_status("Focus a capture row first", cx),
        }
    }

    /// Turn the selected response row into a capture rule.
    ///
    /// The authoring path that matters: the path comes from `path_to`, the same function behind
    /// `Alt+C`, so it is right by construction rather than typed. A name typed by hand against a
    /// path typed by hand is two chances to be wrong about a chain that then fails silently one
    /// request later.
    fn capture_value(&mut self, _: &CaptureValue, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };

        if view.read(cx).selected_body_row().is_none() {
            self.set_status(&self.select_a_row_hint(window), cx);
            return;
        }
        let Some(path) = view.read(cx).selected_body_path() else {
            self.set_status("That row has no path to capture", cx);
            return;
        };

        let name = capture_name(&path);
        // Publishes immediately, against the response on screen. Deferring to the next send is
        // what shipped first, and it made the one path whose whole argument is "author it where
        // the data is" the one path that looks at the data and declines to read it — silently,
        // under a menu row that says "Capture as variable" rather than "capture on next send".
        let environment = self.environment.clone();
        view.update(cx, |view, cx| {
            view.push_capture(path, name, window, cx);
            view.publish_captures(environment, cx);
        });
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
    fn next_response_tab(&mut self, _: &NextResponseTab, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.cycle_response_view(1, cx));
        }
    }

    fn prev_response_tab(&mut self, _: &PrevResponseTab, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.cycle_response_view(-1, cx));
        }
    }

    fn show_response_view(&mut self, view: ResponseView, cx: &mut Context<Self>) {
        if let Some(active) = self.active() {
            active.update(cx, |active, cx| active.show_response_view(view, cx));
        }
    }

    fn show_response_body(&mut self, _: &ShowResponseBody, _: &mut Window, cx: &mut Context<Self>) {
        self.show_response_view(ResponseView::Body, cx);
    }

    fn show_response_headers(
        &mut self,
        _: &ShowResponseHeaders,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_response_view(ResponseView::Headers, cx);
    }

    fn show_response_timing(
        &mut self,
        _: &ShowResponseTiming,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_response_view(ResponseView::Timing, cx);
    }

    fn show_response_diff(&mut self, _: &ShowResponseDiff, _: &mut Window, cx: &mut Context<Self>) {
        self.show_response_view(ResponseView::Diff, cx);
    }

    fn toggle_html_view(&mut self, _: &ToggleHtmlView, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.toggle_html_view(cx));
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
    /// Open the select behind a multipart row's type chip.
    ///
    /// **A select, not a `context_menu`.** That primitive is a right-click menu: a full-window
    /// scrim, and rows laid out for a label plus a right-aligned keybinding column. Borrowed
    /// here it rendered a panel inches wide to hold the words "text" and "file", because the
    /// keybinding column is still reserving its space. `ui::select_list` is the other shape —
    /// pinned under the control, sized to its content, occluding rather than scrimming.
    ///
    /// Clicking the chip while it is already open closes it, so the chip is a toggle and never
    /// a way to stack two of them.
    fn open_part_kind_menu(
        &mut self,
        _: &OpenPartKindMenu,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.active() else { return };
        let Some((ix, at)) = view.update(cx, |view, _| view.take_part_kind_menu()) else {
            return;
        };
        if self.modal_open() {
            return;
        }
        self.part_select = match self.part_select {
            Some((open, ..)) if open == ix => None,
            _ => Some((ix, at)),
        };
        cx.notify();
    }

    /// The open type select, if any. Its own method so a test can read real state — a removed
    /// element keeps its `debug_bounds` entry until another frame is drawn.
    #[cfg(test)]
    pub fn part_select_row(&self) -> Option<usize> {
        self.part_select.map(|(ix, _)| ix)
    }

    fn choose_part_kind(&mut self, ix: usize, is_file: bool, cx: &mut Context<Self>) {
        self.part_select = None;
        if let Some(view) = self.active() {
            view.update(cx, |view, cx| view.set_multipart_kind(ix, is_file, cx));
        }
        cx.notify();
    }

    fn part_kind_select(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (row, at) = self.part_select?;
        let theme = cx.theme().clone();
        // The highlight *is* the tick: exactly one row is marked, and it is the one in force.
        let current = usize::from(self.active()?.read(cx).multipart_is_file(row));

        Some(crate::ui::select_list(
            "part-kind-select",
            at,
            vec![SharedString::from("text"), SharedString::from("file")],
            Some(current),
            px(84.),
            &theme,
            cx,
            |_, _, _| {},
            move |workspace, ix, _window, cx| workspace.choose_part_kind(row, ix == 1, cx),
        ))
    }

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
            // Offered beside "Copy path" because it *is* that path, put somewhere useful instead
            // of on the clipboard. Absent for a raw body, where there is no path to capture.
            items.push(context_menu::MenuItem::new(
                "Capture as variable",
                CaptureValue,
                &focus,
                window,
            ));
            items.push(context_menu::MenuItem::new(
                "Assert on this",
                AssertValue,
                &focus,
                window,
            ));
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
        // **Grouped by what each row acts on**, widening outward: a buffer, then the collection,
        // then the workspace, then what is on screen, then the app. It was three unrelated verbs
        // and a help section, which said nothing about the app's shape — a menu is the one place
        // that structure is legible, since the palette is a flat searchable list on purpose.
        vec![
            MenuItem::new("New tab", NewTab, &focus, window).into(),
            MenuItem::new("New request", NewRequest, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::new("Import a collection or spec", ImportDocument, &focus, window).into(),
            MenuItem::new("Import from curl", ImportCurl, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::new("New workspace", NewWorkspace, &focus, window).into(),
            MenuItem::new("Open workspace", OpenWorkspace, &focus, window).into(),
            MenuRow::Separator,
            MenuItem::new("Find request", OpenRequest, &focus, window).into(),
            MenuItem::new("Command palette", OpenPalette, &focus, window).into(),
            MenuRow::Separator,
            // The two toggles. Both already have a titlebar icon, and a menu row is the
            // discoverable path to a keystroke an icon can only hint at.
            MenuItem::new("Collection panel", ToggleCollectionPanel, &focus, window).into(),
            MenuItem::new("Toggle theme", ToggleTheme, &focus, window).into(),
            MenuRow::Separator,
            // Request-scoped above app-scoped, and named apart: one edits the buffer in front of
            // you, the other edits what a new one starts from.
            MenuItem::new("Request settings", OpenSettings, &focus, window).into(),
            MenuItem::new("Default request settings", OpenDefaults, &focus, window).into(),
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

    /// Ask GitHub whether there is a newer release, once per launch.
    ///
    /// **Once per launch rather than on a stored timer.** A timestamp in `app.json` was the
    /// first plan and buys very little: the endpoint is a redirect with no rate limit, so the
    /// cost of asking is one small request per start. What it *would* buy is a wall clock in
    /// the code path, and the test dispatcher runs a simulated one — so the throttle would be
    /// the half no test could drive. The cost is stated: a window left open for a week does
    /// not re-check.
    ///
    /// Silent in every failure. Offline, firewalled, answered with something unreadable — all
    /// of them leave `Update::Unknown` and put nothing on screen.
    pub fn check_for_update(&mut self, cx: &mut Context<Self>) {
        // An opt-out for anyone who would rather Zuno made no outbound request at start. An
        // env var rather than a setting: the people who want this are the people already
        // launching from a shell, and a toggle for it would need a home in a panel.
        if std::env::var_os("ZUNO_NO_UPDATE_CHECK").is_some() {
            return;
        }
        let Some(engine) = cx.engine() else { return };
        let spec = crate::update::latest_request(crate::app_state::defaults(cx));
        let (_job, events) = engine.send(spec);

        self.update_task = Some(cx.spawn(async move |this, cx| {
            let mut found = None;
            while let Ok(event) = events.recv().await {
                match event {
                    zuno_core::engine::Event::Done { response, .. } => {
                        found = crate::update::tag_from_response(&response);
                        break;
                    }
                    zuno_core::engine::Event::Failed { .. } => break,
                    _ => {}
                }
            }
            let _ = this.update(cx, |workspace, cx| {
                let Some(latest) = found else { return };
                workspace.update = if zuno_core::version::is_newer(&latest, crate::update::current())
                {
                    crate::update::Update::Available(latest)
                } else {
                    crate::update::Update::Current
                };
                cx.notify();
            });
        }));
    }

    /// The version to put in the chip, or `None` for no chip at all.
    fn offered_update(&self, cx: &App) -> Option<String> {
        let dismissed = crate::app_state::dismissed_update(cx);
        self.update
            .offered(dismissed.as_deref())
            .map(str::to_string)
    }

    /// The status line as rendered. A test asserting the clipboard alone cannot see whether the
    /// copy announced itself, and an unannounced clipboard write is indistinguishable from a
    /// dead control.
    /// What the chip would show. Read by tests directly, because `debug_bounds` reads the
    /// *last rendered frame* — a removed element keeps its entry until another frame is drawn,
    /// so `is_none()` there is not evidence of anything (CLAUDE.md).
    #[cfg(test)]
    pub fn offered_update_for_test(&self, cx: &App) -> Option<String> {
        self.offered_update(cx)
    }

    #[cfg(test)]
    pub fn status_for_test(&self, cx: &App) -> Option<SharedString> {
        self.status_message(cx)
    }

    #[cfg(test)]
    pub fn set_update_for_test(&mut self, update: crate::update::Update, cx: &mut Context<Self>) {
        self.update = update;
        cx.notify();
    }

    fn open_update_menu(&mut self, _: &OpenUpdateMenu, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(version) = self.offered_update(cx) else {
            return;
        };
        // The pointer's own position rather than one stashed at click time: `mouse_position`
        // is already in window coordinates, which is what `anchored` wants, so the chip needs
        // to carry nothing and there is no stale anchor to `take`. Pinned to just below the
        // titlebar on y, so the menu drops out of the chrome the way the app menu does rather
        // than overlapping the chip it came from.
        let at = gpui::point(
            window.mouse_position().x,
            gpui::px(crate::chrome::TITLEBAR_HEIGHT),
        );
        use context_menu::{MenuItem, MenuRow};
        // Both verbs are bound globally, so the context does not decide the lookup — but it
        // still has to be *a* handle, and the chip belongs to the window rather than a pane.
        let focus = self.focus_handle.clone();
        // Two rows because there are two questions, and one click can only answer one: how do
        // I update, and should I. The second is why this is a menu and not a single action.
        let rows = vec![
            MenuItem::new("Copy install command", CopyInstallCommand, &focus, window).into(),
            MenuItem::url(
                format!("What's new in {version}"),
                "",
                crate::update::RELEASES_URL,
            )
            .into(),
            MenuRow::Separator,
            MenuItem::new(
                "Dismiss until the next release",
                DismissUpdate,
                &focus,
                window,
            )
            .into(),
        ];
        let restore = self.active().map(|view| view.read(cx).url_focus(cx));
        self.show_menu(rows, at, restore, window, cx);
    }

    /// Copy, and *say* so. A clipboard write is invisible, so without the status line this
    /// reads as a dead control — and the second half of the message matters as much as the
    /// first: nobody should be left waiting for Zuno to install something itself.
    fn copy_install_command(
        &mut self,
        _: &CopyInstallCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            crate::update::INSTALL_COMMAND.to_string(),
        ));
        self.set_status("Install command copied — run it in a terminal", cx);
    }

    fn dismiss_update(&mut self, _: &DismissUpdate, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(version) = self.offered_update(cx) else {
            return;
        };
        crate::app_state::set_dismissed_update(cx, Some(version));
        cx.notify();
    }

    /// The header name being typed.
    fn suggest_target(&self, window: &Window, cx: &App) -> Option<(usize, String)> {
        let view = self.active()?.read(cx);
        let row = view.focused_header_name(window, cx)?;
        let typed = view.headers.get(row)?.name.read(cx).text().to_string();
        Some((row, typed))
    }

    /// What the list holds right now. Read by tests directly — `debug_bounds` reports a stale
    /// entry for a removed element until another frame is drawn, so `is_none()` there is not
    /// evidence the list closed.
    #[cfg(test)]
    pub fn suggestions_for_test(&self, window: &Window, cx: &App) -> Option<Vec<&'static str>> {
        self.suggest_items(window, cx).map(|(items, _)| items)
    }

    /// What the list would show right now, and which entry is highlighted.
    ///
    /// Derived rather than stored, so it cannot disagree with the cell it describes. The
    /// highlight only survives while focus stays on the row it was set for — a stale index
    /// against a different row would highlight an unrelated entry.
    fn suggest_items(&self, window: &Window, cx: &App) -> Option<(Vec<&'static str>, Option<usize>)> {
        let (row, typed) = self.suggest_target(window, cx)?;
        if self.suggest_dismissed == Some(row) {
            return None;
        }
        let items = zuno_core::headers::suggestions(&typed);
        if items.is_empty() {
            return None;
        }
        let highlighted = match self.suggest {
            Some((highlighted_row, ix)) if highlighted_row == row => {
                ix.filter(|ix| *ix < items.len())
            }
            _ => None,
        };
        Some((items, highlighted))
    }

    /// The dropdown under the focused header-name cell.
    ///
    /// **Owned here rather than inside the row**, which is what makes it possible at all: the
    /// request pane has ten `overflow_hidden` ancestors, and an absolutely-positioned child is
    /// still masked by one. Rendered from the root there is nothing to escape — the same move
    /// `context_menu` makes, for the same reason.
    ///
    /// **No scrim and no focus transfer**, which is what separates it from that menu. A scrim
    /// would swallow the next click, and focusing the list would stop the typing it exists to
    /// accompany.
    fn header_suggestions(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let theme = cx.theme().clone();
        let (row, _) = self.suggest_target(window, cx)?;
        let (items, highlighted) = self.suggest_items(window, cx)?;
        // The one thing that genuinely needs the cell to have been painted. A frame behind on
        // the very first draw of a new row, and harmless: a cell does not move while you type.
        let bounds = self.active()?.read(cx).header_name_bounds(row, cx)?;

        // Bottom-left of the cell, in window coordinates — `last_bounds` is already absolute,
        // so reading it as local would add the parent origin twice.
        let at = gpui::point(bounds.left(), bounds.bottom());

        Some(crate::ui::select_list(
            "header-suggestions",
            at,
            items.iter().map(|name| SharedString::from(*name)).collect(),
            highlighted,
            px(180.),
            &theme,
            cx,
            |workspace, ix, cx| {
                if let Some((row, _)) = workspace.suggest {
                    workspace.suggest = Some((row, Some(ix)));
                    cx.notify();
                }
            },
            |workspace, ix, window, cx| workspace.accept_suggestion(ix, window, cx),
        ))
    }

    /// Write the chosen name into the cell.
    ///
    /// Through select-all plus the ordinary edit path rather than by assigning the content, so
    /// `Ctrl+Z` undoes it and `Changed` still fires — the same reasoning as body prettify going
    /// through `Editor::replace_range`.
    fn accept_suggestion(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, _)) = self.suggest_target(window, cx) else {
            return;
        };
        let Some((items, _)) = self.suggest_items(window, cx) else {
            return;
        };
        let Some(name) = items.get(ix).copied() else {
            return;
        };
        let Some(view) = self.active() else {
            return;
        };
        view.update(cx, |view, cx| {
            let Some(input) = view.headers.get(row).map(|row| row.name.clone()) else {
                return;
            };
            input.update(cx, |input, cx| {
                input.select_all_text(cx);
                gpui::EntityInputHandler::replace_text_in_range(input, None, name, window, cx);
            });
        });
        self.suggest = None;
        // Not re-opened for the name just accepted: `suggestions` returns nothing for a
        // finished name, so there is nothing to dismiss.
        cx.notify();
    }

    fn suggest_next(&mut self, _: &SuggestNext, window: &mut Window, cx: &mut Context<Self>) {
        self.step_suggestion(1, window, cx);
    }

    fn suggest_prev(&mut self, _: &SuggestPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.step_suggestion(-1, window, cx);
    }

    fn step_suggestion(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        // Stepping is also how a dismissed list is reopened — pressing `down` is asking for it.
        let Some((row, _)) = self.suggest_target(window, cx) else {
            return;
        };
        self.suggest_dismissed = None;
        let Some((items, highlighted)) = self.suggest_items(window, cx) else {
            return;
        };
        let next = match highlighted {
            // First press lands on the first entry going down, the last going up, rather than
            // on whatever index happened to be stored.
            None if delta > 0 => 0,
            None => items.len() - 1,
            Some(ix) => (ix as isize + delta).rem_euclid(items.len() as isize) as usize,
        };
        self.suggest = Some((row, Some(next)));
        cx.notify();
    }

    fn suggest_confirm(&mut self, _: &SuggestConfirm, window: &mut Window, cx: &mut Context<Self>) {
        // Nothing highlighted means nothing was chosen. This is the rule that keeps a typed
        // `X-Trace-Id` from being replaced by whatever the list happened to rank first.
        let Some((_, Some(ix))) = self.suggest else {
            return;
        };
        self.accept_suggestion(ix, window, cx);
    }

    /// `escape` in a header cell closes the list — and when there is no list, still cancels an
    /// in-flight request.
    ///
    /// The fallback is not optional: this binding is scoped to `HeaderCell` and registered after
    /// the global `escape`, so it *wins* whenever a header name has focus. Without forwarding,
    /// putting the cursor in a header cell would quietly disarm cancelling a request.
    fn suggest_dismiss(&mut self, _: &SuggestDismiss, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, _)) = self.suggest_target(window, cx)
            && self.suggest_items(window, cx).is_some()
        {
            self.suggest = None;
            self.suggest_dismissed = Some(row);
            cx.notify();
            return;
        }
        self.cancel_request(&CancelRequest, window, cx);
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

        let suggested = suggested_filename(
            &view.read(cx).label(cx),
            response.content_type(),
            response.header("content-disposition"),
        );
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
        let environment = self.environment.clone();
        view.update(cx, |view, cx| view.send(&engine, &resolver, environment, cx));
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
                RowKind::Capture => "capture",
                RowKind::Assert => "assertion",
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
        let cert_files = crate::app_state::tls(cx);
        let certs_active = !cert_files.is_empty();
        let update_offer = self.offered_update(cx).map(SharedString::from);
        let proxy_badge = proxy_badge_label(
            &crate::app_state::proxy(cx),
            self.system_proxy.as_deref(),
        );
        let (badge, resolving) =
            environment_badge(self.environment.as_deref(), self.globals_active);
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
            .on_action(cx.listener(Self::import_browse))
            .on_action(cx.listener(Self::open_part_kind_menu))
            .on_action(cx.listener(Self::show_capture_tab))
            .on_action(cx.listener(Self::run_folder))
            .on_action(cx.listener(Self::run_flow))
            .on_action(cx.listener(Self::add_to_flow))
            .on_action(cx.listener(Self::edit_flows))
            .on_action(cx.listener(Self::flow_new))
            .on_action(cx.listener(Self::flow_rename))
            .on_action(cx.listener(Self::flow_trash))
            .on_action(cx.listener(Self::flow_next))
            .on_action(cx.listener(Self::flow_prev))
            .on_action(cx.listener(Self::flow_step_next))
            .on_action(cx.listener(Self::flow_step_prev))
            .on_action(cx.listener(Self::flow_step_up))
            .on_action(cx.listener(Self::flow_step_down))
            .on_action(cx.listener(Self::flow_step_remove))
            .on_action(cx.listener(Self::flow_confirm))
            .on_action(cx.listener(Self::flow_dismiss))
            .on_action(cx.listener(Self::run_dismiss))
            .on_action(cx.listener(Self::show_assert_tab))
            .on_action(cx.listener(Self::add_assertion))
            .on_action(cx.listener(Self::cycle_assert_op))
            .on_action(cx.listener(Self::assert_value))
            .on_action(cx.listener(Self::add_capture))
            .on_action(cx.listener(Self::toggle_capture_secret))
            .on_action(cx.listener(Self::capture_value))
            .on_action(cx.listener(Self::edit_environments))
            .on_action(cx.listener(Self::env_next))
            .on_action(cx.listener(Self::env_prev))
            .on_action(cx.listener(Self::env_new_variable))
            .on_action(cx.listener(Self::env_remove_variable))
            .on_action(cx.listener(Self::env_toggle_secret))
            .on_action(cx.listener(Self::env_new_environment))
            .on_action(cx.listener(Self::env_rename_environment))
            .on_action(cx.listener(Self::env_trash_environment))
            .on_action(cx.listener(Self::env_confirm))
            .on_action(cx.listener(Self::env_dismiss))
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
            .on_action(cx.listener(Self::new_request))
            .on_action(cx.listener(Self::move_request))
            .on_action(cx.listener(Self::import_document))
            .on_action(cx.listener(Self::format_body))
            .on_action(cx.listener(Self::minify_body))
            .on_action(cx.listener(Self::import_confirm))
            .on_action(cx.listener(Self::import_dismiss))
            .on_action(cx.listener(Self::switch_environment))
            .on_action(cx.listener(Self::show_history))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_defaults))
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
            .on_action(cx.listener(Self::set_proxy))
            .on_action(cx.listener(Self::remove_proxy))
            .on_action(cx.listener(Self::choose_client_cert))
            .on_action(cx.listener(Self::choose_root_ca))
            .on_action(cx.listener(Self::open_certificates))
            .on_action(cx.listener(Self::certs_dismiss))
            .on_action(cx.listener(Self::certs_next))
            .on_action(cx.listener(Self::certs_prev))
            .on_action(cx.listener(Self::certs_confirm))
            .on_action(cx.listener(Self::certs_remove))
            .on_action(cx.listener(Self::open_tab_menu))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_tabs_to_the_right))
            .on_action(cx.listener(Self::close_all_tabs))
            .on_action(cx.listener(Self::next_response_tab))
            .on_action(cx.listener(Self::prev_response_tab))
            .on_action(cx.listener(Self::show_response_body))
            .on_action(cx.listener(Self::show_response_headers))
            .on_action(cx.listener(Self::show_response_timing))
            .on_action(cx.listener(Self::show_response_diff))
            .on_action(cx.listener(Self::toggle_html_view))
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
            .on_action(cx.listener(Self::open_update_menu))
            .on_action(cx.listener(Self::copy_install_command))
            .on_action(cx.listener(Self::dismiss_update))
            .on_action(cx.listener(Self::suggest_next))
            .on_action(cx.listener(Self::suggest_prev))
            .on_action(cx.listener(Self::suggest_confirm))
            .on_action(cx.listener(Self::suggest_dismiss))
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
                certs_active,
                update_offer,
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
            .child({
                // What the editor column will actually get: the row is the window less the
                // panel, and the column takes the rest. Zero when the panel is hidden.
                let panel_width = if self.panel_visible {
                    self.clamped_panel_width(window)
                } else {
                    0.
                };
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    // The frame the resize handle is positioned against. It is absolute rather
                    // than a sibling in this row so that a strip wide enough to grab costs
                    // neither the panel nor the panes any width.
                    .relative()
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
                            // The panel is `flex_none`, so the panes are what must give when
                            // the window narrows. Without this the column's content sets a floor
                            // and the two together overflow instead.
                            .min_w(px(0.))
                            .overflow_hidden()
                            // Width the strip will actually get: the row is the window less
                            // the panel, and the editor column takes the rest. Computed here
                            // rather than read off the scroll handle, whose extent is written
                            // during prepaint and is therefore a frame behind (§6).
                            .children(tab_strip(
                                tabs,
                                &self.tab_scroll,
                                f32::from(window.viewport_size().width) - panel_width,
                                &theme,
                                cx,
                            ))
                            .children(self.active()),
                    )
                    // Last, so hit-testing gives the seam to the handle rather than to
                    // whichever pane it overlaps.
                    .children(self.panel_visible.then(|| {
                        crate::collection_panel::resize_handle(self, &theme, window, cx)
                    }))
            })
            .child(status_bar(
                focused_region,
                status_message,
                cookies,
                proxy_badge,
                badge,
                resolving,
                &theme,
                window,
            ))
            // Above the panes, below the resize edges.
            .children(self.picker.as_ref().map(|state| state.picker.clone()))
            .children(self.settings.as_ref().map(|state| state.panel.clone()))
            .children(self.import.as_ref().map(|state| state.panel.clone()))
            .children(self.new_workspace_panel.as_ref().map(|state| state.panel.clone()))
            .children(self.environment_panel.as_ref().map(|state| state.panel.clone()))
            .children(self.run.as_ref().map(|state| state.panel.clone()))
            .children(self.flows.as_ref().map(|state| state.panel.clone()))
            .children(self.menu.as_ref().map(|state| state.menu.clone()))
            .children(self.header_suggestions(window, cx))
            .children(self.part_kind_select(cx))
            // Built here rather than held as an `Entity`: it owns no input and no state beyond
            // which button is selected, so it is plain workspace state like `RenameState`.
            .children(
                self.close_confirm
                    .as_ref()
                    .map(|state| crate::close_panel::render(state, &theme, cx)),
            )
            .children(self.certs.as_ref().map(|panel| {
                crate::cert_panel::render(panel, &cert_files, &theme, cx)
            }))
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
/// `px_3` either side, `gap_1` between label and button, and the 1px `border_r_1` dividing one
/// tab from the next. The active marker is `border_t_2`, so it costs height rather than width.
const TAB_CHROME_WIDTH: f32 = 12. * 2. + 4. + 1.;
const TAB_WIDTH: f32 = TAB_LABEL_WIDTH + TAB_CLOSE_WIDTH + TAB_CHROME_WIDTH;

/// Width of one scroll chevron.
const TAB_CHEVRON_WIDTH: f32 = 20.;

/// Whether the tabs need more room than the strip has.
///
/// A pure function so the threshold can be checked without a window — the alternative is
/// asking the scroll handle, whose extent is written during prepaint and so reads a frame
/// behind (§6). Being wrong here has two different costs: too eager shows a chevron with
/// nothing to reach, too shy leaves a tab unreachable by mouse. It deliberately does **not**
/// reserve the chevrons' own width, which would be circular — if the tabs fit, no chevrons
/// appear and none are needed; if they do not, the chevrons appear and there is still
/// something to scroll to.
fn tabs_overflow(tab_count: usize, available_width: f32) -> bool {
    tab_count as f32 * TAB_WIDTH > available_width
}

/// The offset that brings tab `active_ix` into view, or `None` when it is already there.
///
/// **Pure, and computed rather than measured.** Asking the scroll handle for that tab's bounds
/// would be a frame behind — and worse, on the frame a *newly created* tab first appears it has
/// no bounds at all, which is exactly the case this exists for. Tabs are a fixed width, so
/// arithmetic knows where the nth one is before anything is laid out.
///
/// Returning `None` for "already visible" is what keeps `activate` from firing an animation on
/// every ordinary tab click.
fn reveal_offset(
    active_ix: usize,
    tab_count: usize,
    available_width: f32,
    current_offset: f32,
) -> Option<f32> {
    if !tabs_overflow(tab_count, available_width) {
        return None;
    }

    // The tab area is the strip less both chevrons, which are drawn exactly when it overflows.
    // Floored at one tab: a window narrow enough for the chevrons to eat the whole strip would
    // otherwise produce a negative viewport and scroll nonsense.
    let viewport = (available_width - 2. * TAB_CHEVRON_WIDTH).max(TAB_WIDTH);

    let left = active_ix as f32 * TAB_WIDTH;
    let right = left + TAB_WIDTH;
    // gpui's offset is <= 0 and grows more negative as content moves left, so the first visible
    // pixel of content is at `-offset`.
    let shown_left = -current_offset;
    let shown_right = shown_left + viewport;

    if left < shown_left {
        // Off the left edge: bring its leading edge to the left of the viewport.
        Some(-left)
    } else if right > shown_right {
        // Off the right edge: bring its trailing edge to the right of the viewport, which is
        // the smaller move and keeps the tabs before it on screen.
        Some(-(right - viewport))
    } else {
        None
    }
}

#[cfg(test)]
mod strip_tests {
    use super::*;

    /// A strip wide enough for three tabs, so the fourth is the one out of reach.
    fn three_wide() -> f32 {
        3. * TAB_WIDTH + 2. * TAB_CHEVRON_WIDTH
    }

    #[test]
    fn the_proxy_badge_always_says_something() {
        // It used to be `Option` and hid itself in the default state, which left the feature
        // with no visible surface at all.
        let label = |mode: ProxyMode, env: Option<&str>| {
            proxy_badge_label(&mode, env).to_string()
        };

        assert_eq!(label(ProxyMode::System, None), "proxy system");
        assert_eq!(label(ProxyMode::Off, None), "proxy off");
        assert_eq!(
            label(ProxyMode::Off, Some("http://ignored:8080")),
            "proxy off",
            "off ignores the environment, so naming it would be a lie"
        );
        // The mode being System and a proxy actually being used are different facts.
        assert_eq!(
            label(ProxyMode::System, Some("http://corp:3128")),
            "proxy corp:3128 · env"
        );
        assert_eq!(
            label(ProxyMode::Url("https://p.test:8443".into()), None),
            "proxy p.test:8443"
        );
    }

    #[test]
    fn a_tab_already_in_view_is_left_alone() {
        // This is what keeps an ordinary tab click from firing an animation, and what stops
        // `activate` fighting a scroll the reader just made with the wheel.
        assert_eq!(reveal_offset(1, 10, three_wide(), 0.), None);
        assert_eq!(reveal_offset(0, 2, 4_000., 0.), None, "no overflow, nothing to do");
    }

    #[test]
    fn a_new_tab_past_the_right_edge_is_pulled_into_view() {
        // The reported bug: Ctrl+T appends, so the new tab lands off the right edge and
        // pressing it repeatedly looks like a dead key.
        let offset = reveal_offset(3, 4, three_wide(), 0.).expect("tab 3 is off the right edge");

        // Its trailing edge should sit exactly at the viewport's right edge, which is the
        // smallest move that reveals it — asserted as a position, not as "the value changed".
        let viewport = three_wide() - 2. * TAB_CHEVRON_WIDTH;
        assert_eq!(offset, -(4. * TAB_WIDTH - viewport));

        // And from there it really is inside the window.
        assert_eq!(reveal_offset(3, 4, three_wide(), offset), None);
    }

    #[test]
    fn a_tab_off_the_left_edge_is_pulled_back() {
        // Ctrl+Shift+Tab backwards past the visible range, or clicking a tab in the picker.
        // Scrolled far right, then asked for tab 0.
        let offset = reveal_offset(0, 10, three_wide(), -5. * TAB_WIDTH)
            .expect("tab 0 is off the left edge");
        assert_eq!(offset, 0., "its leading edge goes to the left of the viewport");
    }

    #[test]
    fn a_window_too_narrow_for_the_chevrons_still_scrolls_somewhere_sane() {
        // The chevrons could otherwise eat the whole strip and leave a negative viewport,
        // which would put the offset on the wrong side of zero and scroll the tabs away.
        let offset = reveal_offset(5, 6, TAB_CHEVRON_WIDTH * 2. + 4., 0.).expect("overflowing");
        assert!(offset <= 0., "gpui's offsets are never positive: {offset}");
    }

    #[test]
    fn the_chevrons_appear_exactly_when_a_tab_is_out_of_reach() {
        // Both directions matter and they fail differently. Too shy and a tab past the right
        // edge has no mouse path at all, which is the bug the chevrons exist to fix. Too eager
        // and they are dead controls, which this codebase keeps finding one way or another.
        let two = 2. * TAB_WIDTH;

        assert!(!tabs_overflow(2, two), "two tabs in exactly their own width fit");
        assert!(!tabs_overflow(2, two + 1.), "and fit with room to spare");
        assert!(tabs_overflow(2, two - 1.), "a pixel short is out of reach");

        // The case that motivated this: a normal window and enough tabs to run past it.
        assert!(tabs_overflow(12, 900.));
        assert!(!tabs_overflow(3, 900.));
    }
}

/// One end's scroll chevron.
///
/// **Deliberately not a `ui::icon_button`**, which dispatches an action and titles itself with
/// that action's keystroke. Scrolling a viewport is not a verb: there is no keybinding to teach,
/// and inventing an action would need either a command-palette row nobody would search for or an
/// `EXCLUDED` entry explaining why it is not one. It is chrome for the strip, in the same family
/// as the response body's scroll indicator — the difference being that this one is meant to be
/// clicked, so it takes a pointer cursor and a hover.
///
/// **Always live, never dimmed.** Knowing you are already at an end means reading the scroll
/// extent, which is a frame behind; a chevron that greys out one tab early reads as broken,
/// while a click that cannot move simply does nothing — gpui clamps `offset.x` to `[-max, 0]`
/// in its own prepaint.
fn scroll_chevron(
    id: &'static str,
    icon: crate::ui::Icon,
    delta: f32,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    // Copied out rather than captured: a closure holding `theme` would borrow it for the
    // element's whole life, which `impl IntoElement + use<>` cannot express.
    let hover_bg = theme.bg_hover;

    div()
        .id(id)
        .debug_selector(move || id.to_string())
        // `ui::glyph` reaches its hover colour through the group, since `hover` on a parent does
        // not inherit into an `svg()` any more than `text_color` does.
        .group(crate::ui::ICON_GROUP)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(TAB_CHEVRON_WIDTH))
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |workspace, _: &MouseDownEvent, _, cx| {
                workspace.nudge_tabs(delta, cx);
            }),
        )
        .child(crate::ui::glyph(icon, theme.text_muted, theme.text, 12.))
}

/// The strip of open buffers.
///
/// Hidden entirely at one buffer: a single tab is a row of chrome that says nothing, and
/// the window title already names the request. It appears the moment there's a choice to
/// make, which is also the moment it starts carrying information.
///
/// **The chevrons are pinned siblings, not children of the scrolling box**, so they stay put
/// while the tabs move between them. Inside the scroll container they would slide away with the
/// content, which is the one thing a scroll control must not do.
fn tab_strip(
    tabs: Vec<(usize, SharedString, bool, bool)>,
    scroll: &ScrollHandle,
    available_width: f32,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> Option<impl IntoElement> {
    if tabs.len() < 2 {
        return None;
    }

    // One tab per click: each press brings exactly one more into view, so holding it walks the
    // strip. A screenful would be faster and leaves nothing on screen to orient by.
    let overflowing = tabs_overflow(tabs.len(), available_width);

    Some(
        div()
            .flex()
            .flex_row()
            // **No `items_center` here, deliberately.** gpui leaves `align_items` as `None`,
            // which taffy reads as `stretch` — so the chevrons grow to the strip's full height
            // and their hit area is the whole end of the row rather than a 12px box floating in
            // it. That is also what fixes the hover: a full-height button looks like one. The
            // scrolling box below keeps its own `items_center` for the tabs.
            .flex_none()
            .w_full()
            // The strip's own surface and its bottom rule live out here now, so they run the
            // full width behind the chevrons rather than stopping where the tabs do.
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .children(overflowing.then(|| {
                scroll_chevron(
                    "tab-scroll-left",
                    crate::ui::Icon::ChevronLeft,
                    TAB_WIDTH,
                    theme,
                    cx,
                )
            }))
            .child(
                div()
                    .id("tab-strip")
                    // The handle is the only way to scroll this from a click; omitting it leaves
                    // `set_offset` writing to a state nothing reads.
                    .track_scroll(scroll)
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_row()
                    .items_center()
                    // Many tabs must scroll rather than squeeze every label into illegibility.
                    // `overflow_x_scroll` lives on `StatefulInteractiveElement`, hence `.id()`.
                    .overflow_x_scroll()
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
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |workspace, event: &MouseDownEvent, window, cx| {
                            // Activate first, so the menu's rows can all act on the active
                            // buffer. `position` is already in window coordinates, which is
                            // what `anchored()` wants.
                            workspace.activate(ix, window, cx);
                            workspace.tab_menu_anchor = Some(event.position);
                            window.dispatch_action(Box::new(OpenTabMenu), cx);
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
                    // not — so the accent marker and the neutral divider above cannot share an
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
                            // Accent along the top edge. Inactive tabs keep the width and
                            // paint it in the strip's own background, so switching never
                            // reflows the label by 2px.
                            .border_t_2()
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
            .children(overflowing.then(|| {
                scroll_chevron(
                    "tab-scroll-right",
                    crate::ui::Icon::ChevronRight,
                    // Negative: gpui's offset grows more negative as content moves left.
                    -TAB_WIDTH,
                    theme,
                    cx,
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
pub fn suggested_filename(
    label: &str,
    content_type: Option<&str>,
    disposition: Option<&str>,
) -> String {
    // Match on the essence only: `application/json; charset=utf-8` is still JSON.
    let base = content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("");
    let extension = extension_for(base);

    // **The server's own name wins.** A download endpoint that says
    // `attachment; filename="invoices-2026-Q1.xlsx"` knows something the URL does not, and
    // guessing `api-v1-export.xlsx` from the path throws it away. `disposition::filename` has
    // already reduced it to a single safe path segment — see it for why that is not optional.
    if let Some(name) = disposition.and_then(zuno_core::disposition::filename) {
        // A name the server sent without any extension still gets the one its content type
        // implies. Appending unconditionally would produce `report.csv.csv`; replacing what is
        // there would override a server that knows its own format better than the sniff does.
        return if std::path::Path::new(&name).extension().is_some() {
            name
        } else {
            format!("{name}.{extension}")
        };
    }

    format!("{}.{extension}", collection::slug(label))
}

/// Media types whose extension is not simply their subtype, plus the `application/*` types worth
/// naming. Sorted, and `the_extension_table_is_sorted_and_unique` keeps it that way.
///
/// `application/*` is an allowlist rather than a derivation because its subtypes are mostly not
/// extensions — `octet-stream`, `vnd.openxmlformats-officedocument.spreadsheetml.sheet` — whereas
/// under `image/`, `audio/`, `video/` and `font/` the subtype usually *is* one.
const EXTENSIONS: &[(&str, &str)] = &[
    ("application/gzip", "gz"),
    ("application/javascript", "js"),
    ("application/json", "json"),
    ("application/msword", "doc"),
    ("application/pdf", "pdf"),
    ("application/rtf", "rtf"),
    ("application/sql", "sql"),
    ("application/vnd.ms-excel", "xls"),
    ("application/vnd.ms-powerpoint", "ppt"),
    ("application/vnd.openxmlformats-officedocument.presentationml.presentation", "pptx"),
    ("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "xlsx"),
    ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "docx"),
    ("application/wasm", "wasm"),
    ("application/x-gzip", "gz"),
    ("application/x-ndjson", "ndjson"),
    ("application/x-tar", "tar"),
    ("application/x-yaml", "yaml"),
    ("application/xml", "xml"),
    ("application/yaml", "yaml"),
    ("application/zip", "zip"),
    ("audio/mpeg", "mp3"),
    ("audio/vnd.wave", "wav"),
    ("audio/x-wav", "wav"),
    ("image/jpeg", "jpg"),
    ("image/svg+xml", "svg"),
    ("image/vnd.microsoft.icon", "ico"),
    ("image/x-icon", "ico"),
    ("text/csv", "csv"),
    ("text/html", "html"),
    ("text/javascript", "js"),
    ("text/json", "json"),
    ("text/markdown", "md"),
    ("text/xml", "xml"),
    ("text/yaml", "yaml"),
];

/// The table's keys, for the drift test that keeps it binary-searchable.
#[cfg(test)]
pub fn extension_table_names() -> Vec<&'static str> {
    EXTENSIONS.iter().map(|(name, _)| *name).collect()
}

/// The extension for a media type's essence, or `bin` when nothing is known.
///
/// **`.bin` used to be the answer for everything outside a five-entry list**, on the reasoning
/// that it claims nothing. Claiming nothing is the wrong goal: a JPEG saved as `blob.bin` does
/// not open by double-clicking it, so the conservative choice moved work onto the person rather
/// than avoiding a mistake. `image/jpeg` is not ambiguous.
fn extension_for(base: &str) -> String {
    let base = base.to_ascii_lowercase();
    if let Ok(ix) = EXTENSIONS.binary_search_by(|(name, _)| (*name).cmp(base.as_str())) {
        return EXTENSIONS[ix].1.to_string();
    }

    // Under these four the subtype is usually already an extension — `image/png`, `video/mp4`,
    // `font/woff2` — so deriving covers the long tail (avif, heic, opus, jxl) without a table
    // that goes stale each time a format ships. `application/*` gets no derivation because its
    // subtypes are mostly not extensions: `octet-stream`, `vnd.openxmlformats-…`.
    if let Some((family, subtype)) = base.split_once('/')
        && matches!(family, "image" | "audio" | "video" | "font")
    {
        // A structured suffix names the *syntax*, not the format: `image/svg+xml` is an SVG.
        let subtype = subtype.split('+').next().unwrap_or(subtype);
        let subtype = subtype.strip_prefix("x-").unwrap_or(subtype);
        // **This filter is a guard, not tidiness.** The string comes from a response header, so
        // an `image/../../.ssh/config` would otherwise walk straight past `collection::slug`,
        // which only sanitizes the *label* half of the name. Charset and length together mean
        // nothing but a plausible extension can survive.
        if (1..=5).contains(&subtype.len())
            && subtype.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return subtype.to_string();
        }
    }

    if base.starts_with("text/") { "txt" } else { "bin" }.to_string()
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
/// A certificate's filename, which is the only part of a long path worth a status chip.
fn cert_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// What the status bar says about the proxy. **Always something.**
///
/// It was `Option`, shown only when a proxy was in effect — so in the default state there was
/// no badge, no icon and no hint the feature existed, reachable only by a palette row nobody
/// would think to search for. That is the discoverability rule below, broken by the thing that
/// was meant to satisfy it.
///
/// `system` and the environment's value are distinguished, because "the mode is System" and "a
/// proxy is actually being used" are different facts and only one of them is a warning.
///
/// Takes the environment's value as an argument rather than reading it, so it is unit-testable:
/// `std::env::set_var` is `unsafe` under edition 2024 and racy across parallel tests.
fn proxy_badge_label(mode: &ProxyMode, system: Option<&str>) -> SharedString {
    let host = |value: &str| {
        value
            .trim()
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string()
    };

    SharedString::from(match mode {
        ProxyMode::Off => "proxy off".to_string(),
        ProxyMode::System => match system {
            Some(value) => format!("proxy {} · env", host(value)),
            None => "proxy system".to_string(),
        },
        ProxyMode::Url(url) => format!("proxy {}", host(url)),
    })
}

/// Offer the typed text as a proxy, when it could be one.
///
/// `ProxyMode::from_input` decides, in core, where it is unit-tested — so the picker cannot
/// offer a row that fails at the next send, which is the dead-control shape one layer down.
fn typed_proxy_row(query: &str) -> Option<picker::Item> {
    let mode = ProxyMode::from_input(query)?;
    Some(picker::Item {
        label: SharedString::from(mode.label().to_string()),
        detail: SharedString::from("use this proxy"),
        target: picker::Target::Proxy(mode),
    })
}

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
/// A variable name suggested from a JSONPath: the last segment, or empty when there isn't one.
///
/// A suggestion, not a rule — the name box is focused when a capture is authored this way, so
/// the first thing you can do is disagree with it.
fn capture_name(path: &str) -> String {
    path.rsplit(['.', '['])
        .next()
        .unwrap_or_default()
        .trim_end_matches(']')
        .trim_matches('"')
        .to_string()
}

/// What the environment badge says, and whether anything is actually substituting.
///
/// **Three states, because "nothing selected" and "nothing substituting" are different things.**
/// `globals.json` is the bottom layer whether or not an environment is chosen, so with none
/// chosen a `{{var}}` may still resolve. The badge showed a bare globe for both cases and the
/// switcher's "None" row *said* variables were left unresolved — false whenever globals had
/// values, and a confident string in the UI is read by more people than a stale comment is.
///
/// A pure function so the rule is testable: nothing headless can read a rendered badge.
pub fn environment_badge(environment: Option<&str>, globals_active: bool) -> (SharedString, bool) {
    match (environment, globals_active) {
        (Some(name), _) => (SharedString::from(name.to_string()), true),
        (None, true) => (SharedString::from(environment::GLOBALS), true),
        (None, false) => (SharedString::from("none"), false),
    }
}

/// Whether the always-active layer has anything to substitute.
///
/// A file read, so every caller is a state change rather than a frame — see `globals_active`.
fn globals_has_values(cx: &App) -> bool {
    crate::collections::root(cx)
        .map(Path::to_path_buf)
        .and_then(|root| environment::load(&root, environment::GLOBALS).ok())
        .is_some_and(|env| !env.values.is_empty())
}

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
    proxy: SharedString,
    environment: SharedString,
    resolving: bool,
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
                // Which environment a request will be sent against changes where it goes and
                // what credentials it carries, so it belongs on screen rather than two keystrokes
                // away. **One shape in every state**, globe plus word: it used to be a bare globe
                // with nothing selected and a bare word otherwise, so the control moved and
                // changed size depending on what it was reporting. The globe is now a fixed
                // anchor and the word carries the state — see `environment_badge` for why there
                // are three of those rather than two.
                .child(crate::ui::icon_text_action(
                    "environment-badge",
                    crate::ui::Icon::Globe,
                    environment,
                    "Switch environment",
                    SwitchEnvironment,
                    if resolving { theme.text_muted } else { theme.text_faint },
                    theme,
                ))
                .children(cookies.then(|| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.bg_elevated)
                        .text_color(theme.accent)
                        // `GLYPH_INLINE`, not `GLYPH`: beside a word the larger size is 25%
                        // taller than the text.
                        .child(crate::ui::glyph(
                            crate::ui::Icon::Cookie,
                            theme.accent,
                            theme.accent,
                            crate::ui::GLYPH_INLINE,
                        ))
                        .child("cookies on")
                }))
                // **The half that actually earns this feature.** The toggle says what *will*
                // happen; the badge says what *is* happening — which is the cookie jar's own
                // lesson, and the whole reason a silently-honoured `HTTP_PROXY` was worth
                // fixing rather than just documenting.
                .child({
                    div()
                        .id("proxy-badge")
                        .debug_selector(|| "proxy-badge".to_string())
                        // The glyph cannot be reached by an ancestor's `hover`, so the badge is
                        // the group and the icon brightens through it.
                        .group(crate::ui::ICON_GROUP)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.bg_elevated)
                        .text_color(theme.accent)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.bg_hover))
                        // A one-click switch, like the environment badge beside it, rather than
                        // a label you have to find a command for.
                        .on_mouse_down(
                            MouseButton::Left,
                            |_: &MouseDownEvent, window, cx| {
                                window.dispatch_action(Box::new(SetProxy), cx);
                            },
                        )
                        .child(crate::ui::glyph(
                            crate::ui::Icon::Waypoints,
                            theme.accent,
                            theme.text,
                            crate::ui::GLYPH_INLINE,
                        ))
                        .child(proxy)
                })
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
