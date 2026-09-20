//! Authoring a WebSocket: what to offer at the handshake, and what to say once it is open.
//!
//! **A socket is a conversation, so the editor is a composer rather than a body.** The other
//! two kinds build one payload and send it; here the payload is written again and again down a
//! connection that outlives any single message. `compose` is the box you type the next one in,
//! and `messages` is what you kept so you do not type the same subscribe envelope twice.
//!
//! **No method.** The handshake is a GET and nothing else is legal, so there is no verb to
//! choose — `KindEditor::method` answers `None` here and the chip hides itself, which is the
//! case it was made an `Option` for.

use gpui::{App, AppContext, Context, Entity, Focusable, Window};
use zuno_core::{SavedMessage, WebSocketRequest};

use crate::input::{Editor, TextInput};
use crate::request_view::RequestView;

pub struct WebSocketEditor {
    /// `Sec-WebSocket-Protocol` offers, comma-separated as the header itself is. One line
    /// rather than a row table: it is an ordered list of bare tokens with no second column,
    /// and a table would be a name column with nothing to put beside it.
    pub subprotocols: Entity<TextInput>,
    /// The next message to send. Multi-line, because what goes down a socket is usually JSON.
    pub compose: Entity<Editor>,
    /// Messages kept with the request.
    ///
    /// **Held as plain data, not as editor entities.** Nothing edits them in place — they are
    /// picked and loaded into `compose` — so building a `TextInput` per saved message would be
    /// live state for something that is only ever read. That is the opposite of the twelve
    /// loose fields this module exists to remove.
    pub messages: Vec<SavedMessage>,
}

impl WebSocketEditor {
    pub fn new(cx: &mut Context<RequestView>) -> Self {
        Self::from_spec(&WebSocketRequest::default(), cx)
    }

    pub fn from_spec(socket: &WebSocketRequest, cx: &mut Context<RequestView>) -> Self {
        Self {
            subprotocols: cx.new(|cx| {
                TextInput::new(
                    socket.subprotocols.join(", "),
                    "graphql-transport-ws, wamp.2.json",
                    "WebSocketSubprotocols",
                    cx,
                )
            }),
            // Starts empty on purpose: the composer is what you are about to send, and a
            // buffer that reloads the last thing you sent would send it twice on a stray
            // Enter. What is worth keeping goes in `messages`.
            compose: cx.new(|cx| Editor::new("", "{ \"type\": \"ping\" }", cx)),
            messages: socket.messages.clone(),
        }
    }

    /// Keep what is in the composer, under a name derived from it.
    ///
    /// **Auto-named rather than prompting.** A name field beside every save is chrome on the
    /// surface you use most, to fill in something the payload already says: nearly every
    /// protocol that rides a socket puts a discriminator in `type`, so that is the label. It is
    /// a label and not a key, so duplicates are allowed — two `subscribe` messages differing in
    /// their variables is the normal case, not a mistake to prevent.
    pub fn save(&mut self, body: String) {
        let name = name_for(&body);
        self.messages.push(SavedMessage { name, body });
    }

    /// Forget one. Out of range is ignored rather than panicking: the index comes from a row
    /// that was painted a frame ago, and the list can have changed under it.
    pub fn forget(&mut self, ix: usize) {
        if ix < self.messages.len() {
            self.messages.remove(ix);
        }
    }

    /// Whether anything has been typed here — see `KindEditor::has_content`.
    pub fn has_content(&self, cx: &App) -> bool {
        !self.subprotocols.read(cx).text().trim().is_empty()
            || !self.compose.read(cx).text().trim().is_empty()
            || !self.messages.is_empty()
    }

    /// What `spec()` reads back out. The mirror of `from_spec`, and the reason a socket
    /// survives a load/save round trip.
    ///
    /// `compose` is deliberately **not** written back. It is the next message, not part of the
    /// request — saving it would put an unsent draft into a file meant to be committed, and
    /// every collaborator would inherit somebody's half-typed frame.
    pub fn to_spec(&self, cx: &App) -> WebSocketRequest {
        WebSocketRequest {
            subprotocols: split_protocols(&self.subprotocols.read(cx).text()),
            messages: self.messages.clone(),
        }
    }

    /// Whether anything here differs from the request as it was loaded.
    ///
    /// Destructured with no `..`, for `GraphQlEditor::is_dirty`'s reason: a field added to
    /// `WebSocketRequest` must fail to compile here until someone decides whether editing it
    /// makes a buffer dirty.
    pub fn is_dirty(&self, base: &WebSocketRequest, cx: &App) -> bool {
        let WebSocketRequest {
            subprotocols,
            messages,
        } = base;

        split_protocols(&self.subprotocols.read(cx).text()) != *subprotocols
            || self.messages != *messages
    }

    /// The composer — what `Ctrl+F` searches and the formatter rewrites, for the same reason
    /// HTTP answers with its body editor.
    pub fn primary_editor(&self) -> &Entity<Editor> {
        &self.compose
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.subprotocols.read(cx).focus_handle(cx).is_focused(window)
            || self.compose.read(cx).focus_handle(cx).is_focused(window)
    }
}

/// A label for a payload, taken from the payload.
///
/// `type` first, because graphql-transport-ws, Phoenix, Action Cable, STOMP-over-WS and most
/// hand-rolled protocols all carry one and it is exactly the word a person would have typed.
/// Otherwise the first line, shortened — which at least says which of two saved messages this
/// is, where "Message 3" would not.
fn name_for(body: &str) -> String {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(serde_json::Value::String(kind)) = map.get("type")
        && !kind.trim().is_empty()
    {
        return kind.trim().to_string();
    }

    let first = body.trim().lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "message".to_string();
    }
    zuno_core::request::elide(first, 24).into_owned()
}

/// Split the one-line subprotocol list the way the header is read.
///
/// Empties are dropped rather than preserved: a trailing comma is a typing artifact, and an
/// empty token in `Sec-WebSocket-Protocol` is not a protocol anyone can select.
fn split_protocols(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{name_for, split_protocols};

    /// The label comes from the payload, which is the whole reason saving needs no prompt.
    #[test]
    fn a_saved_message_is_named_by_its_own_type_field() {
        assert_eq!(
            name_for(r#"{"type":"subscribe","id":"1"}"#),
            "subscribe",
            "an envelope's discriminator is the name a person would have typed"
        );
        // Not JSON, so there is nothing to read — the first line still says which one it is.
        assert_eq!(name_for("SUBSCRIBE topic=prices\nmore"), "SUBSCRIBE topic=prices");
        // JSON without a `type` falls back the same way rather than to a serial number.
        assert_eq!(name_for(r#"{"op":"ping"}"#), r#"{"op":"ping"}"#);
        assert_eq!(name_for("   "), "message");
    }

    #[test]
    fn subprotocols_round_trip_through_one_line() {
        assert_eq!(
            split_protocols(" graphql-transport-ws , wamp.2.json ,, "),
            vec!["graphql-transport-ws".to_string(), "wamp.2.json".to_string()],
            "whitespace and a trailing comma are typing, not protocols"
        );
        assert!(split_protocols("   ").is_empty());
    }
}
