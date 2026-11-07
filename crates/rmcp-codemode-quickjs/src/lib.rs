//! QuickJS script sandbox for rmcp-codemode.
//!
//! Runs model-written JavaScript in a QuickJS VM (via
//! [`rquickjs`](https://docs.rs/rquickjs)) where the only capability is calling
//! registered tools. Follows pi's `codemode` semantics:
//!
//! - The script is the body of an async function: top-level `await` and
//!   `return` work.
//! - `tools.<namespace>.<tool>(args)` resolves upstream MCP tools as
//!   namespaced functions; arguments and results make a JSON round trip.
//! - No file system, network, timers, or Node APIs: scripts reach the outside
//!   world only through tool calls.
//! - Only the script's return value, `text()` output, and `console` lines
//!   reach the model — intermediate tool results never round-trip through the
//!   model.
//!
//! # Bridge design
//!
//! Everything crosses the VM boundary as JSON strings through narrow
//! functions (see [`bridge`]), so neither side needs native object
//! conversion. The result contract and its model-facing rendering live in
//! `rmcp_codemode::result` — engine-agnostic, shared with the core crate.
//!
//! # Module map
//!
//! - [`types`]: `SandboxTool`, `ToolFn`, `SandboxError`, `Store`.
//! - [`sandbox`]: the `Sandbox` — one fresh VM per script run.
//! - [`bridge`]: the globals bound into every VM.
//! - [`server`]: the `codemode` MCP server (`CodemodeServer`), exposure
//!   mixing, the worker pool, and store persistence.

#![deny(missing_docs)]

mod bridge;
mod sandbox;
pub mod server;
mod types;

pub use rmcp_codemode::{
    DEFAULT_MAX_OUTPUT_TOKENS, MAX_OUTPUT_CALLS, MAX_OUTPUT_CHARS, OutputItem, ScriptOptions,
    ScriptResult, ToolCall, normalize_identifier, render_result_text,
    render_result_text_with_overflow, ts_declaration,
};
pub use sandbox::Sandbox;
pub use types::{
    STORE_MAX_TOTAL_CHARS, STORE_MAX_VALUE_CHARS, SandboxError, SandboxTool, Store, ToolFn,
};
