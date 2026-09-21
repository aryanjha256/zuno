//! Per-kind authoring state — the app-side mirror of `zuno_core::RequestKind`.
//!
//! **One kind, one module, one variant.** Adding gRPC or MQTT should be a new file plus a new
//! arm here, with the compiler naming every site that has to respond — and *no other file
//! growing a field*. That last clause is the whole point: before this existed, `RequestView`
//! carried twelve loose fields (`body_editor`, `form`, `multipart`, `binary_path`,
//! `graphql_query`, …) with a flat `ViewKind` tag beside them, so a third kind meant four more
//! fields on a struct that already had 47 and a new branch at every site that read them.
//!
//! ## Naming
//!
//! **Type names are global, so they carry the kind: `HttpEditor`, `GraphQlEditor`. Field names
//! are namespaced by their struct, so they don't** — `GraphQlEditor::query`, not
//! `graphql_query`. The old prefixes were a struct's job done by hand.
//!
//! `Method` is deliberately *shared and unprefixed*, because HTTP and GraphQL mean the same
//! thing by it: the HTTP verb. The rule is **prefix when it is the same concept with different
//! values; rename when it is a different concept wearing the same word.** gRPC's "method" is
//! `GetUser` on `UserService` — an address, not a verb — so it must not become a `GrpcMethod`
//! shadowing this one; it belongs as `service`/`method` fields inside a `GrpcEditor`, where the
//! struct names them.

use gpui::{App, Context, Entity, SharedString, Window};
use zuno_core::{Method, RequestKind};

use crate::input::Editor;
use crate::request_view::RequestView;

pub mod graphql;
pub mod http;
pub mod websocket;

pub use graphql::GraphQlEditor;
pub use http::HttpEditor;
pub use websocket::WebSocketEditor;

/// One tab a kind contributes to the request pane's strip.
///
/// A plain label rather than an enum variant per tab: the strip is built by *asking the kind*,
/// so a new kind's tabs need no case added anywhere central. `RequestTab::Kind(i)` indexes
/// into the slice this comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindTab {
    pub label: &'static str,
}

impl KindTab {
    const fn new(label: &'static str) -> Self {
        Self { label }
    }
}

/// Which kind a request is, as a value the picker can carry.
///
/// **A token, not state.** `KindEditor` is the stored answer — holding the `GraphQl` variant
/// *is* the record, which is why `ViewKind` could be deleted. This exists only because a picker
/// hands back a plain value, and a `KindEditor` owns live editor entities that cannot be built
/// until the choice is acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindChoice {
    Http,
    GraphQl,
    WebSocket,
}

impl KindChoice {
    /// What the kind picker offers.
    pub const ALL: [KindChoice; 3] =
        [KindChoice::Http, KindChoice::GraphQl, KindChoice::WebSocket];

    pub fn label(self) -> &'static str {
        match self {
            KindChoice::Http => "HTTP",
            KindChoice::GraphQl => "GraphQL",
            KindChoice::WebSocket => "WebSocket",
        }
    }

    /// What picking it gets you, in the picker's dimmed column — so the list answers "what is
    /// this?" and not only "what is it called?".
    pub fn detail(self) -> &'static str {
        match self {
            KindChoice::Http => "a verb, params and a body",
            KindChoice::GraphQl => "a query and variables",
            KindChoice::WebSocket => "a connection you send messages down",
        }
    }
}

/// The authoring state for whichever kind a buffer is editing.
pub enum KindEditor {
    Http(HttpEditor),
    GraphQl(GraphQlEditor),
    WebSocket(WebSocketEditor),
}

