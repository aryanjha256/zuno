//! The side panel's Git view: the workspace's branch, what changed since the last commit, and the
//! commit box — plus the states before there is a repository at all.
//!
//! **The workspace is the repository.** Git runs as the user's own `git` (see `zuno_core::git`),
//! off the UI thread; this module holds what the last run found and draws it. Actions live on
//! `Workspace`, as every handler does.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext as _, Context, Div, Entity, Global, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Task, UniformListScrollHandle, div, px, uniform_list,
};
use zuno_core::collection::{Badge, NodeKind};
use zuno_core::git::{ChangeKind, FileChange, Git, Probe, Status};

use crate::actions::{
    GitAbort, GitCommit, GitContinue, GitInit, GitOpenConflict, GitOpenDownload, GitPublish,
    GitRefresh, GitSaveIdentity, GitSync, GitToggleIdentityScope,
};
use crate::input::{Editor, TextInput};
use crate::theme::Theme;
use crate::ui::{Icon, glyph};
use crate::workspace::Workspace;

/// Which `git` to run. The tests install one with its own global config, so nothing they do can
/// touch the developer's (invariant 6); the app uses the system's.
pub struct GitRunner(pub Git);

impl Global for GitRunner {}

pub fn runner(cx: &App) -> Git {
    cx.try_global::<GitRunner>()
        .map(|runner| runner.0.clone())
        .unwrap_or_else(Git::system)
}

pub struct GitState {
    /// What the workspace folder is. `None` until the first check finishes.
    pub probe: Option<Probe>,
    pub status: Option<Status>,
    /// What is running, for the button that started it.
    pub busy: Option<&'static str>,
    /// The last failure, in git's words.
    pub error: Option<String>,
    /// Shown in place of the commit box after a commit git refused for want of a name.
    pub identity: Option<IdentityForm>,
    pub message: Entity<Editor>,
    /// Publish's remote URL.
    pub remote_url: Entity<TextInput>,
    pub scroll: UniformListScrollHandle,
    pub(crate) refresh: Option<Task<()>>,
    pub(crate) work: Option<Task<()>>,
}

pub struct IdentityForm {
    pub name: Entity<TextInput>,
    pub email: Entity<TextInput>,
    /// Write it to the global config rather than this repository's.
    pub everywhere: bool,
}

impl GitState {
    pub fn new(cx: &mut Context<Workspace>) -> Self {
        let message = cx.new(|cx| {
            let mut editor = Editor::new("", "Message — Ctrl+Enter commits", cx);
            editor.set_key_context("TextInput BodyEditor GitMessage");
            editor.leave_tab_order();
            editor
        });
        let remote_url = cx.new(|cx| {
            let mut input =
                TextInput::new(String::new(), "git@github.com:you/workspace.git", "GitRemote", cx);
            input.leave_tab_order();
            input
        });
        Self {
            probe: None,
            status: None,
            busy: None,
            error: None,
            identity: None,
            message,
            remote_url,
            scroll: UniformListScrollHandle::new(),
            refresh: None,
            work: None,
        }
    }

    pub fn repo(&self) -> Option<&zuno_core::git::Repo> {
        match &self.probe {
            Some(Probe::Repo(repo)) => Some(repo),
            _ => None,
        }
    }

    /// How many files differ from the last commit, when this is a repository.
    pub fn change_count(&self) -> usize {
        self.status.as_ref().map_or(0, |status| status.files.len())
    }

    /// Each changed file by absolute path, and every folder above one — what the tree marks.
    pub fn markers(&self, root: &Path) -> (HashMap<PathBuf, ChangeKind>, HashSet<PathBuf>) {
        let mut files = HashMap::new();
        let mut folders = HashSet::new();
        let Some(status) = &self.status else {
            return (files, folders);
        };
        for change in &status.files {
            let path = root.join(&change.path);
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if dir == root || !dir.starts_with(root) {
                    break;
                }
                folders.insert(dir.to_path_buf());
                parent = dir.parent();
            }
            files.insert(path, change.kind);
        }
        (files, folders)
    }
}

impl IdentityForm {
    pub fn new(cx: &mut Context<Workspace>) -> Self {
        let field = |placeholder: &'static str, cx: &mut Context<Workspace>| {
            cx.new(|cx| {
                let mut input = TextInput::new(String::new(), placeholder, "GitIdentity", cx);
                input.leave_tab_order();
                input
            })
        };
        Self {
            name: field("Your name", cx),
            email: field("you@example.com", cx),
            everywhere: true,
        }
    }
}

