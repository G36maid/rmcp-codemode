//! Sandbox-facing tool and error types.
//!
//! The contract between a host (or the server module) and the script
//! sandbox: how a tool is registered and how failures are reported.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
};

use serde_json::Value;

/// An async tool dispatcher registered with the sandbox.
///
/// Receives the tool call arguments as a JSON string (or `"null"`), resolves
/// with the tool result as a JSON string, or rejects with an error message.
pub type ToolFn = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// A tool exposed to scripts.
#[allow(clippy::exhaustive_structs)] // user-constructed by design
pub struct SandboxTool {
    /// The tool's full registry path, e.g. `github.list_issues`. Nesting
    /// mirrors the path: `tools.github.list_issues`.
    pub name: String,
    /// Shown in `ALL_TOOLS` and `searchTools`.
    pub description: Option<String>,
    /// The tool's JSON Schema for its input, when known. Rendered as a
    /// TypeScript declaration by `describeTool` and `describeNamespace`.
    pub input_schema: Option<Value>,
    /// The async dispatcher.
    pub execute: ToolFn,
}

impl SandboxTool {
    /// A tool from a path, description, and dispatcher.
    pub fn new(name: impl Into<String>, description: impl Into<String>, execute: ToolFn) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            input_schema: None,
            execute,
        }
    }

    /// Attach the tool's JSON Schema for its input, enabling TypeScript
    /// declarations in `describeTool`.
    pub fn with_input_schema(mut self, schema: Value) -> Self {
        self.input_schema = Some(schema);
        self
    }
}

/// Errors from the sandbox layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// The script failed to parse, threw, or was aborted.
    #[error("{0}")]
    Script(String),
    /// The sandbox itself failed (engine error).
    #[error("sandbox error: {0}")]
    Engine(String),
}
/// The script-visible store: small JSON values kept across script runs.
/// pi's semantics: writes buffer during the run and commit only when the
/// script succeeds, so a failed script leaves no partial writes.
pub type Store = Arc<Mutex<BTreeMap<String, Value>>>;

/// Maximum characters of JSON for one stored value (pi's cap).
pub const STORE_MAX_VALUE_CHARS: usize = 262_144;

/// Maximum characters of JSON across all stored values (pi's cap).
pub const STORE_MAX_TOTAL_CHARS: usize = 1_048_576;
/// Default VM heap limit (pi's limit): 256 MB.
pub const DEFAULT_MEMORY_LIMIT: usize = 256 * 1024 * 1024;
/// Map a plain rquickjs error into a sandbox error.
pub fn rquickjs_error(err: rquickjs::Error) -> SandboxError {
    SandboxError::Engine(err.to_string())
}
