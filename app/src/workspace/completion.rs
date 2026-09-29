//! Completion in the GraphQL query editor: the list under the caret, and the keys that drive it.
//!
//! **The header-name dropdown's design, one surface over** (architecture.md §6l): owned by
//! `Workspace` so no `overflow_hidden` ancestor clips it, no scrim and no focus transfer so typing
//! carries on, and a list *derived* in render from the editor's text and caret rather than stored
//! beside them. Three things are new here:
//!
//! - **It opens by itself only while a name is being typed.** An empty prefix shows nothing unless
//!   `Ctrl+Space` asked at exactly this spot, so moving through a document never pops a list up.
//! - **`Tab` accepts the top row; `Enter` only one the arrows moved to** — §6l's rule that typing
//!   never makes `Enter` replace what you wrote. Every other `Enter` is a newline, as it was.
//! - **The schema is parsed off the UI thread** into `Workspace::graphql_schemas`, once per file,
//!   and a fresh *From server* drops the cached copy. The per-keystroke scan of the document is on
//!   the UI thread, like the header list's match: a query is a few hundred bytes.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, Entity, EntityId, IntoElement, SharedString, Window, px};

use zuno_core::graphql::complete::{self, SchemaIndex, Suggestion};

use super::Workspace;
use crate::actions::{
    CancelRequest, CompleteAccept, CompleteConfirm, CompleteDismiss, CompleteNext, CompletePrev,
    FocusNext, TriggerCompletion,
};
use crate::input::Editor;
use crate::theme::ActiveTheme as _;

/// Where the list stands, keyed so any edit or caret move makes a stale entry simply not match.
#[derive(Default)]
pub(super) struct CompletionState {
    /// The row the arrows moved to, in which editor. `None` is the safety rule: nothing typed is
    /// ever replaced by `Enter` unless someone chose a row.
    highlight: Option<(EntityId, usize)>,
    /// Where `escape` closed it — editor, caret, text length — so it stays shut until either moves.
    dismissed: Option<Spot>,
    /// Where `Ctrl+Space` asked, so an empty prefix still shows the list at that one spot.
    forced: Option<Spot>,
}

type Spot = (EntityId, usize, usize);

/// A schema file's state in the cache.
pub(super) enum SchemaSlot {
    Loading,
    Ready(Arc<SchemaIndex>),
    /// Unreadable or unparseable. Kept, so a broken file is not re-read on every frame; the next
    /// *From server* or a different file name replaces it.
    Failed,
}

/// An open list, as derived this frame.
struct Open {
    editor: Entity<Editor>,
    context: complete::Context,
    items: Vec<Suggestion>,
    highlighted: Option<usize>,
}

impl Workspace {
    /// The active request's query editor and schema file, when it is a GraphQL request with one.
    pub(super) fn graphql_target(&self, cx: &gpui::App) -> Option<(Entity<Editor>, PathBuf)> {
        let graphql = self.active()?.read(cx).kind.as_graphql()?;
        let schema = graphql.schema.read(cx).text().trim().to_string();
        if schema.is_empty() {
            return None;
        }
        let root = crate::collections::root(cx);
        Some((
            graphql.query.clone(),
            zuno_core::graphql::resolve_schema(&schema, root),
        ))
    }

    /// Start parsing the active request's schema if nothing has yet. Called from render; the
    /// read and the parse run on the background executor, and the frame after they finish shows
    /// the list.
    pub(super) fn load_graphql_schema(&mut self, cx: &mut Context<Self>) {
        let Some((_, path)) = self.graphql_target(cx) else {
            return;
        };
        if self.graphql_schemas.contains_key(&path) {
            return;
        }
        self.graphql_schemas.insert(path.clone(), SchemaSlot::Loading);
        let read = path.clone();
        cx.spawn(async move |this, cx| {
            let slot = cx
                .background_executor()
                .spawn(async move {
                    let sdl = std::fs::read_to_string(&read).ok()?;
                    SchemaIndex::parse(&sdl).ok()
                })
                .await
                .map_or(SchemaSlot::Failed, |index| SchemaSlot::Ready(Arc::new(index)));
            let _ = this.update(cx, |workspace, cx| {
                workspace.graphql_schemas.insert(path, slot);
                cx.notify();
            });
        })
        .detach();
    }