/// The colour a change is marked in: amber for edits, green for new, red for gone or conflicting.
pub fn change_colour(kind: ChangeKind, theme: &Theme) -> gpui::Hsla {
    match kind {
        ChangeKind::Modified | ChangeKind::Renamed => theme.status_client_error,
        ChangeKind::Added => theme.status_success,
        ChangeKind::Deleted | ChangeKind::Conflicted => theme.status_server_error,
    }
}

/// The view under the panel's header.
pub fn render(workspace: &Workspace, theme: &Theme, cx: &mut Context<Workspace>) -> Vec<gpui::AnyElement> {
    let git = &workspace.git;
    match &git.probe {
        None => vec![notice("Checking git…", theme).into_any_element()],
        Some(Probe::NotInstalled) => vec![not_installed(theme).into_any_element()],
        Some(Probe::NotARepo) => vec![not_a_repo(git, theme).into_any_element()],
        Some(Probe::Repo(repo)) => {
            let mut body = vec![toolbar(git, theme).into_any_element()];
            let status = git.status.as_ref();
            let stopped = status.is_some_and(|status| status.operation.is_some());
            if status.is_some_and(|status| !status.remote) {
                body.push(publish_card(git, theme).into_any_element());
            }
            body.push(match (&git.identity, stopped) {
                (_, true) => conflict_box(git, theme).into_any_element(),
                (Some(form), false) => identity_box(form, git, theme).into_any_element(),
                (None, false) => commit_box(git, theme).into_any_element(),
            });
            body.push(changes(workspace, theme, cx).into_any_element());
            if repo.is_nested() {
                let outside = git.status.as_ref().map_or(0, |status| status.outside);
                let text = match outside {
                    0 => "Commits only this workspace's folder.".to_string(),
                    1 => "Commits only this workspace's folder. 1 other change in the repo is \
                          left alone."
                        .to_string(),
                    n => format!(
                        "Commits only this workspace's folder. {n} other changes in the repo are \
                         left alone."
                    ),
                };
                body.push(footnote(text, theme).into_any_element());
            }
            body
        }
    }
}

fn notice(text: &'static str, theme: &Theme) -> Div {
    div().px_3().pt_3().text_xs().text_color(theme.text_faint).child(text)
}

/// The centred icon, title and line every empty state here opens with.
fn empty_state(title: &'static str, body: &'static str, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(10.))
        .px(px(20.))
        .pt(px(44.))
        .text_center()
        .child(glyph(Icon::GitBranch, theme.text_faint, theme.text_faint, 32.))
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(title),
        )
        .child(div().text_xs().text_color(theme.text_muted).child(body))
}

fn not_installed(theme: &Theme) -> Div {
    empty_state(
        "Git isn't installed",
        "Zuno syncs workspaces with your own git, so your keys and settings just work. Install \
         it, then come back.",
        theme,
    )
    .child(button("git-download", "git-scm.com/downloads", GitOpenDownload, false, theme))
    .child(button("git-check-again", "Check again", GitRefresh, false, theme))
}

fn not_a_repo(git: &GitState, theme: &Theme) -> Div {
    empty_state(
        "Not in git yet",
        "Keep this workspace in git to sync it between your machines, or with a team.",
        theme,
    )
    .child(button(
        "git-init",
        git.busy.unwrap_or("Initialize repository"),
        GitInit,
        true,
        theme,
    ))
    .child(
        div()
            .mt(px(6.))
            .text_size(px(10.5))
            .text_color(theme.text_faint)
            .child("*.local.json is ignored from the first commit, so your secrets stay on this machine."),
    )
    .children(git.error.clone().map(|error| error_line(error, theme)))
}

fn toolbar(git: &GitState, theme: &Theme) -> Div {
    let status = git.status.as_ref();
    let branch = status
        .and_then(|status| status.branch.clone())
        .unwrap_or_else(|| "detached".to_string());
    // The right end: Sync once there is a remote to sync with, a plain word otherwise.
    let right = match status {
        Some(status) if status.operation.is_some() => plain("Syncing paused", theme),
        Some(status) if !status.remote => plain("local only", theme),
        Some(status) => {
            let counts = if status.upstream {
                format!("↓{} ↑{}", status.behind, status.ahead)
            } else {
                "not pushed".to_string()
            };
            sync_button(git.busy.filter(|busy| *busy == "Syncing…"), counts, theme)
                .into_any_element()
        }
        None => plain("", theme),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .h(px(30.))
        .pl_2()
        .pr(px(8.))
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.))
                .min_w(px(0.))
                .overflow_hidden()
                .child(glyph(Icon::GitBranch, theme.text_muted, theme.text_muted, 12.))
                .child(
                    div()
                        .whitespace_nowrap()
                        .font_family(theme.mono.clone())
                        .text_size(px(11.))
                        .text_color(theme.text)
                        .child(branch),
                ),
        )
        .child(right)
}

