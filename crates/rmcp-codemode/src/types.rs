//! Contract types for the codemode engine, mirroring pi's `types.ts` 1:1.
//!
//! Every type doc-comments its pi source name so ported tests can be traced
//! back to the oracle suite (`packages/codemode/test/`). Field semantics are
//! pi's; see SPEC.md §5 for the contract narrative.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// The single argument schema document. Values are never validated against it;
/// it exists to render declarations. (pi: `CodemodeJsonSchema`.)
pub type JsonSchema = Value;

/// Context handed to a tool's execute closure. (pi: `CodemodeToolContext`.)
#[derive(Clone, Debug)]
pub struct ToolContext {
    /// Aborted when the script finishes (including unawaited calls), the
    /// execution times out, the caller aborts, or the sandbox is closed.
    pub signal: CancellationToken,
}

/// A callable the script reaches as `tools.<js_name>(args)` /
/// `tools["<name>"](args)`, or — for globals — `<name>(args)`. (pi:
/// `CodemodeTool`.)
#[derive(Clone)]
pub struct Tool {
    /// Tools: the canonical name (identifiers invalid in JS are normalized to
    /// `js_name` for property access). Globals: must be a valid identifier, or
    /// `<namespace>.<member>` which groups into a frozen namespace object.
    pub name: String,
    /// Doc comment in declarations; `ALL_TOOLS` entry for tools.
    pub description: Option<String>,
    /// Schema of the single argument; rendered as the parameter type.
    pub input_schema: Option<JsonSchema>,
    /// Schema of the resolved value; rendered as the promise type.
    pub output_schema: Option<JsonSchema>,
    /// Globals only: `execute` receives all call arguments as an array.
    pub spread: bool,
    /// Globals only: explicit TypeScript parameter/return list replacing the
    /// schema rendering, e.g. `(type: string): Promise<Model[]>`.
    pub signature: Option<String>,
    /// `args` is what the script passed, after a JSON round trip. The return
    /// value must be JSON-serializable; a thrown error surfaces in the script
    /// as an `Error` with the same message.
    pub execute: ToolFn,
}

pub type ToolFn = std::sync::Arc<
    dyn Fn(Value, ToolContext) -> futures::future::BoxFuture<'static, Result<Value, ToolError>>
        + Send
        + Sync,
>;

/// Why a tool invocation failed. (pi: a thrown error becomes an `Error` with
/// the same message in the script.)
#[derive(Debug, Clone)]
pub struct ToolError {
    pub message: String,
}

/// One item of the script's output, in production order. (pi:
/// `CodemodeOutputItem`.)
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    /// `text()` or `console.*` (`console: true`).
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        console: bool,
    },
    /// `image()`; `data` is base64.
    Image { data: String, mime_type: String },
}

/// How one nested call ended. (pi: `CodemodeCallStatus`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Ok,
    Error,
    Cancelled,
}

/// A nested call the script made. (pi: `CodemodeCall`.)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Call {
    pub name: String,
    pub status: CallStatus,
    pub duration_ms: u64,
}

/// Why a run failed. (pi: `CodemodeErrorKind`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The script threw or failed to parse; `name`/`stack` come from the
    /// script's error.
    Script,
    /// The overall deadline expired, or an in-thread VM used up its interrupt
    /// budget.
    Timeout,
    /// The caller's signal fired or the sandbox was closed.
    Aborted,
    /// The VM or its transport failed outside the script's control.
    Sandbox,
}

/// A run failure. (pi: `CodemodeError`.)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ScriptError {
    pub kind: ErrorKind,
    pub name: Option<String>,
    pub message: String,
    pub stack: Option<String>,
}

/// Keys the script changed with `store()`; only successful runs report writes.
/// (pi: `CodemodeStoreWrites`.)
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct StoreWrites {
    pub set: BTreeMap<String, Value>,
    /// Keys stored as `undefined`.
    pub delete: Vec<String>,
}

/// The result the script itself ended with (returned, threw, or exited):
/// result `seq` was being processed, and `issued` calls had been issued.
/// (pi: `CodemodeDuring`.)
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct During {
    pub seq: u64,
    pub issued: u64,
}

/// The outcome of an execution. `output` and `calls` are kept for failed runs
/// too, up to the failure. (pi: `CodemodeResult`; the shared fields are
/// factored out Rust-style.)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RunResult {
    /// `Ok` = the script returned (or `exit()`ed with `value: None`);
    /// `Err` = it threw/timed out/was aborted/the sandbox failed.
    pub outcome: Result<Success, ScriptError>,
    pub output: Vec<OutputItem>,
    pub calls: Vec<Call>,
    pub during: Option<During>,
}

/// The success half of [`RunResult`]. (pi: the `ok: true` variant of
/// `CodemodeResult`.)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Success {
    /// The script's return value; `exit()` completes with `Value::Null`.
    pub value: Value,
    pub store_writes: StoreWrites,
}