impl KindEditor {
    /// Build the editors for a saved request.
    ///
    /// Exhaustive with no catch-all, for the reason `RequestView::load` was: a kind this cannot
    /// express is a kind whose fields `spec()` will *destroy* on the next save, which is exactly
    /// how form, multipart and binary bodies were once silently emptied.
    pub fn from_spec(kind: &RequestKind, cx: &mut Context<RequestView>) -> Self {
        match kind {
            RequestKind::Http(http) => KindEditor::Http(HttpEditor::from_spec(http, cx)),
            RequestKind::GraphQl(graphql) => {
                KindEditor::GraphQl(GraphQlEditor::from_spec(graphql, cx))
            }
            RequestKind::WebSocket(socket) => {
                KindEditor::WebSocket(WebSocketEditor::from_spec(socket, cx))
            }
        }
    }

    /// A blank buffer of the chosen kind.
    pub fn empty(choice: KindChoice, cx: &mut Context<RequestView>) -> Self {
        match choice {
            KindChoice::Http => KindEditor::Http(HttpEditor::new(cx)),
            KindChoice::GraphQl => KindEditor::GraphQl(GraphQlEditor::new(cx)),
            KindChoice::WebSocket => KindEditor::WebSocket(WebSocketEditor::new(cx)),
        }
    }

    pub fn choice(&self) -> KindChoice {
        match self {
            KindEditor::Http(_) => KindChoice::Http,
            KindEditor::GraphQl(_) => KindChoice::GraphQl,
            KindEditor::WebSocket(_) => KindChoice::WebSocket,
        }
    }

    /// Whether switching away from this kind would throw away something that was typed.
    ///
    /// **Switching kind is destructive in a way switching body type is not.** `set_body_type`
    /// keeps the editor text, the form rows and the multipart parts, so a mistaken change costs
    /// nothing; a kind change replaces the whole `KindEditor` and the other side goes with it.
    /// This is what lets the switch *ask first* rather than silently discard — and answer "no"
    /// on an untouched buffer, which is the common case, so picking a kind up front costs no
    /// extra keystroke.
    pub fn has_content(&self, cx: &App) -> bool {
        match self {
            KindEditor::Http(http) => http.has_content(cx),
            KindEditor::GraphQl(graphql) => graphql.has_content(cx),
            KindEditor::WebSocket(socket) => socket.has_content(cx),
        }
    }

    /// What a switch away from this kind would throw away, in words.
    ///
    /// Listed rather than summarised as "the body", because that was only part of it: an HTTP
    /// request also loses its method and its query params, and a prompt has to name what it is
    /// asking permission to destroy.
    pub fn discards(&self, cx: &App) -> String {
        let mut parts: Vec<&str> = Vec::new();
        match self {
            KindEditor::Http(http) => {
                if !matches!(http.body(cx), zuno_core::Body::Empty) {
                    parts.push("the body");
                }
                if http.query.iter().any(|row| {
                    !row.name.read(cx).text().trim().is_empty()
                        || !row.value.read(cx).text().trim().is_empty()
                }) {
                    parts.push("the query params");
                }
                parts.push("the method");
            }
            KindEditor::GraphQl(graphql) => {
                if !graphql.query.read(cx).text().trim().is_empty() {
                    parts.push("the query");
                }
                if !graphql.variables.read(cx).text().trim().is_empty() {
                    parts.push("the variables");
                }
                if !graphql.operation.read(cx).text().trim().is_empty() {
                    parts.push("the operation name");
                }
            }
            KindEditor::WebSocket(socket) => {
                if !socket.compose.read(cx).text().trim().is_empty() {
                    parts.push("the message you are composing");
                }
                if !socket.subprotocols.read(cx).text().trim().is_empty() {
                    parts.push("the subprotocols");
                }
                if !socket.messages.is_empty() {
                    parts.push("the saved messages");
                }
            }
        }
        match parts.len() {
            0 => "nothing".to_string(),
            1 => parts[0].to_string(),
            _ => format!("{} and {}", parts[..parts.len() - 1].join(", "), parts[parts.len() - 1]),
        }
    }

    /// What `RequestView::spec` reads back out.
    pub fn to_spec(&self, cx: &App) -> RequestKind {
        match self {
            KindEditor::Http(http) => RequestKind::Http(http.to_spec(cx)),
            KindEditor::GraphQl(graphql) => RequestKind::GraphQl(graphql.to_spec(cx)),
            KindEditor::WebSocket(socket) => RequestKind::WebSocket(socket.to_spec(cx)),
        }
    }

