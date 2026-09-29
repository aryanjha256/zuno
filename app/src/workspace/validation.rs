//! Validating a GraphQL request against its saved schema as it is typed: the query, and the
//! Variables JSON against what the query declares.
//!
//! **Marks, never gates.** Problems are underlined where they are and counted in each tab's
//! header, and Send sends regardless: the saved schema can be older than the server, and the
//! server is what decides.
//!
//! **Scheduled from render, at most once per distinct input**, after a pause in typing, and run on
//! the background executor — `apollo-compiler`'s validation walks the whole document against the
//! whole schema, which is invariant 3's territory. The input is the query, the variables and the
//! operation name together, because the variables' problems depend on all three. A result is kept
//! only if they still read exactly as they did when checked; otherwise a newer run is on its way.

use std::time::Duration;

use gpui::{Context, EntityId};

use super::Workspace;
use super::completion::SchemaSlot;
use crate::kinds::graphql::VariablesKey;

/// How long typing has to pause before the request is checked. Long enough that a word in
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
        let (query, variables) = (graphql.query.clone(), graphql.variables.clone());
        let id = query.entity_id();
        let key = graphql.variables_key(cx);
        let stale_query = graphql.problems(cx).is_none() && graphql.validated.is_some();
        let stale_variables =
            graphql.variable_problems(cx).is_none() && graphql.variables_validated.is_some();

        // Underlines computed for different text point at the wrong characters: gone at once,
        // rather than sliding under whatever is typed next until the new result lands.
        if stale_query {
            query.update(cx, |editor, cx| editor.set_problems(Vec::new(), cx));
        }
        if stale_variables {
            variables.update(cx, |editor, cx| editor.set_problems(Vec::new(), cx));
        }

        let index = self
            .graphql_target(cx)
            .and_then(|(_, path)| match self.graphql_schemas.get(&path) {
                Some(SchemaSlot::Ready(index)) => Some(index.clone()),
                _ => None,
            });
        // The request pane cannot reach this cache, so the schema is handed down to the editor —
        // which is what the browser reads. Only on a change, so render does not notify itself.
        let handed = index.clone();
        view.update(cx, |view, cx| {
            if let Some(graphql) = view.kind.as_graphql_mut()
                && graphql.set_index(handed)
            {
                cx.notify();
            }
        });
        let Some(index) = index else {
            // No schema to check against, or not loaded yet — which render will retry.
            self.validating = None;
            return;
        };
        if self.validating.as_ref() == Some(&(id, key.clone())) {
            return;
        }
        self.validating = Some((id, key.clone()));

        let target = view.downgrade();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE).await;
            let still_wanted = this
                .update(cx, |workspace, _| still_validating(workspace, id, &key))
                .unwrap_or(false);
            if !still_wanted {
                return;
            }
            let (checked, query_problems, variable_problems) = cx
                .background_executor()
                .spawn(async move {
                    let (document, json, operation) = &key;
                    let query_problems = index.validate(document);
                    let variable_problems =
                        index.check_variables(document, Some(operation.as_str()), json);
                    (key, query_problems, variable_problems)
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(graphql) = view.kind.as_graphql_mut() else {
                    return;
                };
                if graphql.variables_key(cx) != checked {
                    return;
                }
                let ranges = |problems: &[zuno_core::graphql::complete::Problem]| {
                    problems.iter().map(|problem| problem.range.clone()).collect()
                };
                let query_ranges = ranges(&query_problems);
                let variable_ranges = ranges(&variable_problems);
                graphql.query.update(cx, |editor, cx| editor.set_problems(query_ranges, cx));
                graphql.variables.update(cx, |editor, cx| editor.set_problems(variable_ranges, cx));
                graphql.validated = Some((checked.0.clone(), query_problems));
                graphql.variables_validated = Some((checked, variable_problems));
                cx.notify();
            });
        })
        .detach();
    }
}

fn still_validating(workspace: &Workspace, id: EntityId, key: &VariablesKey) -> bool {
    workspace
        .validating
        .as_ref()
        .is_some_and(|(validating, validated)| *validating == id && validated == key)
}