fn plain(text: &'static str, theme: &Theme) -> gpui::AnyElement {
    div()
        .flex_none()
        .text_size(px(10.5))
        .text_color(theme.text_faint)
        .child(text)
        .into_any_element()
}

/// Sync, with how far apart the branch and its remote are as of the last fetch.
fn sync_button(busy: Option<&'static str>, counts: String, theme: &Theme) -> gpui::Stateful<Div> {
    div()
        .id("git-sync")
        .debug_selector(|| "git-sync".to_string())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .flex_none()
        .h(px(22.))
        .px(px(8.))
        .rounded(px(4.))
        .border_1()
        .border_color(theme.border_focused)
        .text_size(px(11.))
        .text_color(theme.text)
        .cursor_pointer()
        .hover(|style| style.bg(theme.bg_hover))
        .tooltip(|window, cx| {
            crate::ui::Tooltip::for_action("Fetch, rebase onto the remote, push", &GitSync, window, cx)
        })
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(GitSync), cx);
        })
        .child(glyph(Icon::RotateCw, theme.text, theme.text, 12.))
        .child(busy.unwrap_or("Sync"))
        .child(div().text_color(theme.text_faint).child(counts))
}

/// The first push: a remote's URL and Publish.
fn publish_card(git: &GitState, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(7.))
        .m_2()
        .p(px(10.))
        .rounded(px(5.))
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child("Publish to a remote"),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_muted)
                .child("Create an empty repository on GitHub, GitLab or anywhere, and paste its URL."),
        )
        .child(crate::ui::field_box(git.remote_url.clone(), theme))
        .child(button(
            "git-publish",
            git.busy.filter(|busy| *busy == "Publishing…").unwrap_or("Publish"),
            GitPublish,
            false,
            theme,
        ))
}

/// A sync stopped on a conflict: what happened, and the three ways on.
fn conflict_box(git: &GitState, theme: &Theme) -> Div {
    let red = theme.status_server_error;
    let conflicted: Vec<&FileChange> = git
        .status
        .as_ref()
        .map(|status| {
            status
                .files
                .iter()
                .filter(|file| file.kind == ChangeKind::Conflicted)
                .collect()
        })
        .unwrap_or_default();
    let name = |file: &FileChange| {
        let file = file.path.rsplit('/').next().unwrap_or(&file.path);
        file.strip_suffix(".json").unwrap_or(file).to_string()
    };
    let (title, body) = match conflicted.as_slice() {
        [] => (
            "Conflicts fixed".to_string(),
            "Continue to finish the sync and push.".to_string(),
        ),
        [one] => (
            "1 request changed on both sides".to_string(),
            format!("Your edit and a teammate's both touch {}. Fix it in your editor, then continue.", name(one)),
        ),
        many => (
            format!("{} requests changed on both sides", many.len()),
            "Fix them in your editor, then continue.".to_string(),
        ),
    };
    let mut actions = div().flex().flex_row().flex_wrap().gap(px(6.)).mt(px(2.));
    if !conflicted.is_empty() {
        actions = actions.child(button("git-open-conflict", "Open in editor", GitOpenConflict, false, theme));
    }
    actions = actions
        .child(button(
            "git-continue",
            git.busy.filter(|busy| *busy == "Continuing…").unwrap_or("Continue"),
            GitContinue,
            conflicted.is_empty(),
            theme,
        ))
        .child(button(
            "git-abort",
            git.busy.filter(|busy| *busy == "Undoing…").unwrap_or("Undo sync"),
            GitAbort,
            false,
            theme,
        ));
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(6.))
        .m_2()
        .p(px(10.))
        .rounded(px(5.))
        .bg(red.alpha(0.10))
        .border_1()
        .border_color(red.alpha(0.35))
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(red)
                .child(title),
        )
        .child(div().text_size(px(11.)).text_color(theme.text_muted).child(body))
        .child(actions)
        .children(git.error.clone().map(|error| error_line(error, theme)))
}

fn commit_box(git: &GitState, theme: &Theme) -> Div {
    let count = git.change_count();
    let label: SharedString = match (git.busy, count) {
        (Some(busy), _) => busy.into(),
        (None, 0) => "Nothing to commit".into(),
        (None, 1) => "Commit 1 change".into(),
        (None, n) => format!("Commit {n} changes").into(),
    };
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(6.))
        .p_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .debug_selector(|| "git-message".to_string())
                .h(px(52.))
                .px(px(6.))
                .py(px(4.))
                .rounded(px(4.))
                .bg(theme.bg)
                .border_1()
                .border_color(theme.border)
                .overflow_hidden()
                .text_xs()
                .child(git.message.clone()),
        )
        .child(button("git-commit", label, GitCommit, count > 0, theme))
        .children(git.error.clone().map(|error| error_line(error, theme)))
}

