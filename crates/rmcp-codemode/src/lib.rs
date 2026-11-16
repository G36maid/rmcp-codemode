//! The engine-agnostic codemode core: contract types, engine trait, and
//! naming rules. A 1:1 Rust mirror of pi's `@earendil-works/pi-codemode`
//! public API (`packages/codemode/src/types.ts`) — see SPEC.md §5.
//!
//! This crate never links a JS engine; the QuickJS implementation lives in
//! `rmcp-codemode-quickjs`, and MCP-wire concerns (catalog descriptions,
//! discovery globals, upstream ingestion) live at the server layer (SPEC
//! §4, D14).

pub mod catalog;
pub mod declarations;
pub mod engine;
pub mod mcp;
pub mod naming;
pub mod source;
pub mod types;

pub use catalog::{
    CodemodeMode, DEFAULT_CODEMODE_INLINE_BUDGET, DescriptionOptions, MAX_SERVER_DESCRIPTION_CHARS,
    MAX_SERVERS_SECTION_CHARS, MCP_SERVERS_SECTION, McpServerListing, ServerReach, ToolExposure,
    ToolNamespace, create_codemode_description, describe_globals, describe_output,
    describe_script_call, get_tool_exposure, listed_tools, render_servers_section,
    render_tool_section, to_codemode_declaration, tool_pattern_matches,
};
pub use declarations::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, Declarations, render_declarations, render_tool_output_type,
    render_tool_sample, render_tool_signature, resolve_schema_ref, schema_to_type,
};
pub use engine::{Engine, RunHandle};
pub use source::{
    CODEMODE_OPTIONS_PREFIX, CodemodeSourceError, ParsedSource, SourceOptions,
    parse_codemode_source,
};
pub use types::{
    Call, CallStatus, CallTarget, DeclaredGlobal, DeclaredTool, DeliverError, During, ErrorKind,
    ExecuteOptions, HostCall, JsonSchema, Outcome, OutputItem, RunOptions, RunResult,
    SandboxOptions, ScriptError, StoreWrites, Success, Tool, ToolContext, ToolError, ToolFn,
};
