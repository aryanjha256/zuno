//! Authoring an HTTP request: a verb, query rows, and one of five body shapes.
//!
//! Everything here used to sit loose on `RequestView` — nine fields and their methods, mixed in
//! among the response viewer, the history and the search state. Moving it is what makes a third
//! kind a new file rather than four more fields on a struct that already had 47.

use std::path::PathBuf;

use gpui::{App, AppContext, Context, Entity, Focusable, SharedString, Window};
use zuno_core::{
    Body, FormField, HttpRequest, Method, MultipartField, MultipartValue, QueryParam, RawKind,
};

use crate::input::Editor;
use crate::request_view::{BodyType, KeyValueRow, MultipartRow, RequestView, rows_match};

/// Everything an HTTP request is edited through.
pub struct HttpEditor {
    pub method: Method,
    pub query: Vec<KeyValueRow>,
    /// The editor owns the body text, exactly as the inputs own theirs — `to_spec` reads
    /// through to it rather than mirroring into a field.
    pub body_editor: Entity<Editor>,
    /// Which body this request sends. `Raw` is further qualified by `body_kind`.
    ///
    /// Stored rather than inferred: a `Form` body with no fields yet and an `Empty` body are
    /// different intentions that look identical in the data, so nothing else records the choice.
    pub body_type: BodyType,
    /// Private, and set only through `set_body_kind`. The editor's colouring is derived from it,
    /// and a public field is how those two drift: assigning it directly compiles and silently
    /// leaves JSON text painted as plain, or XML painted as JSON.
    body_kind: RawKind,
    /// Fields of a form body. Same widget as the header and query tables, since `FormField` has
    /// the same shape as `Header`.
    pub form: Vec<KeyValueRow>,
    /// Parts of a multipart body.
    pub multipart: Vec<MultipartRow>,
    /// **Only the path is held, never the bytes.** `build.rs` reads the file at the send
    /// boundary, so a file edited between sends goes out in its new state and a 2GB upload never
    /// enters this process's memory.
    pub binary_path: Option<PathBuf>,
}

impl HttpEditor {
    pub fn new(cx: &mut Context<RequestView>) -> Self {
        Self::from_spec(&HttpRequest::default(), cx)
    }

    /// **Exhaustive on `Body` with no catch-all.** Every variant has an editor, so adding one is
    /// a compile error until someone decides how to edit it — which is stronger than the
    /// catch-all this replaced, and the catch-all was itself a fix for `load` silently
    /// *dropping* non-raw bodies that the next save then wrote over.
    pub fn from_spec(http: &HttpRequest, cx: &mut Context<RequestView>) -> Self {
        let query = http
            .query
            .iter()
            .map(|param| KeyValueRow::new(param.enabled, &param.name, &param.value, "QueryCell", cx))
            .collect();

        let (body_text, body_kind, body_type) = match &http.body {
            Body::Raw { text, kind } => (text.clone(), *kind, BodyType::Raw),
            // Empty stays editable: typing into it is how you get a raw body.
            Body::Empty => (String::new(), RawKind::Json, BodyType::Empty),
            Body::Form(_) => (String::new(), RawKind::Json, BodyType::Form),
            Body::Binary(_) => (String::new(), RawKind::Json, BodyType::Binary),
            Body::Multipart(_) => (String::new(), RawKind::Json, BodyType::Multipart),
        };

        let binary_path = match &http.body {
            Body::Binary(path) => Some(path.clone()),
            _ => None,
        };

        let multipart = match &http.body {
            Body::Multipart(fields) => fields
                .iter()
                .map(|field| {
                    let (text, is_file) = match &field.value {
                        MultipartValue::Text(text) => (text.clone(), false),
                        MultipartValue::File(path) => (path.display().to_string(), true),
                    };
                    MultipartRow {
                        row: KeyValueRow::new(field.enabled, &field.name, &text, "PartCell", cx),
                        is_file,
                    }
                })
                .collect(),
            _ => Vec::new(),
        };

        let form = match &http.body {
            Body::Form(fields) => fields
                .iter()
                .map(|field| {
                    KeyValueRow::new(field.enabled, &field.name, &field.value, "FormCell", cx)
                })
                .collect(),
            _ => Vec::new(),
        };

        let body_editor = cx.new(|cx| Editor::new(body_text, "Request body…", cx));
        body_editor.update(cx, |editor, cx| {
            editor.set_highlight_json(matches!(body_kind, RawKind::Json), cx)
        });

        Self {
            method: http.method.clone(),
            query,
            body_editor,
            body_type,
            body_kind,
            form,
            multipart,
            binary_path,
        }
    }

