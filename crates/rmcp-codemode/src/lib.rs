//! The engine-agnostic codemode core.
//!
//! Code mode lets a model write a script that calls tools, so intermediate
//! results stay inside the script and tool schemas stay out of the context.
//! The semantics are the union of three shipped designs, expressed as one
//! model instead of glued pieces:
//!
//! - **pi** (`@earendil-works/pi-codemode` + the coding-agent extension):
//!   the script contract (globals, `@options`, output caps, result format,
//!   worker admission, close-match guards), namespace metadata
//!   (`ToolNamespace` with descriptions, instructions, aliases), the
//!   catalog's round-robin inline budget, host-controlled runs
//!   (`start()`/`deliver()`), and deterministic replay (virtual clock +
//!   seeded PRNG + fingerprint).
//! - **OpenCode** (`@opencode/codemode`): the tool space as a hierarchical
//!   record — path segments are namespaces, namespace descriptions
//!   participate in search, discovery is paged (`{ items, remaining, next }`).
//! - **Cloudflare** (`@cloudflare/codemode`): the connector as a namespace
//!   (`name()`/`instructions()`/`tool()` decoration), per-tool approval and
//!   revert annotations, the durable call log's states, and snippets.
//!
//! Everything here is engine-agnostic. [`engine`] defines what a script
//! backend must honor; `rmcp-codemode-quickjs` is the reference backend.

#![deny(missing_docs)]

pub mod budget;
pub mod catalog;
pub mod engine;
pub mod identity;
pub mod result;
pub mod search_shim;
pub mod signature;
pub mod space;
pub mod upstream;

pub use budget::{
    Budget, STORE_MAX_TOTAL_CHARS, STORE_MAX_VALUE_CHARS, ScriptOptions, extract_options,
};
pub use catalog::{Catalog, Described, Mode};
pub use engine::{
    AnsweredCall, Decision, Delivery, Determinism, Engine, EngineError, HostCall, Outcome,
    RecordedOutcome, RunContext, RunHandle, SuspendState, SuspendingEngine,
};
pub use identity::normalize_identifier;
pub use result::{
    CallRecord, CallStatus, OutputItem, ScriptResult, StoreWrites, item_chars, render_result_text,
    render_result_text_with_overflow,
};
/// What a script's call may resolve to, as the host answers a [`HostCall`].
///
/// A rejection becomes an `Error` with `message` inside the script, carrying
/// the own properties of `properties` (pi's outcome properties, so hosts can
/// attach structured data such as `structuredOutput`).
pub use serde_json::Value;
pub use space::{
    Exposure, NamespaceMeta, Space, ToolAnnotations, ToolFn, ToolFnFut, ToolHandle,
    flat_identifier, resolve_path, validate_segments,
};