fn identity_box(form: &IdentityForm, git: &GitState, theme: &Theme) -> Div {
    let warn = theme.status_client_error;
    let label = |text: &'static str| div().text_size(px(10.5)).text_color(theme.text_muted).child(text);
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(7.))
        .m_2()
        .p(px(10.))
        .rounded(px(5.))
        .bg(warn.alpha(0.08))
        .border_1()
        .border_color(warn.alpha(0.35))
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(warn)
                .child("Git doesn't know who you are yet"),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_muted)
                .child("Every commit carries a name and an email. Set them once and commit again."),
        )
        .child(label("Name"))
        .child(crate::ui::field_box(form.name.clone(), theme))
        .child(label("Email"))
        .child(crate::ui::field_box(form.email.clone(), theme))
        .child(
            div()
                .id("git-identity-scope")
                .debug_selector(|| "git-identity-scope".to_string())
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .text_size(px(10.5))
                .text_color(theme.text)
                .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(GitToggleIdentityScope), cx);
                })
                .child(checkbox(form.everywhere, theme))
                .child("Use for every repository on this machine"),
        )
        .child(button("git-save-identity", git.busy.unwrap_or("Save and commit"), GitSaveIdentity, true, theme))
        .children(git.error.clone().map(|error| error_line(error, theme)))
}

fn checkbox(checked: bool, theme: &Theme) -> Div {
    let mut box_ = div()
        .flex_none()
        .size(px(12.))
        .rounded(px(3.))
        .border_1()
        .flex()
        .items_center()
        .justify_center();
    if checked {
        box_ = box_
            .bg(theme.accent)
            .border_color(theme.accent)
            .child(glyph(Icon::Check, theme.bg, theme.bg, 9.));
    } else {
        box_ = box_.border_color(theme.border_focused);
    }
    box_
}

fn error_line(error: String, theme: &Theme) -> Div {
    div()
        .text_size(px(10.5))
        .text_color(theme.status_server_error)
        .child(error)
}

fn footnote(text: String, theme: &Theme) -> Div {
    div()
        .flex_none()
        .mt_auto()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
        .text_size(px(10.5))
        .text_color(theme.text_faint)
        .child(text)
}

/// A button that dispatches `action`. `primary` fills it with the accent, for the one thing a
/// state is asking you to do.
fn button<A: gpui::Action + Clone + 'static>(
    id: &'static str,
    label: impl Into<SharedString>,
    action: A,
    primary: bool,
    theme: &Theme,
) -> gpui::Stateful<Div> {
    let base = div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.))
        .px(px(12.))
        .rounded(px(4.))
        .text_xs()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(action.boxed_clone(), cx);
        })
        .child(label.into());
    if primary {
        base.bg(theme.accent)
            .text_color(gpui::rgb(0xf4f4f5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .hover(|style| style.opacity(0.9))
    } else {
        base.border_1()
            .border_color(theme.border_focused)
            .text_color(theme.text)
            .hover(|style| style.bg(theme.bg_hover))
    }
}

/// One line of the Git view's list: a section's heading with its count, or a file in it.
#[derive(Clone)]
enum Line {
    Heading(&'static str, usize),
    /// A file, and whether its row offers Discard — only your own changes, outside a stopped sync.
    File(FileChange, bool),
}

/// The sections, in order. Mid-sync: what is conflicted, then what the remote brings — the local
/// changes git is replaying are its business until it stops. Otherwise: what you changed, then
/// what the remote has that you have not taken yet.
fn lines(status: &Status) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut section = |title: &'static str, files: Vec<FileChange>| {
        if !files.is_empty() {
            lines.push(Line::Heading(title, files.len()));
            let ours = title == "CHANGES";
            lines.extend(files.into_iter().map(|file| {
                let discardable = ours
                    && matches!(file.kind, ChangeKind::Added | ChangeKind::Modified | ChangeKind::Deleted);
                Line::File(file, discardable)
            }));
        }
    };
    if status.operation.is_some() {
        section(
            "CONFLICTS",
            status
                .files
                .iter()
                .filter(|file| file.kind == ChangeKind::Conflicted)
                .cloned()
                .collect(),
        );
    } else {
        section("CHANGES", status.files.clone());
    }
    section("INCOMING", status.incoming.clone());
    lines
}