    /// Whether anything has been typed here — see `KindEditor::has_content`.
    ///
    /// Query rows count: they are part of the request and are lost with the rest.
    pub fn has_content(&self, cx: &App) -> bool {
        !matches!(self.body(cx), Body::Empty)
            || !self.body_editor.read(cx).text().trim().is_empty()
            || self.query.iter().any(|row| {
                !row.name.read(cx).text().trim().is_empty()
                    || !row.value.read(cx).text().trim().is_empty()
            })
    }

    pub fn to_spec(&self, cx: &App) -> HttpRequest {
        HttpRequest {
            method: self.method.clone(),
            query: self
                .query
                .iter()
                .map(|row| QueryParam {
                    enabled: row.enabled,
                    name: row.name.read(cx).text().to_string(),
                    value: row.value.read(cx).text().to_string(),
                })
                .collect(),
            body: self.body(cx),
        }
    }

    /// A blank editor means no body at all, not an empty raw one — sending
    /// `Content-Type: application/json` with zero bytes confuses servers.
    pub fn body(&self, cx: &App) -> Body {
        match self.body_type {
            BodyType::Form => Body::Form(
                self.form
                    .iter()
                    .map(|row| FormField {
                        enabled: row.enabled,
                        name: row.name.read(cx).text().to_string(),
                        value: row.value.read(cx).text().to_string(),
                    })
                    .collect(),
            ),
            BodyType::Multipart => Body::Multipart(
                self.multipart
                    .iter()
                    .map(|part| {
                        let text = part.row.value.read(cx).text().to_string();
                        MultipartField {
                            enabled: part.row.enabled,
                            name: part.row.name.read(cx).text().to_string(),
                            value: if part.is_file {
                                MultipartValue::File(PathBuf::from(text))
                            } else {
                                MultipartValue::Text(text)
                            },
                        }
                    })
                    .collect(),
            ),
            // No file chosen yet is `Empty`, not a broken `Binary("")` — the request is
            // incomplete, not malformed, and sending nothing is the honest reading.
            BodyType::Binary => match &self.binary_path {
                Some(path) => Body::Binary(path.clone()),
                None => Body::Empty,
            },
            // Unconditional: "None" means no body even though the editor may still hold text.
            // Falling through to the editor here meant picking None sent the previous body
            // anyway — the setting looked applied and wasn't.
            BodyType::Empty => Body::Empty,
            BodyType::Raw => {
                let text = self.body_editor.read(cx).text();
                // An empty raw body is `Empty`, not `Raw("")`: it keeps a blank editor from
                // sending a Content-Type for content that isn't there.
                if text.trim().is_empty() {
                    Body::Empty
                } else {
                    Body::Raw {
                        text: text.to_string(),
                        kind: self.body_kind,
                    }
                }
            }
        }
    }

    /// Destructured with no `..` for `GraphQlEditor::is_dirty`'s reason: a field added to
    /// `HttpRequest` must fail to compile here until someone decides whether editing it makes a
    /// buffer dirty.
    pub fn is_dirty(&self, base: &HttpRequest, cx: &App) -> bool {
        let HttpRequest {
            method,
            query,
            body,
        } = base;

        self.method != *method
            || !rows_match(&self.query, query, |p| (p.enabled, &p.name, &p.value), cx)
            || !self.body_matches(body, cx)
    }

