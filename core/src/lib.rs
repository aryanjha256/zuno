//! `zuno-core` — the GPUI-free half of Zuno.
//!
//! Request modeling, HTTP execution, JSON flattening, and text buffers all live
//! here so they can be tested without opening a window. Nothing in this crate
//! knows about rendering, colors, or key bindings.
//!
//! Landed so far: the request and response models (M1.0), the HTTP engine (M1.2).
//! Still to come: `json` (M1.3), `text` (M1.4).

pub mod engine;
pub mod request;
pub mod response;

pub use engine::{Engine, EngineError, Event, JobId};
pub use request::{
    Body, FormField, Header, Method, MultipartField, MultipartValue, QueryParam, RawKind,
    RequestId, RequestSettings, RequestSpec,
};
pub use response::{HttpVersion, ResponseData, SizeInfo, StatusClass, Timing};
