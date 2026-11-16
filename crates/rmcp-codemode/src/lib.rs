//! The engine-agnostic codemode core: contract types, engine trait, and
//! naming rules. A 1:1 Rust mirror of pi's `@earendil-works/pi-codemode`
//! public API (`packages/codemode/src/types.ts`) — see SPEC.md §5.
//!
//! This crate never links a JS engine; the QuickJS implementation lives in
//! `rmcp-codemode-quickjs`, and MCP-wire concerns (catalog descriptions,
//! discovery globals, upstream ingestion) live at the server layer (SPEC
//! §4, D14).

pub mod engine;
pub mod naming;
pub mod types;

pub use engine::{Engine, RunHandle};
pub use types::{
    Call, CallStatus, CallTarget, DeclaredGlobal, DeclaredTool, DeliverError, During, ErrorKind,
    ExecuteOptions, HostCall, JsonSchema, Outcome, OutputItem, RunOptions, RunResult,
    SandboxOptions, ScriptError, StoreWrites, Success, Tool, ToolContext, ToolError, ToolFn,
};
