//! Authoring a GraphQL request: a document, its variables, and which operation to run.
//!
//! **Fields are unprefixed because the struct names them.** They were `graphql_query`,
//! `graphql_variables` and `graphql_operation` while they sat loose on `RequestView`, and that
//! prefix was doing a job a type should do — which is most of the argument for this module
//! existing at all.

use gpui::{App, AppContext, Context, Entity, Focusable, Window};
use zuno_core::{GraphQlRequest, Method};

use crate::input::{Editor, TextInput};
use crate::request_view::RequestView;

/// Everything a GraphQL request is edited through.
pub struct GraphQlEditor {
    /// POST normally; GET when the query should be cacheable by ordinary HTTP machinery.
    /// Shared with HTTP because it means the same thing — see `kinds::mod`'s note on naming.
    pub method: Method,
    /// The document. An `Editor` rather than a `TextInput` because it is multi-line, and it
    /// wants the same find, undo and horizontal scrolling every other body surface has.
    pub query: Entity<Editor>,
    /// The variables, as JSON *text*. A second editor rather than a key/value table: they are
    /// a JSON object whose values nest arbitrarily, which a two-column table cannot express.
    pub variables: Entity<Editor>,
    /// `operationName` — only meaningful when the document holds more than one named
    /// operation, which is why it is a single line beside the editors rather than a tab.
    pub operation: Entity<TextInput>,
}

impl GraphQlEditor {
    pub fn new(cx: &mut Context<RequestView>) -> Self {
        Self::from_spec(&GraphQlRequest::default(), cx)
    }

    pub fn from_spec(graphql: &GraphQlRequest, cx: &mut Context<RequestView>) -> Self {
        Self {
            method: graphql.method.clone(),
            query: cx.new(|cx| Editor::new(&graphql.query, "query { … }", cx)),
            variables: cx.new(|cx| Editor::new(&graphql.variables, "{ }", cx)),
            operation: cx.new(|cx| {
                TextInput::new(
                    graphql.operation.clone().unwrap_or_default(),
                    "operation name",
                    "GraphQlOperation",
                    cx,
                )
            }),
        }
    }

    /// Whether anything has been typed here — see `KindEditor::has_content`.
    pub fn has_content(&self, cx: &App) -> bool {
        // The operation name counts: it is typed, it is lost on a kind switch, and leaving it
        // out meant a request with only a name filled in was discarded without being asked.
        !self.query.read(cx).text().trim().is_empty()
            || !self.variables.read(cx).text().trim().is_empty()
            || !self.operation.read(cx).text().trim().is_empty()
    }

    /// What `spec()` reads back out. The mirror of `from_spec`, and the reason a GraphQL
    /// request survives a load/save round trip.
    pub fn to_spec(&self, cx: &App) -> GraphQlRequest {
        let operation = self.operation.read(cx).text().trim().to_string();
        GraphQlRequest {
            method: self.method.clone(),
            query: self.query.read(cx).text().to_string(),
            variables: self.variables.read(cx).text().to_string(),
            // Blank means "this document has one operation, work it out" — an empty string
            // would be sent as `operationName: ""`, which several servers reject.
            operation: (!operation.is_empty()).then_some(operation),
        }
    }

    /// Whether anything here differs from the request as it was loaded.
    ///
    /// Destructured with no `..`, for the reason `RequestView::is_dirty` is: a field added to
    /// `GraphQlRequest` must fail to compile here until someone decides whether editing it
    /// makes a buffer dirty.
    pub fn is_dirty(&self, base: &GraphQlRequest, cx: &App) -> bool {
        let GraphQlRequest {
            method,
            query,
            variables,
            operation,
        } = base;

        let typed = self.operation.read(cx).text().trim().to_string();
        let typed = (!typed.is_empty()).then_some(typed);

        self.method != *method
            || self.query.read(cx).text() != query
            || self.variables.read(cx).text() != variables
            || typed.as_deref() != operation.as_deref()
    }

    /// The document editor — this kind's main text surface, which is what `Ctrl+F` and the
    /// formatter act on. HTTP answers with its body editor; see `KindEditor::primary_editor`.
    pub fn primary_editor(&self) -> &Entity<Editor> {
        &self.query
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.query.read(cx).focus_handle(cx).is_focused(window)
            || self.variables.read(cx).focus_handle(cx).is_focused(window)
            || self.operation.read(cx).focus_handle(cx).is_focused(window)
    }
}