    /// Whether this buffer differs from the request it was loaded from.
    ///
    /// **A kind mismatch is itself a change**, and has to be caught before the field-by-field
    /// comparison: the other kind's editors still hold their text, so every field could match
    /// while the request now sends something else entirely.
    pub fn is_dirty(&self, base: &RequestKind, cx: &App) -> bool {
        match (self, base) {
            (KindEditor::Http(editor), RequestKind::Http(base)) => editor.is_dirty(base, cx),
            (KindEditor::GraphQl(editor), RequestKind::GraphQl(base)) => {
                editor.is_dirty(base, cx)
            }
            (KindEditor::WebSocket(editor), RequestKind::WebSocket(base)) => {
                editor.is_dirty(base, cx)
            }
            _ => true,
        }
    }

    /// The tabs this kind contributes, between the spine's Headers and Capture.
    pub fn tabs(&self) -> &'static [KindTab] {
        // `const` rather than an inline array: the slice has to outlive this call, and a
        // borrowed temporary here is a compile error rather than a dangle — which is the whole
        // reason these are `&'static` and not owned.
        const HTTP: [KindTab; 2] = [KindTab::new("Params"), KindTab::new("Body")];
        const GRAPHQL: [KindTab; 2] = [KindTab::new("Query"), KindTab::new("Variables")];
        // Handshake first: it is what you set once, and Message is where the time goes — the
        // same reasoning that puts Body second for HTTP and opens on it.
        const WEBSOCKET: [KindTab; 2] = [KindTab::new("Handshake"), KindTab::new("Message")];

