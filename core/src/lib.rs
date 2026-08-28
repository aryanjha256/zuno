//! `zuno-core` — the GPUI-free half of Zuno.
//!
//! Request modeling, HTTP execution, JSON flattening, and text buffers all live
//! here so they can be tested without opening a window. Nothing in this crate
//! knows about rendering, colors, or key bindings.
//!
//! Landed so far: the request and response models, the HTTP engine, the JSON outline and line
//! index, response diffing, curl import, the collection format, environments, fuzzy matching for
//! the picker, and body search. Listed by *what* rather than by milestone — the milestone tags
//! this line used to carry stopped meaning anything once modules kept arriving after M1.

pub mod collection;
pub mod curl;
pub mod diff;
pub mod engine;
pub mod environment;
pub mod fuzzy;
pub mod highlight;
pub mod json;
pub mod lines;
pub mod request;
pub mod response;
pub mod search;

pub use collection::CollectionError;
pub use environment::{Environment, EnvironmentError, Resolver};
pub use curl::{CurlError, CurlImport};
pub use diff::ResponseDiff;
pub use engine::{Engine, EngineError, Event, JobId};
pub use highlight::{Token, TokenKind};
pub use json::{JsonError, JsonOutline, Row, RowKind, ScalarKind, Span};
pub use lines::LineIndex;
pub use request::{
    Body, FormField, Header, Method, MultipartField, MultipartValue, QueryParam, RawKind,
    RequestId, RequestSettings, RequestSpec, label_for,
};
pub use response::{HttpVersion, ResponseData, SizeInfo, StatusClass, Timing};
pub use search::Hits;