/// The changed files, by section.
fn changes(workspace: &Workspace, theme: &Theme, cx: &mut Context<Workspace>) -> impl IntoElement + use<> {
    let lines: Vec<Line> = workspace.git.status.as_ref().map(lines).unwrap_or_default();
    let nothing = workspace
        .git
        .status
        .as_ref()
        .is_none_or(|status| status.files.is_empty() && status.operation.is_none());
    let root = crate::collections::root(cx).map(Path::to_path_buf);
    // A changed request's row wears the same pill or icon as its tree row.
    let badges: HashMap<PathBuf, Badge> = workspace
        .tree
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::Request { badge, .. } => Some((node.path.clone(), badge.clone())),
            NodeKind::Directory => None,
        })
        .collect();
    let entity = cx.entity();
    let row_theme = theme.clone();

    let empty = nothing.then(|| {
        div()
            .px_3()
            .pt_3()
            .text_xs()
            .text_color(theme.text_faint)
            .child("Everything in this workspace is committed.")
    });

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.))
        .children(empty)
        .child(
            uniform_list("git-changes", lines.len(), move |range, _window, _cx| {
                range
                    .map(|ix| match &lines[ix] {
                        Line::Heading(title, count) => section_heading(title, *count, &row_theme),
                        Line::File(change, discardable) => {
                            let path = root.as_ref().map(|root| root.join(&change.path));
                            let badge = path.as_ref().and_then(|path| badges.get(path)).cloned();
                            change_row(ix, change, *discardable, badge, path, &row_theme, entity.clone())
                        }
                    })
                    .collect()
            })
            .track_scroll(workspace.git.scroll.clone())
            .flex_1(),
        )
}

fn section_heading(title: &'static str, count: usize, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .items_end()
        .w_full()
        .h(px(26.))
        .pl_3()
        .pr(px(10.))
        .pb(px(5.))
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(title)
        .child(count.to_string())
}

fn change_row(
    ix: usize,
    change: &FileChange,
    discardable: bool,
    badge: Option<Badge>,
    path: Option<PathBuf>,
    theme: &Theme,
    workspace: Entity<Workspace>,
) -> Div {
    let colour = change_colour(change.kind, theme);
    // Shown on the row under the pointer only: a column of undo arrows on every row would read
    // as the list's main verb, and discarding is the one that cannot be taken back.
    let discard = discardable.then(|| {
        let change = change.clone();
        let workspace = workspace.clone();
        div()
            .debug_selector(move || format!("git-discard-{ix}"))
            .flex_none()
            .size(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .opacity(0.)
            .group_hover(GIT_ROW, |style| style.opacity(1.))
            .hover(|style| style.bg(theme.bg_elevated))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                // The row under it opens the file; this must not.
                cx.stop_propagation();
                workspace.update(cx, |workspace, cx| {
                    workspace.ask_discard(change.clone(), window, cx)
                });
            })
            .child(glyph(Icon::RotateCcw, theme.text_muted, theme.text, 12.))
    });
    let (folder, file) = match change.path.rsplit_once('/') {
        Some((folder, file)) => (folder.to_string(), file.to_string()),
        None => (String::new(), change.path.clone()),
    };
    let name = file.strip_suffix(".json").unwrap_or(&file).to_string();
    let gone = matches!(change.kind, ChangeKind::Deleted);

    let first = match badge {
        Some(badge) => crate::collection_panel::badge_cell(&badge, theme).mr(px(0.)),
        None => div()
            .flex_none()
            .w(px(crate::collection_panel::METHOD_WIDTH))
            .flex()
            .justify_center()
            .child(glyph(Icon::File, theme.text_faint, theme.text_faint, 13.)),
    };

    let mut name_cell = div().text_color(if gone { theme.text_faint } else { theme.text }).child(name);
    if gone {
        name_cell = name_cell.line_through();
    }

    div()
        .debug_selector(move || format!("git-change-{ix}"))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(26.))
        .pl(px(8.))
        .pr(px(10.))
        .gap(px(8.))
        .text_xs()
        .cursor_pointer()
        .group(GIT_ROW)
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let Some(path) = path.clone() else { return };
            workspace.update(cx, |workspace, cx| workspace.open_changed_file(path, window, cx));
        })
        .child(first)
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(6.))
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(name_cell)
                .child(div().text_size(px(10.5)).text_color(theme.text_faint).child(folder)),
        )
        .children(discard)
        .child(
            div()
                .flex_none()
                .font_family(theme.mono.clone())
                .text_size(px(10.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(colour)
                .child(change.kind.letter()),
        )
}

/// The group a change row's hover-only controls answer to.
const GIT_ROW: &str = "git-change-row";
