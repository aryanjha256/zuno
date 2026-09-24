//! Authoring a gRPC call: which schema, which method, and the request message.
//!
//! **The chosen method is held as three plain fields, not as a compiled descriptor.** A
//! `zuno_core::grpc::Method` is the richer thing and it is tempting to hold one — but it only
//! exists once a `.proto` has compiled, and `from_spec` runs on a saved request whose file may
//! be missing, unreadable or not yet written. A buffer that could not name its own method until
//! a compile succeeded would lose that method on the next save, which is exactly how form,
//! multipart and binary bodies were once silently emptied: `spec()` derives, and what the
//! editors cannot represent is *destroyed* rather than merely hidden.
//!
//! So the schema is read for the picker and for sending, and never to decide what this buffer
//! is. The three fields round-trip on their own.

use gpui::{App, AppContext, Context, Entity, Focusable, Window};

use zuno_core::request::GrpcRequest;

use crate::input::{Editor, TextInput};
use crate::request_view::RequestView;

pub struct GrpcEditor {
    /// A bare filename inside the collection's `protos/`, or a path to a `.proto` anywhere.
    /// See `zuno_core::grpc::resolve_proto` for why both spellings are allowed.
    pub proto: Entity<TextInput>,
    /// The fully-qualified service, `helloworld.Greeter`. Written by the method picker.
    pub service: String,
    /// The bare method name, `SayHello`.
    pub method: String,
    /// Whether the chosen method streams its requests, and whether it streams its replies.
    ///
    /// Copied from the descriptor when the method is picked, because the button has to say Send
    /// or Connect before anything has been compiled. See `GrpcRequest::server_streaming`.
    pub client_streaming: bool,
    pub server_streaming: bool,
    /// The request message as JSON. Multi-line, and the surface where the time goes.
    pub message: Entity<Editor>,
}

impl GrpcEditor {
    pub fn new(cx: &mut Context<RequestView>) -> Self {
        Self::from_spec(&GrpcRequest::default(), cx)
    }

    pub fn from_spec(grpc: &GrpcRequest, cx: &mut Context<RequestView>) -> Self {
        Self {
            proto: cx.new(|cx| {
                TextInput::new(
                    grpc.proto.clone(),
                    // The placeholder is the *portable* spelling, because that is the one worth
                    // teaching: a bare name resolves inside the collection and survives being
                    // cloned by somebody else.
                    "greeter.proto",
                    "GrpcProto",
                    cx,
                )
            }),
            service: grpc.service.clone(),
            method: grpc.method.clone(),
            client_streaming: grpc.client_streaming,
            server_streaming: grpc.server_streaming,
            message: cx.new(|cx| Editor::new(grpc.message.clone(), "{ \"name\": \"zuno\" }", cx)),
        }
    }

    /// Record a method chosen from the picker.
    ///
    /// All three fields at once, because they are one fact read off one descriptor — setting
    /// them separately is how `server_streaming` would come to disagree with `method`.
    pub fn choose(&mut self, method: &zuno_core::grpc::Method) {
        self.service = method.service.clone();
        self.method = method.name.clone();
        self.client_streaming = method.client_streaming;
        self.server_streaming = method.server_streaming;
    }

    /// `helloworld.Greeter/SayHello`, or nothing chosen yet.
    ///
    /// Not `Method::path`: that is the HTTP/2 `:path` and carries a leading slash, which is the
    /// wire's spelling rather than a person's.
    pub fn chosen(&self) -> Option<String> {
        if self.service.trim().is_empty() || self.method.trim().is_empty() {
            return None;
        }
        Some(format!("{}/{}", self.service, self.method))
    }

    /// Whether more messages may be sent into an open call.
    ///
    /// **Only the client half.** A server-streaming call sends exactly one request and then
    /// listens, so a composer that stayed live on one would offer a send the protocol has no
    /// way to deliver — the dead-control shape this codebase keeps finding.
    pub fn sends_more(&self) -> bool {
        self.client_streaming
    }

    /// Whether anything has been typed here — see `KindEditor::has_content`.
    pub fn has_content(&self, cx: &App) -> bool {
        !self.proto.read(cx).text().trim().is_empty()
            || !self.service.trim().is_empty()
            || !self.method.trim().is_empty()
            || !self.message.read(cx).text().trim().is_empty()
    }

    /// What `spec()` reads back out. The mirror of `from_spec`.
    pub fn to_spec(&self, cx: &App) -> GrpcRequest {
        GrpcRequest {
            proto: self.proto.read(cx).text().trim().to_string(),
            service: self.service.clone(),
            method: self.method.clone(),
            message: self.message.read(cx).text().to_string(),
            client_streaming: self.client_streaming,
            server_streaming: self.server_streaming,
        }
    }

    /// Whether anything here differs from the request as it was loaded.
    ///
    /// Destructured with no `..`, for `WebSocketEditor::is_dirty`'s reason: a field added to
    /// `GrpcRequest` must fail to compile here until someone decides whether editing it makes a
    /// buffer dirty.
    pub fn is_dirty(&self, base: &GrpcRequest, cx: &App) -> bool {
        let GrpcRequest {
            proto,
            service,
            method,
            message,
            client_streaming,
            server_streaming,
        } = base;

        self.proto.read(cx).text().trim() != proto
            || self.service != *service
            || self.method != *method
            || self.message.read(cx).text() != *message
            || self.client_streaming != *client_streaming
            || self.server_streaming != *server_streaming
    }

    /// The message editor — what `Ctrl+F` searches and the formatter rewrites.
    pub fn primary_editor(&self) -> &Entity<Editor> {
        &self.message
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.proto.read(cx).focus_handle(cx).is_focused(window)
            || self.message.read(cx).focus_handle(cx).is_focused(window)
    }
}