        match self {
            KindEditor::Http(_) => &HTTP,
            KindEditor::GraphQl(_) => &GRAPHQL,
            KindEditor::WebSocket(_) => &WEBSOCKET,
        }
    }

    /// What a tab is *called*, including anything dynamic — a row count, the body's flavour.
    ///
    /// Asked of the kind rather than matched in `request_pane`, because a label that lives at
    /// the render site is a label a new kind cannot change: that is the bug this fixes, where a
    /// GraphQL request drew tabs reading **Params** and **Body**.
    pub fn tab_label(&self, slot: u8) -> SharedString {
        match (self, slot) {
            (KindEditor::Http(http), 0) => {
                SharedString::from(format!("Params {}", http.query.len()))
            }
            (KindEditor::Http(http), _) => match http.body_type {
                crate::request_view::BodyType::Empty => SharedString::from("Body"),
                _ => SharedString::from(format!("Body {}", http.body_label())),
            },
            // **"Query", and it holds mutations too.** The envelope field is named `query` and
            // carries *any* operation — `{"query": "mutation Foo { … }"}` is ordinary GraphQL —
            // and every GraphQL client calls this pane Query, so that is the word people look
            // for. It was briefly "Document", which is the spec's term and nobody's habit.
            //
            // No count: lines are a useful number for a table of rows, not for prose.
            (KindEditor::GraphQl(_), 0) => SharedString::from("Query"),
            (KindEditor::GraphQl(_), _) => SharedString::from("Variables"),
            (KindEditor::WebSocket(_), 0) => SharedString::from("Handshake"),
            (KindEditor::WebSocket(socket), _) => match socket.messages.len() {
                0 => SharedString::from("Message"),
                saved => SharedString::from(format!("Message {saved}")),
            },
        }
    }

    /// Whether the spine's Capture and Assert tabs mean anything here.
    ///
    /// **Both are defined against a single finished response** — a capture reads one body and
    /// publishes into the environment, an assertion returns one verdict — and a session has no
    /// such thing. They ran off `Event::Done`, which a socket never emits, so a socket drew two
    /// tabs that silently did nothing: the same dead-control shape as the Variables tab's add
    /// button, and found the same way.
    ///
    /// Asked of the kind rather than matched in `RequestTab::for_kind`, so the next kind
    /// answers for itself instead of someone remembering to add a case.
    pub fn checks_a_response(&self) -> bool {
        match self {
            KindEditor::Http(_) | KindEditor::GraphQl(_) => true,
            KindEditor::WebSocket(_) => false,
        }
    }

    /// Which of `tabs()` a buffer opens on — where the time actually goes for this kind.
    pub fn default_tab(&self) -> u8 {
        match self {
            // The body, not the params.
            KindEditor::Http(_) => 1,
            // The document.
            KindEditor::GraphQl(_) => 0,
            // The composer. The handshake is set once and rarely revisited.
            KindEditor::WebSocket(_) => 1,
        }
    }

    /// This kind's main text surface — what `Ctrl+F` searches and the formatter rewrites.
    ///
    /// `None` when there is nothing to search: an HTTP request whose body is a form, a file, or
    /// absent has no text editor, and a find bar over it would be a control that does nothing.
    pub fn primary_editor(&self) -> Option<&Entity<Editor>> {
        match self {
            KindEditor::Http(http) => http.primary_editor(),
            KindEditor::GraphQl(graphql) => Some(graphql.primary_editor()),
            KindEditor::WebSocket(socket) => Some(socket.primary_editor()),
        }
    }

    /// The HTTP verb, for the kinds that have one.
    ///
    /// `None` for a kind with no such concept — gRPC is always POST and never shows it, MQTT
    /// has none at all — which is why the method chip asks rather than assuming.
    pub fn method(&self) -> Option<&Method> {
        match self {
            KindEditor::Http(http) => Some(&http.method),
            KindEditor::GraphQl(graphql) => Some(&graphql.method),
            // The handshake is a GET and nothing else is legal, so there is no choice to show.
            KindEditor::WebSocket(_) => None,
        }
    }

    pub fn set_method(&mut self, method: Method) {
        match self {
            KindEditor::Http(http) => http.method = method,
            KindEditor::GraphQl(graphql) => graphql.method = method,
            // Nothing to set. Reached only if something dispatches a method change at a kind
            // that reports `None` from `method()`, which the chip does not draw.
            KindEditor::WebSocket(_) => {}
        }
    }

    pub fn as_http(&self) -> Option<&HttpEditor> {
        match self {
            KindEditor::Http(http) => Some(http),
            KindEditor::GraphQl(_) | KindEditor::WebSocket(_) => None,
        }
    }

    pub fn as_http_mut(&mut self) -> Option<&mut HttpEditor> {
        match self {
            KindEditor::Http(http) => Some(http),
            KindEditor::GraphQl(_) | KindEditor::WebSocket(_) => None,
        }
    }

    pub fn as_graphql(&self) -> Option<&GraphQlEditor> {
        match self {
            KindEditor::GraphQl(graphql) => Some(graphql),
            KindEditor::Http(_) | KindEditor::WebSocket(_) => None,
        }
    }

    pub fn as_graphql_mut(&mut self) -> Option<&mut GraphQlEditor> {
        match self {
            KindEditor::GraphQl(graphql) => Some(graphql),
            KindEditor::Http(_) | KindEditor::WebSocket(_) => None,
        }
    }

    pub fn as_websocket(&self) -> Option<&WebSocketEditor> {
        match self {
            KindEditor::WebSocket(socket) => Some(socket),
            KindEditor::Http(_) | KindEditor::GraphQl(_) => None,
        }
    }

    pub fn as_websocket_mut(&mut self) -> Option<&mut WebSocketEditor> {
        match self {
            KindEditor::WebSocket(socket) => Some(socket),
            KindEditor::Http(_) | KindEditor::GraphQl(_) => None,
        }
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        match self {
            KindEditor::Http(http) => http.is_focused(window, cx),
            KindEditor::GraphQl(graphql) => graphql.is_focused(window, cx),
            KindEditor::WebSocket(socket) => socket.is_focused(window, cx),
        }
    }
}
