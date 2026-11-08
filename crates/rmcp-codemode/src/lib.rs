//! Code mode for RMCP.
//!
//! This crate implements the third-party reference crate proposed in
//! [modelcontextprotocol/rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437):
//! a client-side aggregator that connects to N upstream MCP servers, merges their
//! tools into one registry, and exposes them to any host as a `codemode` script
//! tool — the model writes a script, upstream tools run inside a sandbox, and only
//! the script's output reaches the model.
//!
//! # Primitives
//!
//! - [`ToolExposure`] — per-server exposure metadata (`direct` / `deferred` /
//!   `codemode`), mirroring Codex's `ToolExposure` enum and pi's `exposure` field.
//! - [`registry`] — the aggregated tool registry with namespaces and exposure.
//! - [`search`] — BM25 tool search, the discovery primitive for deferred tools.
//! - [`aggregate`] — the client-side aggregation interface: connect upstreams →
//!   aggregated registry → dispatch.
//!
//! Semantics follow pi's shipped `codemode` tool
//! ([docs](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/codemode.md))
//! with OpenCode's [`packages/codemode`](https://github.com/anomalyco/opencode/blob/dev/packages/codemode)
//! as the second reference for the aggregation side.

#![deny(missing_docs)]

pub mod aggregate;
pub mod exposure;
pub mod identity;
pub mod naming;
pub mod registry;
pub mod result;
pub mod search;
pub mod signature;

pub use aggregate::{Aggregator, UpstreamConfig};
pub use exposure::ToolExposure;
pub use identity::{normalize_identifier, unknown_tool_message};
pub use naming::NamingConvention;
pub use registry::{NamespaceInfo, Registry, RegistryEntry};
pub use result::{
    DEFAULT_MAX_OUTPUT_TOKENS, MAX_OUTPUT_CALLS, MAX_OUTPUT_CHARS, OutputItem, ScriptOptions,
    ScriptResult, ToolCall, len_of, parse_image_input, render_result_text,
    render_result_text_with_overflow,
};
pub use search::{SearchHit, SearchIndex};
pub use signature::ts_declaration;
