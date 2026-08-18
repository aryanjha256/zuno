//! `zuno-core` — the GPUI-free half of Zuno.
//!
//! Request modeling, HTTP execution, JSON flattening, and text buffers all live
//! here so they can be tested without opening a window. Nothing in this crate
//! knows about rendering, colors, or key bindings.
//!
//! Landed so far (M1.0): the request and response models.
//! Still to come: `engine` (M1.2), `json` (M1.3), `text` (M1.4).

pub mod request;
pub mod response;

pub use request::{
    Body, FormField, Header, Method, MultipartField, MultipartValue, QueryParam, RawKind,
    RequestId, RequestSettings, RequestSpec,
};
pub use response::{HttpVersion, ResponseData, SizeInfo, StatusClass, Timing};
