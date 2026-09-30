//! Authoring a GraphQL request: a document, its variables, and which operation to run.
//!
//! **Fields are unprefixed because the struct names them.** They were `graphql_query`,
//! `graphql_variables` and `graphql_operation` while they sat loose on `RequestView`, and that
//! prefix was doing a job a type should do — which is most of the argument for this module
//! existing at all.

use gpui::{App, AppContext, Context, Entity, Focusable, Window};
use zuno_core::{GraphQlRequest, GraphQlTransport, Method};

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
    /// How the operation reaches the server.
    ///
    /// **Plain data, not an editor entity**, for `WebSocketEditor::messages`' reason: nothing
    /// types into it, it is picked — so a live input would be state for something only ever
    /// read.
    pub transport: GraphQlTransport,
    /// The schema file: a bare name inside the collection's `schemas/`, or a path anywhere —
    /// `GrpcEditor::proto`'s field, for the same reasons.
    pub schema: Entity<TextInput>,
    /// The query's problems against the schema, with the exact text they were found in. Not part
    /// of the request — derived, and shown only while the query still *is* that text, since a
    /// range into text that has since changed points at the wrong characters.
    pub validated: Option<(String, Vec<zuno_core::graphql::complete::Problem>)>,
    /// The Variables JSON's problems, with the three texts they depend on — the query (which
    /// declares the variables), the JSON, and the operation name — shown only while all three
    /// still read that way.
    pub variables_validated: Option<(VariablesKey, Vec<zuno_core::graphql::complete::Problem>)>,
    /// The parsed schema this request names, handed down by `Workspace` once it has loaded —
    /// the request pane cannot reach `Workspace`'s cache itself. `None` without one.
    pub index: Option<std::sync::Arc<zuno_core::graphql::complete::SchemaIndex>>,
    /// The schema browser under the query, when open. Not part of the request and not saved:
    /// it is a way of looking, for as long as this tab is open.
    pub browser: Option<crate::schema_browser::SchemaBrowser>,
}

/// What a variables check was computed against: the query, the variables JSON, the operation.
pub type VariablesKey = (String, String, String);

impl GraphQlEditor {
    pub fn new(cx: &mut Context<RequestView>) -> Self {
        Self::from_spec(&GraphQlRequest::default(), cx)
    }

    pub fn from_spec(graphql: &GraphQlRequest, cx: &mut Context<RequestView>) -> Self {
        Self {
            method: graphql.method.clone(),
            // `GraphQlQuery` scopes the completion keys to this editor alone — see
            // `Workspace::graphql_completion`.
            query: cx.new(|cx| {
                let mut editor = Editor::new(&graphql.query, "query { … }", cx);
                editor.set_key_context("TextInput BodyEditor GraphQlQuery");
                editor
            }),
            variables: cx.new(|cx| Editor::new(&graphql.variables, "{ }", cx)),
            operation: cx.new(|cx| {
                TextInput::new(
                    graphql.operation.clone().unwrap_or_default(),
                    "operation name",
                    "GraphQlOperation",
                    cx,
                )
            }),
            transport: graphql.transport,
            schema: cx.new(|cx| {
                TextInput::new(graphql.schema.clone(), "api.graphql", "GraphQlSchema", cx)
            }),
            validated: None,
            variables_validated: None,
            index: None,
            browser: None,
        }
    }

    /// Take the schema `Workspace` loaded, passing it on to an open browser. Returns whether
    /// anything changed, so the caller notifies only then.
    pub fn set_index(
        &mut self,
        index: Option<std::sync::Arc<zuno_core::graphql::complete::SchemaIndex>>,
    ) -> bool {
        let same = match (&self.index, &index) {
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return false;
        }
        if let Some(browser) = self.browser.as_mut() {
            browser.set_index(index.clone());
        }
        self.index = index;
        true
    }

    /// Open the browser under the query, or close it.
    pub fn toggle_browser(&mut self, cx: &mut Context<RequestView>) {
        self.browser = match self.browser.take() {
            Some(_) => None,
            None => Some(crate::schema_browser::SchemaBrowser::new(self.index.clone(), cx)),
        };
    }

    /// The texts a variables check depends on, as they read now.
    pub fn variables_key(&self, cx: &App) -> VariablesKey {
        (
            self.query.read(cx).text().to_string(),
            self.variables.read(cx).text().to_string(),
            self.operation.read(cx).text().trim().to_string(),
        )
    }

    /// The variables' problems as things stand, or `None` when any of the three texts has changed
    /// since the last check.
    pub fn variable_problems(&self, cx: &App) -> Option<&[zuno_core::graphql::complete::Problem]> {
        let (key, problems) = self.variables_validated.as_ref()?;
        (*key == self.variables_key(cx)).then_some(problems.as_slice())
    }

    /// The problems in the query as it stands, or `None` when it has changed since the last
    /// validation (or there has been none).
    pub fn problems(&self, cx: &App) -> Option<&[zuno_core::graphql::complete::Problem]> {
        let (text, problems) = self.validated.as_ref()?;
        (self.query.read(cx).text() == text).then_some(problems.as_slice())
    }

    /// Whether anything has been typed here — see `KindEditor::has_content`.
    pub fn has_content(&self, cx: &App) -> bool {
        // The operation name counts: it is typed, it is lost on a kind switch, and leaving it
        // out meant a request with only a name filled in was discarded without being asked.
        !self.query.read(cx).text().trim().is_empty()
            || !self.variables.read(cx).text().trim().is_empty()
            || !self.operation.read(cx).text().trim().is_empty()
            // A transport that was chosen rather than defaulted is a decision, and a kind
            // switch would throw it away as surely as it throws away the document.
            || self.transport != GraphQlTransport::default()
            || !self.schema.read(cx).text().trim().is_empty()
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
            transport: self.transport,
            schema: self.schema.read(cx).text().trim().to_string(),
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
            transport,
            schema,
        } = base;

        let typed = self.operation.read(cx).text().trim().to_string();
        let typed = (!typed.is_empty()).then_some(typed);

        self.method != *method
            || self.query.read(cx).text() != query
            || self.variables.read(cx).text() != variables
            || typed.as_deref() != operation.as_deref()
            || self.transport != *transport
            || self.schema.read(cx).text().trim() != schema
    }

    /// Whether this will open a socket, as the engine will decide it.
    ///
    /// Through `GraphQlTransport::opens_a_websocket` rather than reimplemented, so the label on
    /// screen and the route the request actually takes cannot disagree — which they would the
    /// first time someone adjusted one of them.
    pub fn uses_websocket(&self, cx: &App) -> bool {
        // Asked of the text in place: going through `to_spec` cloned the whole document on every
        // repaint of the Query header.
        let operation = self.operation.read(cx).text().trim();
        self.transport.opens_a_websocket(
            self.query.read(cx).text(),
            (!operation.is_empty()).then_some(operation),
        )
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
            || self.schema.read(cx).focus_handle(cx).is_focused(window)
    }
}
