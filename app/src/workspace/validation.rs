//! Validating the GraphQL query against its saved schema as it is typed.
//!
//! **Marks, never gates.** Problems are underlined in the query and counted in the Query tab's
//! header, and Send sends regardless: the saved schema can be older than the server, and the
//! server is what decides.
//!
//! **Scheduled from render, at most once per distinct text**, after a pause in typing, and run on
//! the background executor — `apollo-compiler`'s validation walks the whole document against the
//! whole schema, which is invariant 3's territory. A result is kept only if the query still reads
//! exactly as it did when checked; otherwise a newer run is already on its way.

use std::time::Duration;

use gpui::{Context, EntityId};

use super::Workspace;
use super::completion::SchemaSlot;

/// How long typing has to pause before the query is checked. Long enough that a word in
/// progress is not underlined letter by letter; short enough to feel immediate once you stop.
const PAUSE: Duration = Duration::from_millis(300);

impl Workspace {
    pub(super) fn schedule_graphql_validation(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.active() else {
            return;
        };
        let Some(graphql) = view.read(cx).kind.as_graphql() else {
            return;
        };
        let editor = graphql.query.clone();
        let id = editor.entity_id();
        let current = editor.read(cx).text().to_string();
        let stale = graphql.problems(cx).is_none() && graphql.validated.is_some();

        // Underlines computed for different text point at the wrong characters: gone at once,
        // rather than sliding under whatever is typed next until the new result lands.
        if stale {
            editor.update(cx, |editor, cx| editor.set_problems(Vec::new(), cx));
        }

        let index = self
            .graphql_target(cx)
            .and_then(|(_, path)| match self.graphql_schemas.get(&path) {
                Some(SchemaSlot::Ready(index)) => Some(index.clone()),
                _ => None,
            });
        let Some(index) = index else {
            // No schema to check against, or not loaded yet — which render will retry.
            self.validating = None;
            return;
        };
        if self.validating.as_ref() == Some(&(id, current.clone())) {
            return;
        }
        self.validating = Some((id, current.clone()));

        let target = view.downgrade();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE).await;
            let still_wanted = this
                .update(cx, |workspace, _| still_validating(workspace, id, &current))
                .unwrap_or(false);
            if !still_wanted {
                return;
            }
            let (checked, problems) = cx
                .background_executor()
                .spawn(async move {
                    let problems = index.validate(&current);
                    (current, problems)
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(graphql) = view.kind.as_graphql_mut() else {
                    return;
                };
                if graphql.query.read(cx).text() != checked {
                    return;
                }
                let ranges = problems.iter().map(|problem| problem.range.clone()).collect();
                graphql.query.update(cx, |editor, cx| editor.set_problems(ranges, cx));
                graphql.validated = Some((checked, problems));
                cx.notify();
            });
        })
        .detach();
    }
}

fn still_validating(workspace: &Workspace, id: EntityId, text: &str) -> bool {
    workspace
        .validating
        .as_ref()
        .is_some_and(|(validating, validated)| *validating == id && validated == text)
}