    /// Forget a schema file's parsed copy, so the next frame reads the file again.
    pub(super) fn forget_graphql_schema(&mut self, path: &std::path::Path) {
        self.graphql_schemas.remove(path);
    }

    /// The focused query editor, its spot, and the parsed schema — or `None` when no list could
    /// show at all.
    fn completion_target(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<(Entity<Editor>, Spot, Arc<SchemaIndex>)> {
        let (editor, path) = self.graphql_target(cx)?;
        let read = editor.read(cx);
        if !gpui::Focusable::focus_handle(read, cx).is_focused(window) || read.has_selection() {
            return None;
        }
        let spot = (editor.entity_id(), read.cursor_offset(), read.text().len());
        let SchemaSlot::Ready(index) = self.graphql_schemas.get(&path)? else {
            return None;
        };
        Some((editor, spot, index.clone()))
    }

    /// The list as it stands this frame, derived from the editor rather than stored beside it.
    fn completion(&self, window: &Window, cx: &gpui::App) -> Option<Open> {
        let (editor, spot, index) = self.completion_target(window, cx)?;
        if self.completion.dismissed == Some(spot) {
            return None;
        }
        let context = complete::context(editor.read(cx).text(), spot.1)?;
        // A `$` in a value is itself the request for a variable, so it opens the list with
        // nothing typed after it. Nowhere else does an empty name open it unasked.
        let asked = matches!(context.spot, complete::Spot::Variable { .. })
            || self.completion.forced == Some(spot);
        if context.prefix.is_empty() && !asked {
            return None;
        }
        let items = index.suggest(&context.spot, &context.prefix);
        // A finished name offers nothing — the header list's rule — or accepting a row would
        // leave the list open over the word it just wrote.
        if items.is_empty() || (items.len() == 1 && items[0].label == context.prefix) {
            return None;
        }
        let highlighted = match self.completion.highlight {
            Some((id, ix)) if id == spot.0 && ix < items.len() => Some(ix),
            _ => None,
        };
        Some(Open {
            editor,
            context,
            items,
            highlighted,
        })
    }

    /// What the list holds, for tests: labels, and which is highlighted.
    #[cfg(test)]
    pub fn completion_for_test(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<(Vec<String>, Option<usize>)> {
        self.completion(window, cx).map(|open| {
            (
                open.items.into_iter().map(|item| item.label).collect(),
                open.highlighted,
            )
        })
    }

    /// The list under the caret, anchored at the caret's painted position.
    pub(super) fn graphql_completion(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let open = self.completion(window, cx)?;
        let theme = cx.theme().clone();
        // Absent for the one frame before the editor has painted the caret, like the header list.
        let caret = open.editor.read(cx).caret_bounds()?;
        let id = open.editor.entity_id();

        let rows = open
            .items
            .into_iter()
            .map(|item| crate::ui::SelectRow {
                label: SharedString::from(item.label),
                detail: Some(SharedString::from(item.detail)),
                dimmed: item.deprecated,
            })
            .collect();

        Some(crate::ui::select_list(
            "graphql-completions",
            gpui::point(caret.left(), caret.bottom()),
            rows,
            open.highlighted,
            px(200.),
            &theme,
            cx,
            move |workspace, ix, cx| {
                workspace.completion.highlight = Some((id, ix));
                cx.notify();
            },
            |workspace, ix, window, cx| workspace.accept_completion(ix, window, cx),
        ))
    }

    /// Write row `ix` over the name at the caret, through the ordinary edit path so `Ctrl+Z`
    /// takes it back.
    fn accept_completion(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.completion(window, cx) else {
            return;
        };
        let Some(item) = open.items.get(ix) else {
            return;
        };
        let replace = open.context.replace.clone();
        let insert = item.insert.clone();
        let spot = open.editor.update(cx, |editor, cx| {
            editor.replace_range(replace, &insert, window, cx);
            (cx.entity_id(), editor.cursor_offset(), editor.text().len())
        });
        self.completion.highlight = None;
        self.completion.forced = None;
        // Closed over the word just written — `user` would otherwise keep offering `users` — and
        // open again as soon as anything is typed or the caret moves, as after `escape`.
        self.completion.dismissed = Some(spot);
        cx.notify();
    }

    pub(super) fn complete_next(&mut self, _: &CompleteNext, window: &mut Window, cx: &mut Context<Self>) {
        self.step_completion(1, Box::new(crate::input::editor::Down), window, cx);
    }

    pub(super) fn complete_prev(&mut self, _: &CompletePrev, window: &mut Window, cx: &mut Context<Self>) {
        self.step_completion(-1, Box::new(crate::input::editor::Up), window, cx);
    }

    /// Move the highlight, or — with no list open — do what the arrow did before, which is move
    /// the caret. These bindings win over the editor's own, so the fallback is the whole reason
    /// the arrows still work in a GraphQL document.
    fn step_completion(
        &mut self,
        delta: isize,
        otherwise: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.completion(window, cx) else {
            window.dispatch_action(otherwise, cx);
            return;
        };
        let count = open.items.len() as isize;
        let next = match open.highlighted {
            None if delta > 0 => 0,
            None => count - 1,
            Some(ix) => (ix as isize + delta).rem_euclid(count),
        };
        self.completion.highlight = Some((open.editor.entity_id(), next as usize));
        cx.notify();
    }

    /// `Tab` accepts the highlighted row, or the top one; with no list it moves focus as before.
    pub(super) fn complete_accept(&mut self, _: &CompleteAccept, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.completion(window, cx) else {
            self.focus_next(&FocusNext, window, cx);
            return;
        };
        self.accept_completion(open.highlighted.unwrap_or(0), window, cx);
    }

    /// `Enter` accepts only a row the arrows chose; otherwise it is a newline, as it always was.
    pub(super) fn complete_confirm(&mut self, _: &CompleteConfirm, window: &mut Window, cx: &mut Context<Self>) {
        match self.completion(window, cx).and_then(|open| open.highlighted) {
            Some(ix) => self.accept_completion(ix, window, cx),
            None => window.dispatch_action(Box::new(crate::input::editor::Newline), cx),
        }
    }

    /// `escape` closes the list — and with none open still cancels a request in flight, for the
    /// header list's reason: this binding wins whenever the query editor has focus.
    pub(super) fn complete_dismiss(&mut self, _: &CompleteDismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion(window, cx).is_some()
            && let Some((_, spot, _)) = self.completion_target(window, cx)
        {
            self.completion.dismissed = Some(spot);
            self.completion.highlight = None;
            cx.notify();
            return;
        }
        self.cancel_request(&CancelRequest, window, cx);
    }

    /// `Ctrl+Space`: show the list here even with nothing typed, or say why there is none.
    pub(super) fn trigger_completion(&mut self, _: &TriggerCompletion, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active() else { return };
        let has_schema = view
            .read(cx)
            .kind
            .as_graphql()
            .is_some_and(|graphql| !graphql.schema.read(cx).text().trim().is_empty());
        if !has_schema {
            self.set_status("No schema to complete from — use From server on the Query tab", cx);
            return;
        }
        let Some((_, spot, _)) = self.completion_target(window, cx) else {
            return;
        };
        self.completion.forced = Some(spot);
        self.completion.dismissed = None;
        self.completion.highlight = None;
        cx.notify();
    }
}