/// A call the script made in a host-controlled run, answered via
/// `crate::engine::RunHandle::deliver`. (pi: `CodemodeHostCall`.)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HostCall {
    /// The VM's own number for the call: `1`, `2`, … in the order the script
    /// made its calls (tools and globals).
    pub id: u64,
    pub target: CallTarget,
    pub name: String,
    /// What the script passed, after a JSON round trip; for spread globals the
    /// argument list.
    pub args: Value,
}

/// Who was called. (pi: `CodemodeHostCall.target`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallTarget {
    Tool,
    Global,
}

/// A tool as a host-controlled run's script sees it. (pi:
/// `CodemodeDeclaredTool`.)
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredTool {
    pub name: String,
    /// The identifier the script uses, `tools.<js_name>`; default
    /// `crate::naming::to_codemode_identifier(name)`. A host replaying a run
    /// passes the one the run started with, so a changed mapping cannot rename
    /// a tool under a resumed script.
    pub js_name: Option<String>,
    pub description: Option<String>,
}

/// A global as a host-controlled run's script sees it. (pi:
/// `CodemodeDeclaredGlobal`.)
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredGlobal {
    pub name: String,
    pub spread: bool,
}

/// A call's answer. `Err` becomes an `Error` with `message` in the script,
/// carrying the own properties of `properties` (`name`, `message`, `stack`
/// ignored). (pi: `CodemodeOutcome`.)
#[derive(Debug, Clone)]
pub enum Outcome {
    Ok(Value),
    Err {
        message: String,
        properties: Option<BTreeMap<String, Value>>,
    },
}

/// Options of a host-controlled run. (pi: `CodemodeRunOptions`.)
pub struct RunOptions {
    /// The VM clock's start, in ms since the epoch: `Date.now()` until the
    /// first result arrives.
    pub started_at: u64,
    /// Seeds `Math.random()` and other randomness; 32-bit unsigned.
    pub seed: u32,
    pub tools: Vec<DeclaredTool>,
    pub globals: Vec<DeclaredGlobal>,
    /// Values the script reads with `load(key)`.
    pub store: BTreeMap<String, Value>,
    pub signal: Option<CancellationToken>,
    /// Deadline of this run in ms. `None` = no deadline (not the sandbox
    /// default).
    pub timeout_ms: Option<u64>,
    /// The VM's fingerprint, before the script runs and before any of its
    /// calls (§5.5).
    pub on_started: Option<Box<dyn Fn(String) + Send>>,
    /// The script made a call. Answer it with `crate::engine::RunHandle::deliver`,
    /// now or later.
    pub on_call: Box<dyn Fn(HostCall) + Send>,
    /// The VM processed result `seq` (`0`: before any result) and waits again,
    /// having issued `issued` calls in total.
    pub on_idle: Option<Box<dyn Fn(u64, u64) + Send>>,
}

/// Options every sandbox accepts at construction, wherever its VM runs. (pi:
/// `CodemodeSandboxBaseOptions`.)
#[derive(Clone, Default)]
pub struct SandboxOptions {
    pub tools: Vec<Tool>,
    /// Top-level helper functions; behave like tools (JSON round trip,
    /// promise result) but are not recorded in `result.calls`. Names must be
    /// identifiers and must not shadow the built-in globals (`tools`,
    /// `ALL_TOOLS`, `console`, `text`, `image`, `exit`, `store`, `load`).
    pub globals: Vec<Tool>,
    /// Overall deadline per execution, including time in tools. `None`
    /// disables it; the run ends only when the script settles or is aborted.
    pub timeout_ms: Option<u64>,
    /// Poll budget: the VM fails the run as `timeout` once the engine's
    /// interrupt handler has polled this often, counting across awaits on
    /// tools — a script that computes forever never settles. `None` = no
    /// budget (pi: `InlineSandboxOptions.interruptBudget`, default
    /// `DEFAULT_INTERRUPT_BUDGET = 100_000`; kept out of pi's contract
    /// `types.ts` — it is a sandbox-construction knob).
    pub interrupt_budget: Option<u64>,
    /// Maximum memory the VM may allocate; beyond it fails inside the script
    /// as `InternalError: out of memory`.
    pub memory_limit_bytes: Option<u64>,
}

/// Per-execution overrides. (pi: `CodemodeExecuteOptions`.)
#[derive(Clone, Default)]
pub struct ExecuteOptions {
    pub signal: Option<CancellationToken>,
    /// Overrides the sandbox default for this execution.
    pub timeout_ms: Option<u64>,
    /// Values the script reads with `load(key)`; the script's own `store()`
    /// calls come back as `result.store_writes`.
    pub store: BTreeMap<String, Value>,
}

/// Failure to answer a call. (pi: `deliver()` throws for these cases.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliverError {
    /// The call id is not waiting for an answer.
    NotWaiting,
    /// The result number was already used.
    SeqUsed,
    /// The `now` value is not a finite millisecond count. Represented as
    /// `u64`, so this variant exists for completeness of the pi contract.
    NotFinite,
}