    /// `body()`'s mapping asked as a question instead of built as a value.
    ///
    /// **Must stay in step with `body()` directly above**, including its two collapses to
    /// `Empty` — a blank editor and a binary body with no file chosen. Mirroring by hand is what
    /// keeps `is_dirty` allocation-free; `a_freshly_loaded_request_is_clean` covers every variant
    /// so the mirror cannot drift silently.
    fn body_matches(&self, base: &Body, cx: &App) -> bool {
        match self.body_type {
            BodyType::Empty => matches!(base, Body::Empty),
            BodyType::Raw => {
                let text = self.body_editor.read(cx).text();
                match base {
                    Body::Empty => text.trim().is_empty(),
                    Body::Raw { text: base, kind } => {
                        !text.trim().is_empty() && base == text && *kind == self.body_kind
                    }
                    _ => false,
                }
            }
            BodyType::Binary => match (&self.binary_path, base) {
                (Some(path), Body::Binary(base)) => path == base,
                (None, Body::Empty) => true,
                _ => false,
            },
            BodyType::Form => match base {
                Body::Form(base) => {
                    rows_match(&self.form, base, |f| (f.enabled, &f.name, &f.value), cx)
                }
                _ => false,
            },
            BodyType::Multipart => match base {
                Body::Multipart(base) => {
                    self.multipart.len() == base.len()
                        && self.multipart.iter().zip(base).all(|(row, part)| {
                            let text = row.row.value.read(cx).text();
                            row.row.enabled == part.enabled
                                && row.row.name.read(cx).text() == part.name
                                && match (&part.value, row.is_file) {
                                    // `body()` builds this with `PathBuf::from(text)`, so the
                                    // round trip back to a string is the comparison.
                                    (MultipartValue::File(path), true) => {
                                        path.display().to_string() == text
                                    }
                                    (MultipartValue::Text(base), false) => base == text,
                                    _ => false,
                                }
                        })
                }
                _ => false,
            },
        }
    }

    /// The body editor, but **only when a raw body is what's showing**.
    ///
    /// `None` for a form, a file or no body at all: there is no text on screen to search, and a
    /// find bar over one of those would be a control that does nothing.
    pub fn primary_editor(&self) -> Option<&Entity<Editor>> {
        matches!(self.body_type, BodyType::Raw | BodyType::Empty).then_some(&self.body_editor)
    }

    /// Choose the body type.
    ///
    /// Nothing is discarded: the editor's text, the form rows, the multipart parts and the
    /// binary path all stay put, so switching JSON → Form → JSON round-trips and only what gets
    /// *sent* changes. A mistaken type change is therefore never destructive.
    pub fn set_body_type(&mut self, body_type: BodyType) {
        self.body_type = body_type;
    }

    /// Test-only, like `Workspace::tab_count`: nothing in the UI reads the kind directly, it
    /// reads the label derived from it.
    pub fn body_kind(&self) -> RawKind {
        self.body_kind
    }

    /// Set the raw body's flavour, and the editor's colouring with it.
    ///
    /// One funnel, for the reason `Workspace::activate` is one: the two live in different
    /// entities, so keeping them in step at each call site is a rule to remember rather than a
    /// thing that cannot be got wrong.
    pub fn set_body_kind(&mut self, kind: RawKind, cx: &mut Context<RequestView>) {
        self.body_kind = kind;
        let json = matches!(kind, RawKind::Json);
        self.body_editor
            .update(cx, |editor, cx| editor.set_highlight_json(json, cx));
    }

    /// **`Empty` reports "None", not the retained raw sub-kind.** Folding the two together meant
    /// a body-less request advertised "JSON" on the pane's chip while the pane beside it read
    /// "No body" — and since the picker marks its current row by comparing this string against
    /// the row labels, it marked *JSON* as current on every fresh buffer and could never mark
    /// None. The string has to stay equal to the picker's own "None" label.
    pub fn body_label(&self) -> SharedString {
        match self.body_type {
            BodyType::Empty => SharedString::from("None"),
            BodyType::Form => SharedString::from("Form"),
            BodyType::Binary => SharedString::from("Binary"),
            BodyType::Multipart => SharedString::from("Multipart"),
            BodyType::Raw => SharedString::from(self.body_kind.label()),
        }
    }

    /// Point a binary body at a file, switching the body type to match.
    pub fn set_binary_path(&mut self, path: PathBuf) {
        self.binary_path = Some(path);
        self.set_body_type(BodyType::Binary);
    }

    pub fn multipart_is_file(&self, ix: usize) -> bool {
        self.multipart.get(ix).is_some_and(|part| part.is_file)
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.body_editor.read(cx).focus_handle(cx).is_focused(window)
            || self.query.iter().any(|row| row.is_focused(window, cx))
            || self.form.iter().any(|row| row.is_focused(window, cx))
            || self.multipart.iter().any(|part| part.row.is_focused(window, cx))
    }
}
