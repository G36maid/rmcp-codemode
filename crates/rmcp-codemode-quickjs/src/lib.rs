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
//! Everything crosses the VM boundary as JSON strings through a single
//! dispatcher (`__callTool`), so neither side needs native object conversion:
//! the script's tool calls and return value are stringified inside the VM, and
//! results are parsed inside it. This keeps the bridge one function wide and
//! the result format identical to what a host sees.

#![deny(missing_docs)]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use rmcp_codemode::search::SearchIndex;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Promise,
    prelude::{Async as AsyncFn, Func},
};
use serde_json::{Value, json};

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

/// One output item from a script.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::exhaustive_enums)] // plain data enum
pub enum OutputItem {
    /// A text item from `text()`.
    Text(String),
    /// A console line (`console.log` and friends).
    Console(String),
    /// An image from `image()`: raw base64 payload and its MIME type.
    Image {
        /// The base64-encoded image bytes.
        data: String,
        /// The image's MIME type, e.g. `image/png`.
        mime_type: String,
    },
}

/// Output side-channel: the items plus a running character total, so the
/// output caps stay O(1) per call.
#[derive(Default)]
struct OutputBuffer {
    items: Vec<OutputItem>,
    chars: usize,
}

impl OutputBuffer {
    fn push(&mut self, item: OutputItem) {
        self.chars += len_of(&item);
        self.items.push(item);
    }
}

/// One tool call recorded during execution.
#[derive(Debug, Clone)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct ToolCall {
    /// The tool's registry path, as the script named it.
    pub name: String,
    /// Wall time of the upstream call in milliseconds.
    pub duration_ms: u128,
}

/// The script's result.
#[derive(Debug, Default)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct ScriptResult {
    /// Whether the script completed without throwing.
    pub ok: bool,
    /// The script's error message when [`ScriptResult::ok`] is false: a
    /// thrown error, a rejected tool call, or a parse failure.
    pub error: Option<String>,
    /// The script's return value (JSON), when the script completed.
    pub value: Option<Value>,
    /// Output items, in the order the script produced them.
    pub output: Vec<OutputItem>,
    /// Tool calls made by the script, in order.
    pub calls: Vec<ToolCall>,
    /// Wall time of the whole script, in milliseconds.
    pub duration_ms: u128,
    /// The output budget in model tokens from the script's `@options` line
    /// (default [`DEFAULT_MAX_OUTPUT_TOKENS`]); applied by
    /// [`render_result_text`].
    pub max_output_tokens: u64,
}

/// Default output budget in model tokens (pi's default).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;

/// Hard cap on total output characters across `text()` and `console` calls
/// (pi's cap). A script that pushes past it fails.
pub const MAX_OUTPUT_CHARS: usize = 16_777_216;

/// Hard cap on `text()` and `console` calls (pi's cap).
pub const MAX_OUTPUT_CALLS: usize = 100_000;

/// Characters per estimated model token, used to apply
/// [`ScriptResult::max_output_tokens`] to rendered text.
const CHARS_PER_TOKEN: usize = 4;

/// The script-visible store: small JSON values kept across script runs.
/// pi's semantics: writes buffer during the run and commit only when the
/// script succeeds, so a failed script leaves no partial writes.
pub type Store = Arc<Mutex<BTreeMap<String, Value>>>;

/// Maximum characters of JSON for one stored value (pi's cap).
pub const STORE_MAX_VALUE_CHARS: usize = 262_144;

/// Maximum characters of JSON across all stored values (pi's cap).
pub const STORE_MAX_TOTAL_CHARS: usize = 1_048_576;

/// Parsed `@options` line from the first line of a script (pi's semantics:
/// the options line is optional, must be the first line, and is validated
/// before the script runs).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct ScriptOptions {
    /// Output budget in model tokens.
    pub max_output_tokens: Option<u64>,
    /// Whole-script deadline in milliseconds; overrides the host's default.
    pub timeout_ms: Option<u64>,
}

/// Split off and parse a leading `// @options: {...}` line. Returns the
/// (unchanged) source and the parse outcome; an options line anywhere but
/// the first line is an error, as is malformed JSON or a bad value.
fn extract_options(source: &str) -> (String, Result<ScriptOptions, String>) {
    let mut options = ScriptOptions::default();
    let mut lines = source.lines();
    if let Some(rest) = lines
        .next()
        .and_then(|first| first.trim().strip_prefix("// @options:"))
    {
        let parsed: Value = match serde_json::from_str(rest.trim()) {
            Ok(value) => value,
            Err(e) => {
                return (
                    source.to_owned(),
                    Err(format!("invalid @options line: {e}")),
                );
            }
        };
        let Some(object) = parsed.as_object() else {
            return (
                source.to_owned(),
                Err("invalid @options line: expected a JSON object".to_owned()),
            );
        };
        for (key, value) in object {
            match key.as_str() {
                "max_output_tokens" => match value.as_u64() {
                    Some(n) => options.max_output_tokens = Some(n),
                    None => {
                        return (
                                source.to_owned(),
                                Err("invalid @options line: max_output_tokens must be a non-negative integer".to_owned()),
                            );
                    }
                },
                "timeout_ms" => match value.as_u64() {
                    Some(n) => options.timeout_ms = Some(n),
                    None => {
                        return (
                            source.to_owned(),
                            Err(
                                "invalid @options line: timeout_ms must be a non-negative integer"
                                    .to_owned(),
                            ),
                        );
                    }
                },
                other => {
                    return (
                        source.to_owned(),
                        Err(format!("invalid @options line: unknown option '{other}'")),
                    );
                }
            }
        }
    }
    if source
        .lines()
        .skip(1)
        .any(|line| line.contains("// @options:"))
    {
        return (
            source.to_owned(),
            Err("the @options line must be the first line".to_owned()),
        );
    }
    (source.to_owned(), Ok(options))
}

/// The sandbox: one QuickJS runtime, reused across script executions.
pub struct Sandbox {
    runtime: AsyncRuntime,
    context: AsyncContext,
    /// Namespace descriptions served to `describeNamespace`; set by the host
    /// (an aggregator knows its upstreams' descriptions).
    namespace_descriptions: Arc<BTreeMap<String, String>>,
}

/// Default VM heap limit (pi's limit): 256 MB.
const DEFAULT_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

impl Sandbox {
    /// Create a sandbox.
    pub async fn new() -> Result<Self, SandboxError> {
        Self::with_memory_limit(DEFAULT_MEMORY_LIMIT).await
    }

    /// Create a sandbox with an explicit VM heap limit in bytes. An allocation
    /// past the limit throws `InternalError: out of memory` inside the script.
    pub async fn with_memory_limit(limit: usize) -> Result<Self, SandboxError> {
        let runtime = AsyncRuntime::new().map_err(|e| SandboxError::Engine(e.to_string()))?;
        runtime.set_memory_limit(limit).await;
        runtime.set_max_stack_size(1024 * 1024).await;
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|e| SandboxError::Engine(e.to_string()))?;
        Ok(Self {
            runtime,
            context,
            namespace_descriptions: Arc::default(),
        })
    }

    /// Set the namespace descriptions served to `describeNamespace` (pi's
    /// namespace-level `description` field).
    pub fn set_namespace_descriptions(&mut self, descriptions: BTreeMap<String, String>) {
        self.namespace_descriptions = Arc::new(descriptions);
    }

    /// Execute a script.
    ///
    /// `deadline` is a hard wall-clock limit for the whole script, including
    /// upstream tool calls; a script may extend or replace it with its own
    /// `// @options: {"timeout_ms": ...}` first line (pi's semantics). The
    /// deadline preempts pure-JS busy loops via the VM's interrupt handler.
    /// `None` means no deadline.
    pub async fn execute(
        &self,
        source: &str,
        tools: Vec<SandboxTool>,
        deadline: Option<std::time::Duration>,
    ) -> Result<ScriptResult, SandboxError> {
        self.execute_with_store(source, tools, deadline, None).await
    }

    /// Execute a script with a persistent store.
    ///
    /// The store keeps small JSON values across script runs (pi's
    /// `store`/`load`): the script's `store(key, value)` writes buffer during
    /// the run and commit only when the script succeeds, so a failed script
    /// leaves no partial writes. The same handle passed on the next run sees
    /// the committed values.
    pub async fn execute_with_store(
        &self,
        source: &str,
        tools: Vec<SandboxTool>,
        deadline: Option<std::time::Duration>,
        store: Option<&Store>,
    ) -> Result<ScriptResult, SandboxError> {
        let started = Instant::now();
        let (source, options) = extract_options(source);
        let options = match options {
            Ok(options) => options,
            Err(message) => {
                return Ok(ScriptResult {
                    ok: false,
                    error: Some(message),
                    duration_ms: started.elapsed().as_millis(),
                    max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                    ..ScriptResult::default()
                });
            }
        };
        // The script's own options line wins over the host default (pi's
        // semantics: the options line is the script's voice).
        let deadline = options
            .timeout_ms
            .map(std::time::Duration::from_millis)
            .or(deadline);
        let deadline_instant = deadline.map(|d| started + d);

        let output: Arc<Mutex<OutputBuffer>> = Arc::new(Mutex::new(OutputBuffer::default()));
        let calls: Arc<Mutex<Vec<ToolCall>>> = Arc::new(Mutex::new(Vec::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let exit_requested = Arc::new(AtomicBool::new(false));
        // Store writes buffer here during the run and commit on success.
        let pending_store: Arc<Mutex<BTreeMap<String, Value>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        // The interrupt handler gives the host two hard stops inside pure-JS
        // execution: the deadline, and a script's own exit() that a try/catch
        // swallowed.
        {
            let exit_requested = Arc::clone(&exit_requested);
            self.runtime
                .set_interrupt_handler(Some(Box::new(move || {
                    exit_requested.load(Ordering::Relaxed)
                        || deadline_instant.is_some_and(|d| Instant::now() >= d)
                })))
                .await;
        }

        let mut tool_map: BTreeMap<String, SandboxTool> = BTreeMap::new();
        for tool in tools {
            if tool_map.contains_key(&tool.name) {
                return Err(SandboxError::Engine(format!(
                    "duplicate tool path `{}`",
                    tool.name
                )));
            }
            tool_map.insert(tool.name.clone(), tool);
        }
        let tool_map = Arc::new(tool_map);

        let output_arc = Arc::clone(&output);
        let calls_arc = Arc::clone(&calls);
        let tool_map_arc = Arc::clone(&tool_map);
        let in_flight_arc = Arc::clone(&in_flight);
        let exit_flag = Arc::clone(&exit_requested);
        let pending_store_arc = Arc::clone(&pending_store);
        let store_arc = store.cloned();
        let namespace_descriptions_arc = Arc::clone(&self.namespace_descriptions);
        // The deadline covers the whole execution — the script's own awaits
        // and upstream tool calls included (the interrupt handler handles
        // pure-JS loops; this backstop covers hung upstream calls).
        let run = async {
            let envelope: Value = self
                .context
                .async_with(async |ctx| {
                    run_script(
                        ctx,
                        &source,
                        output_arc,
                        calls_arc,
                        tool_map_arc,
                        in_flight_arc,
                        exit_flag,
                        deadline_instant,
                        pending_store_arc,
                        store_arc,
                        namespace_descriptions_arc,
                    )
                    .await
                })
                .await?;
            self.runtime.idle().await;
            Ok::<Value, SandboxError>(envelope)
        };
        let envelope = match deadline {
            Some(deadline) => tokio::time::timeout(deadline, run)
                .await
                .map_err(|_| SandboxError::Script("script exceeded its deadline".into()))??,
            None => run.await?,
        };
        let mut result = finalize(envelope, &output, &calls);
        // pi's semantics: store writes commit only when the script succeeds.
        if let Some(store) = store
            .filter(|_| result.ok)
            .filter(|_| !pending_store.lock().unwrap().is_empty())
        {
            let mut pending = pending_store.lock().unwrap();
            let mut committed = store.lock().unwrap();
            for (key, value) in std::mem::take(&mut *pending) {
                // Storing `undefined` deletes the key.
                if value.is_null() {
                    committed.remove(&key);
                } else {
                    committed.insert(key, value);
                }
            }
        }
        // A swallowed exit() ends the script successfully with no value, even
        // if the script kept running pure-JS code and returned (pi: "ends the
        // script successfully on exit()").
        if exit_requested.load(Ordering::Relaxed) {
            result.ok = true;
            result.error = None;
            result.value = None;
        }
        result.duration_ms = started.elapsed().as_millis();
        result.max_output_tokens = options
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        Ok(result)
    }
}

/// Bind globals, run the script, and return its result envelope.
#[allow(clippy::too_many_arguments)] // bridge wiring, all needed
async fn run_script<'js>(
    ctx: rquickjs::Ctx<'js>,
    source: &str,
    output: Arc<Mutex<OutputBuffer>>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    tool_map: Arc<BTreeMap<String, SandboxTool>>,
    in_flight: Arc<AtomicUsize>,
    exit_requested: Arc<AtomicBool>,
    deadline_instant: Option<Instant>,
    pending_store: Arc<Mutex<BTreeMap<String, Value>>>,
    committed_store: Option<Store>,
    namespace_descriptions: Arc<BTreeMap<String, String>>,
) -> Result<Value, SandboxError> {
    setup_globals(
        &ctx,
        Arc::clone(&output),
        Arc::clone(&calls),
        Arc::clone(&tool_map),
        Arc::clone(&in_flight),
        Arc::clone(&exit_requested),
        pending_store,
        committed_store,
        namespace_descriptions,
    )?;
    // The script is the body of an async function. The IIFE's result crosses
    // as a JSON envelope string so neither side needs native value conversion.
    // A thrown `{ __codemode_exit: true }` sentinel (from `exit()`) resolves
    // the script successfully with no value.
    let wrapped = format!(
        "(async () => {{\n{source}\n}})().then(\n\x20 (v) => JSON.stringify({{ ok: true, value: v === undefined ? null : v }}),\n\x20 (e) => e && e.__codemode_exit\n\x20   ? JSON.stringify({{ ok: true, value: null }})\n\x20   : JSON.stringify({{ ok: false, error: e && e.message !== undefined ? String(e.message) : String(e) }})\n )"
    );
    // A parse error or an unsettled promise surfaces as a failed result, not
    // an Err: pi reports script failures through the result, and partial
    // output (text before the failure) is kept.
    let promise: Promise = match ctx.eval(wrapped.as_bytes()).catch(&ctx) {
        Ok(promise) => promise,
        Err(caught) => {
            return Ok(classified_envelope(
                &ctx,
                describe_caught(&ctx, caught),
                &exit_requested,
                deadline_instant,
            ));
        }
    };
    // Poll the result promise in short ticks so a script that waits on a
    // promise nothing can settle (pi's semantics: no timers here, so that is
    // any promise with no tool call pending) fails immediately instead of
    // hanging until a deadline that may not exist.
    let mut future = Box::pin(promise.into_future::<String>());
    let tick = std::time::Duration::from_millis(25);
    let mut idle_ticks = 0u32;
    let envelope: String = loop {
        match tokio::time::timeout(tick, future.as_mut()).await {
            Ok(Ok(envelope)) => break envelope,
            Ok(Err(err)) => {
                return Ok(classified_envelope(
                    &ctx,
                    describe_error(&ctx, &err),
                    &exit_requested,
                    deadline_instant,
                ));
            }
            Err(_) => {
                if exit_requested.load(Ordering::Relaxed) {
                    return Ok(failed_envelope(
                        "script called exit() but never finished".into(),
                    ));
                }
                if in_flight.load(Ordering::Relaxed) == 0 {
                    idle_ticks += 1;
                    if idle_ticks >= 3 {
                        return Ok(failed_envelope(
                            "script waits on a promise that can never settle (no timers; no tool call pending)"
                                .into(),
                        ));
                    }
                } else {
                    idle_ticks = 0;
                }
            }
        }
    };

    serde_json::from_str(&envelope)
        .map_err(|e| SandboxError::Engine(format!("bad result envelope: {e}")))
}

/// Classify an error caught at the Rust boundary: an exit request resolves
/// the script successfully (pi: `exit()` ends the script successfully), a
/// deadline interruption reports the deadline, everything else is the
/// script's error.
fn classified_envelope(
    ctx: &rquickjs::Ctx<'_>,
    message: String,
    exit_requested: &AtomicBool,
    deadline_instant: Option<Instant>,
) -> Value {
    let _ = ctx;
    if exit_requested.load(Ordering::Relaxed) {
        return json!({ "ok": true, "value": null });
    }
    if deadline_instant.is_some_and(|d| Instant::now() >= d) {
        return failed_envelope("script exceeded its deadline".into());
    }
    failed_envelope(message)
}

/// An envelope for a script that failed before producing one.
fn failed_envelope(message: String) -> Value {
    serde_json::json!({ "ok": false, "error": message })
}

/// Build the final [`ScriptResult`] from the result envelope and side channels.
fn finalize(
    envelope: Value,
    output: &Mutex<OutputBuffer>,
    calls: &Mutex<Vec<ToolCall>>,
) -> ScriptResult {
    let ok = envelope.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let value = envelope.get("value").cloned().filter(|v| !v.is_null());
    let error = envelope
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|_| !ok);
    ScriptResult {
        ok,
        error,
        value,
        output: std::mem::take(&mut output.lock().unwrap().items),
        calls: std::mem::take(&mut *calls.lock().unwrap()),
        ..ScriptResult::default()
    }
}

/// Describe a caught error, preferring the thrown JS value's message.
fn describe_caught(_ctx: &rquickjs::Ctx<'_>, caught: rquickjs::CaughtError<'_>) -> String {
    match caught {
        rquickjs::CaughtError::Exception(exc) => {
            let message = exc.message().unwrap_or_default();
            if message.is_empty() {
                "script threw".into()
            } else {
                message
            }
        }
        rquickjs::CaughtError::Value(value) => {
            if let Some(exc) = value
                .as_object()
                .and_then(|o| rquickjs::Exception::from_object(o.clone()))
            {
                exc.message().unwrap_or_else(|| "script threw".into())
            } else {
                format!("script threw {}", js_value_debug(&value))
            }
        }
        rquickjs::CaughtError::Error(err) => err.to_string(),
    }
}

/// Describe a plain rquickjs error, pulling the pending exception if present.
#[allow(dead_code)]
fn describe_error(ctx: &rquickjs::Ctx<'_>, err: &rquickjs::Error) -> String {
    if matches!(err, rquickjs::Error::Exception) {
        let thrown = ctx.catch();
        return describe_caught(ctx, rquickjs::CaughtError::Value(thrown));
    }
    err.to_string()
}

/// Best-effort rendering of a JS value for error messages.
fn js_value_debug(value: &rquickjs::Value<'_>) -> String {
    match value.as_string() {
        Some(s) => s.to_string().unwrap_or_else(|_| "string".into()),
        None => match value.type_of() {
            rquickjs::Type::Undefined => "undefined".into(),
            rquickjs::Type::Null => "null".into(),
            _ => "value".into(),
        },
    }
}

/// Bind `__emitText`, `__emitConsole`, `__emitImage`, `__storeValue`,
/// `__loadValue`, `__callTool`, `__searchTools`, `__describeTool`, and
/// `__describeNamespace`, and the script-facing globals (`tools`, `text`,
/// `console`, `image`, `store`, `load`, `ALL_TOOLS`, `searchTools`,
/// `describeTool`, `describeNamespace`, `exit`).
#[allow(clippy::too_many_arguments)] // bridge wiring, all needed
fn setup_globals<'js>(
    ctx: &rquickjs::Ctx<'js>,
    output: Arc<Mutex<OutputBuffer>>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    tools: Arc<BTreeMap<String, SandboxTool>>,
    in_flight: Arc<AtomicUsize>,
    exit_requested: Arc<AtomicBool>,
    pending_store: Arc<Mutex<BTreeMap<String, Value>>>,
    committed_store: Option<Store>,
    namespace_descriptions: Arc<BTreeMap<String, String>>,
) -> Result<(), SandboxError> {
    let globals = ctx.globals();

    // ---- output emitters: return an error message (empty = ok) when an
    // output cap is hit or the script has exited; the JS leaf throws it. ----
    {
        let output = Arc::clone(&output);
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__emitText",
                Func::from(move |text: String| -> String {
                    if exit_requested.load(Ordering::Relaxed) {
                        return "script has exited".to_owned();
                    }
                    let mut guard = output.lock().unwrap();
                    if guard.items.len() >= MAX_OUTPUT_CALLS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CALLS} text/console calls"
                        );
                    }
                    if guard.chars + text.len() > MAX_OUTPUT_CHARS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CHARS} characters of output"
                        );
                    }
                    guard.push(OutputItem::Text(text));
                    String::new()
                }),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let output = Arc::clone(&output);
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__emitConsole",
                Func::from(move |line: String| -> String {
                    if exit_requested.load(Ordering::Relaxed) {
                        return "script has exited".to_owned();
                    }
                    let mut guard = output.lock().unwrap();
                    if guard.items.len() >= MAX_OUTPUT_CALLS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CALLS} text/console calls"
                        );
                    }
                    if guard.chars + line.len() > MAX_OUTPUT_CHARS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CHARS} characters of output"
                        );
                    }
                    guard.push(OutputItem::Console(line));
                    String::new()
                }),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- tool dispatcher: (path, argsJson) -> envelope JSON string; the JS
    // leaf throws `new Error(envelope.error)` when the call failed, so the
    // script sees clean messages (pi's semantics). Lookup order: exact path,
    // then normalized identifier aliases (pi's `my-tool` ↔ `tools.my_tool`),
    // then a close-match suggestion (pi's "Did you mean"). ----
    {
        let calls = Arc::clone(&calls);
        let tools = Arc::clone(&tools);
        let in_flight = Arc::clone(&in_flight);
        let exit_requested = Arc::clone(&exit_requested);
        let call_tool = move |path: String, args_json: String| {
            let calls = Arc::clone(&calls);
            let tools = Arc::clone(&tools);
            let in_flight = Arc::clone(&in_flight);
            let exit_requested = Arc::clone(&exit_requested);
            async move {
                let dispatch = async |calls: Arc<Mutex<Vec<ToolCall>>>,
                                      tools: Arc<BTreeMap<String, SandboxTool>>,
                                      in_flight: Arc<AtomicUsize>| {
                    if exit_requested.load(Ordering::Relaxed) {
                        return Ok::<String, rquickjs::Error>(
                            json!({ "ok": false, "error": "script has exited" }).to_string(),
                        );
                    }
                    // Identifier aliases, first registration wins on collision
                    // (pi's semantics: `tools.my_tool` reaches `my-tool`).
                    let canonical = if tools.contains_key(&path) {
                        Some(path.clone())
                    } else {
                        tools
                            .keys()
                            .find(|known| normalize_identifier(known) == path)
                            .cloned()
                    };
                    let Some(tool_path) = canonical else {
                        return Ok::<String, rquickjs::Error>(
                            json!({ "ok": false, "error": unknown_tool_message(&path, &tools) })
                                .to_string(),
                        );
                    };
                    let path = tool_path;
                    let Some(tool) = tools.get(&path) else {
                        return Ok(
                            json!({ "ok": false, "error": unknown_tool_message(&path, &tools) })
                                .to_string(),
                        );
                    };
                    // Validate the argument shape only when the script sent one;
                    // `JSON.stringify(undefined)` arrives as the string "undefined".
                    if args_json != "null" && serde_json::from_str::<Value>(&args_json).is_err() {
                        return Ok(json!({ "ok": false, "error": format!("Invalid arguments for tool '{path}': not valid JSON") }).to_string());
                    }
                    // The in-flight counter powers never-settling detection:
                    // while any upstream call is pending, waiting is legal.
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    let _guard = InFlightGuard(&in_flight);
                    let started = Instant::now();
                    let result = (tool.execute)(args_json.clone()).await;
                    calls.lock().unwrap().push(ToolCall {
                        name: path.clone(),
                        duration_ms: started.elapsed().as_millis(),
                    });
                    Ok(match result {
                        Ok(value) => match serde_json::from_str::<Value>(&value) {
                            Ok(parsed) => json!({ "ok": true, "value": parsed }).to_string(),
                            Err(e) => json!({ "ok": false, "error": format!("Tool '{path}' returned invalid JSON: {e}") }).to_string(),
                        },
                        Err(message) => json!({ "ok": false, "error": format!("{path}: {message}") }).to_string(),
                    })
                };
                dispatch(calls, tools, in_flight).await
            }
        };
        globals
            .set("__callTool", Func::from(AsyncFn(call_tool)))
            .map_err(rquickjs_error)?;
    }

    // ---- image emitter: parses a data URI (`data:<mime>;base64,<data>`) or
    // bare base64 (assumed PNG) into an image output item. ----
    {
        let output = Arc::clone(&output);
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__emitImage",
                Func::from(move |value: String| -> String {
                    if exit_requested.load(Ordering::Relaxed) {
                        return "script has exited".to_owned();
                    }
                    let Some((mime_type, data)) = parse_image_input(&value) else {
                        return "image() expects a base64 data URI like data:image/png;base64,...".to_owned();
                    };
                    let mut guard = output.lock().unwrap();
                    if guard.items.len() >= MAX_OUTPUT_CALLS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CALLS} output calls"
                        );
                    }
                    if guard.chars + data.len() > MAX_OUTPUT_CHARS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CHARS} characters of output"
                        );
                    }
                    guard.push(OutputItem::Image { data, mime_type });
                    String::new()
                }),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- store bridges: writes buffer in `pending_store` (committed on
    // script success); reads see pending writes first. ----
    {
        let pending_store = Arc::clone(&pending_store);
        let committed_store = committed_store.clone();
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__storeValue",
                Func::from(
                    move |key: String, value_json: String, is_delete: bool| -> String {
                        if exit_requested.load(Ordering::Relaxed) {
                            return "script has exited".to_owned();
                        }
                        let mut pending = pending_store.lock().unwrap();
                        if is_delete {
                            // Storing `undefined` deletes the key.
                            pending.insert(key, Value::Null);
                            return String::new();
                        }
                        if value_json.len() > STORE_MAX_VALUE_CHARS {
                            return format!(
                                "store limit exceeded: one value may hold at most {STORE_MAX_VALUE_CHARS} characters of JSON"
                            );
                        }
                        let value: Value = match serde_json::from_str(&value_json) {
                            Ok(value) => value,
                            Err(e) => return format!("store() failed to encode the value: {e}"),
                        };
                        let committed_chars = committed_store
                            .as_ref()
                            .map(|store| {
                                store
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .map(|v| serde_json::to_string(v).map_or(0, |s| s.len()))
                                    .sum::<usize>()
                            })
                            .unwrap_or(0);
                        let pending_chars = pending
                            .values()
                            .map(|v| serde_json::to_string(v).map_or(0, |s| s.len()))
                            .sum::<usize>();
                        if committed_chars + pending_chars > STORE_MAX_TOTAL_CHARS {
                            return format!(
                                "store limit exceeded: all values together may hold at most {STORE_MAX_TOTAL_CHARS} characters of JSON"
                            );
                        }
                        pending.insert(key, value);
                        String::new()
                    },
                ),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let pending_store = Arc::clone(&pending_store);
        let committed_store = committed_store.clone();
        globals
            .set(
                "__loadValue",
                Func::from(move |key: String| -> String {
                    if let Some(value) = pending_store.lock().unwrap().get(&key).cloned() {
                        return serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
                    }
                    if let Some(value) = committed_store
                        .as_ref()
                        .and_then(|store| store.lock().unwrap().get(&key).cloned())
                    {
                        return serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
                    }
                    "null".to_owned()
                }),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- discovery bridges: pure synchronous reads over the tool map, so
    // they are plain closures returning JSON strings. ----
    {
        let tools = Arc::clone(&tools);
        globals
            .set(
                "__searchTools",
                Func::from(move |query: String, opts_json: String| -> String {
                    search_tools_response(&tools, &query, &opts_json)
                }),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let tools = Arc::clone(&tools);
        globals
            .set(
                "__describeTool",
                Func::from(move |name: String| -> String { describe_tool_response(&tools, &name) }),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let tools = Arc::clone(&tools);
        let namespace_descriptions = Arc::clone(&namespace_descriptions);
        globals
            .set(
                "__describeNamespace",
                Func::from(move |name: String| -> String {
                    describe_namespace_response(&tools, &namespace_descriptions, &name)
                }),
            )
            .map_err(rquickjs_error)?;
    }

    {
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__requestExit",
                Func::from(move || exit_requested.store(true, Ordering::Relaxed)),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- JS-side setup: console, text, tools tree, ALL_TOOLS ----
    let all_tools: Vec<Value> = tools
        .values()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description.clone().unwrap_or_default(),
            })
        })
        .collect();
    let all_tools_json = serde_json::to_string(&all_tools)
        .map_err(|e| SandboxError::Engine(format!("serialize tool catalog: {e}")))?;

    let setup = format!(
        r#"
globalThis.__leafPaths = new Set(JSON.parse({tools_spec_literal}));
globalThis.ALL_TOOLS = JSON.parse({all_tools_json});

globalThis.__makeLeaf = (path) => async (args) => {{
    const envelope = JSON.parse(await globalThis.__callTool(path, JSON.stringify(args === undefined ? null : args)));
    if (!envelope.ok) throw new Error(envelope.error);
    return envelope.value;
}};
globalThis.__isNamespace = (prefix) => {{
    for (const path of globalThis.__leafPaths) if (path.startsWith(prefix + ".")) return true;
    return false;
}};
// Props that fall through to undefined unless a real tool owns the name, so
// `tools.toString` is undefined and JSON.stringify(tools) stays {{}} while a
// tool named `constructor` or `__proto__` still resolves (OpenCode's
// "blocked member names" semantics: real paths always win).
globalThis.__reserved = new Set(["then", "toString", "valueOf", "toJSON", "call", "apply", "bind"]);
// The top-level `tools` is an object (pi: JSON.stringify(tools) stays an
// empty object);
// nested namespaces are callable proxies (OpenCode: a namespace without its
// own tool is not callable).
globalThis.__resolvePath = (path) => {{
    if (globalThis.__leafPaths.has(path)) return __makeLeaf(path);
    if (__isNamespace(path)) return __makeNamespace(path);
    return __makeLeaf(path); // unknown leaf: __callTool reports it with suggestions
}};
globalThis.__childSegments = (prefix) => {{
    const out = new Set();
    const at = prefix === "" ? "" : prefix + ".";
    for (const path of globalThis.__leafPaths) {{
        if (!path.startsWith(at)) continue;
        const rest = path.slice(at.length);
        out.add(rest.split(".")[0]);
    }}
    return out;
}};
globalThis.__memberPath = (prefix, prop) => (prefix ? prefix + "." + prop : prop);
globalThis.__objectHandler = (prefix) => ({{
        get(_target, prop) {{
            if (typeof prop !== "string") return undefined;
            if (__reserved.has(prop) && !globalThis.__leafPaths.has(__memberPath(prefix, prop))) return undefined;
            return __resolvePath(__memberPath(prefix, prop));
        }},
        has(_target, prop) {{
            if (typeof prop !== "string") return Reflect.has(_target, prop);
            const path = __memberPath(prefix, prop);
            return globalThis.__leafPaths.has(path) || __isNamespace(path) || Reflect.has(_target, prop);
        }},
        ownKeys(target) {{
            return [...__childSegments(prefix), ...Reflect.ownKeys(target)];
        }},
        getOwnPropertyDescriptor(target, prop) {{
            if (typeof prop === "string" && __childSegments(prefix).has(prop)) {{
                return {{ value: __resolvePath(__memberPath(prefix, prop)), writable: true, enumerable: true, configurable: true }};
            }}
            return Reflect.getOwnPropertyDescriptor(target, prop);
        }},
    }});
globalThis.__makeObjectNamespace = (prefix) => new Proxy({{}}, __objectHandler(prefix));
globalThis.tools = __makeObjectNamespace("");
globalThis.__makeNamespace = function (prefix) {{
    return new Proxy(function () {{}}, {{
        ...__objectHandler(prefix),
        apply(_target, _this, args) {{
            if (globalThis.__leafPaths.has(prefix)) return __makeLeaf(prefix).apply(null, args);
            throw new Error(`Tool '${{prefix}}' is not callable`);
        }},
    }});
}};

globalThis.text = (value) => {{
    const err = globalThis.__emitText(typeof value === "string" ? value : JSON.stringify(value));
    if (err) throw new Error(err);
}};
globalThis.console = {{
    log: (...parts) => {{
        const err = globalThis.__emitConsole(parts.map((p) => (typeof p === "string" ? p : JSON.stringify(p))).join(" "));
        if (err) throw new Error(err);
    }},
}};
globalThis.console.info = globalThis.console.log;
globalThis.console.warn = globalThis.console.log;
globalThis.console.error = globalThis.console.log;
globalThis.console.debug = globalThis.console.log;
// image() adds an image output item from a base64 data URI.
globalThis.image = (value) => {{
    const err = globalThis.__emitImage(typeof value === "string" ? value : "");
    if (err) throw new Error(err);
}};
// store(key, value) keeps a JSON value for later codemode calls; storing
// undefined deletes the key. Writes commit only when the script succeeds.
globalThis.store = (key, value) => {{
    const err = globalThis.__storeValue(
        String(key),
        value === undefined ? "null" : JSON.stringify(value),
        value === undefined,
    );
    if (err) throw new Error(err);
}};
globalThis.load = (key) => {{
    const raw = globalThis.__loadValue(String(key));
    return raw === "null" ? undefined : JSON.parse(raw);
}};
// exit() ends the script successfully: the sentinel unwinds the IIFE (the
// rejection handler maps it to a successful envelope); if a try/catch
// swallows it, the exit flag stops every host call afterward and the VM's
// interrupt handler aborts at the next loop or function entry.
globalThis.exit = () => {{
    globalThis.__requestExit();
    throw {{ __codemode_exit: true }};
}};
globalThis.searchTools = async (query, options) =>
    JSON.parse(globalThis.__searchTools(query === undefined ? "" : String(query), JSON.stringify(options ?? {{}})));
globalThis.describeTool = async (name) => {{
    const raw = globalThis.__describeTool(String(name));
    return raw === "null" ? undefined : JSON.parse(raw);
}};
globalThis.describeNamespace = async (name) => {{
    const raw = globalThis.__describeNamespace(String(name));
    return raw === "null" ? undefined : JSON.parse(raw);
}};
"#,
        tools_spec_literal = serde_json::to_string(
            &serde_json::to_string(&tools.keys().cloned().collect::<Vec<_>>())
                .map_err(|e| SandboxError::Engine(format!("serialize tool spec: {e}")))?
        )
        .map_err(|e| SandboxError::Engine(format!("serialize tool spec literal: {e}")))?,
        all_tools_json = serde_json::to_string(&all_tools_json)
            .map_err(|e| SandboxError::Engine(format!("serialize tool catalog literal: {e}")))?,
    );
    ctx.eval::<(), _>(setup.as_bytes())
        .catch(ctx)
        .map_err(|caught| SandboxError::Script(describe_caught(ctx, caught)))?;
    Ok(())
}

/// Decrement the in-flight tool-call counter when the dispatch future ends,
/// however it ends.
struct InFlightGuard<'a>(&'a AtomicUsize);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Total characters of an output list, for the output caps.
fn len_of(item: &OutputItem) -> usize {
    match item {
        OutputItem::Text(text) | OutputItem::Console(text) => text.len(),
        OutputItem::Image { data, .. } => data.len(),
    }
}

/// Parse an `image()` argument: a `data:<mime>;base64,<data>` URI, or a bare
/// base64 payload (assumed PNG).
fn parse_image_input(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("data:") {
        let (metadata, data) = rest.split_once(",")?;
        let mime_type = metadata
            .strip_suffix(";base64")
            .filter(|m| !m.is_empty())?
            .to_owned();
        if data.is_empty() {
            return None;
        }
        Some((mime_type, data.to_owned()))
    } else if !value.is_empty() {
        Some(("image/png".to_owned(), value.to_owned()))
    } else {
        None
    }
}

/// Resolve a script-supplied tool name to a canonical path: the exact path,
/// then a normalized identifier alias (pi's `tools.my_tool` -> `my-tool`).
fn canonical_tool_path(name: &str, tools: &BTreeMap<String, SandboxTool>) -> Option<String> {
    if tools.contains_key(name) {
        return Some(name.to_owned());
    }
    tools
        .keys()
        .find(|known| normalize_identifier(known) == name)
        .cloned()
}

/// The `searchTools(query, { limit?, namespace? })` response: `{ name,
/// description }[]` ranked by BM25 (pi's default limit 8). An empty query
/// returns tools in path order, so scripts can browse a namespace.
fn search_tools_response(
    tools: &BTreeMap<String, SandboxTool>,
    query: &str,
    opts_json: &str,
) -> String {
    let options: Value = serde_json::from_str(opts_json).unwrap_or(json!({}));
    let limit = options
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(8);
    let namespace = options.get("namespace").and_then(Value::as_str);
    let candidates: Vec<(&String, &SandboxTool)> = tools
        .iter()
        .filter(|(path, _)| {
            namespace.is_none_or(|ns| path.starts_with(ns) && path[ns.len()..].starts_with('.'))
        })
        .collect();
    if candidates.is_empty() {
        return "[]".to_owned();
    }
    let index = SearchIndex::build(candidates.iter().map(|(path, tool)| {
        (
            (*path).clone(),
            tool.description.clone().unwrap_or_default(),
            Vec::new(),
        )
    }));
    let hits = index.search(query, limit);
    let hits_json: Vec<Value> = hits
        .iter()
        .map(|hit| json!({ "name": hit.name, "description": hit.description }))
        .collect();
    serde_json::to_string(&hits_json).unwrap_or_else(|_| "[]".to_owned())
}

/// The `describeTool(name)` response: `{ name, description, declaration }` or
/// `null` when the tool is unknown (the JS leaf maps `null` to `undefined`).
fn describe_tool_response(tools: &BTreeMap<String, SandboxTool>, name: &str) -> String {
    let Some(path) = canonical_tool_path(name, tools) else {
        return "null".to_owned();
    };
    let tool = &tools[&path];
    let response = json!({
        "name": path,
        "description": tool.description.clone().unwrap_or_default(),
        "declaration": ts_declaration(
            &path,
            tool.description.as_deref(),
            tool.input_schema.as_ref(),
        ),
    });
    serde_json::to_string(&response).unwrap_or_else(|_| "null".to_owned())
}

/// The `describeNamespace(name)` response: `{ name, description?, tools:
/// [...] }` for a path prefix, or `null` when no tool lives under it. The
/// namespace description comes from the host's registry when known.
fn describe_namespace_response(
    tools: &BTreeMap<String, SandboxTool>,
    namespace_descriptions: &BTreeMap<String, String>,
    name: &str,
) -> String {
    let prefix = format!("{name}.");
    let members: Vec<&SandboxTool> = tools
        .iter()
        .filter(|(path, _)| path.starts_with(&prefix))
        .map(|(_, tool)| tool)
        .collect();
    if members.is_empty() {
        return "null".to_owned();
    }
    let tools_json: Vec<Value> = members
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description.clone().unwrap_or_default(),
                "declaration": ts_declaration(
                    &tool.name,
                    tool.description.as_deref(),
                    tool.input_schema.as_ref(),
                ),
            })
        })
        .collect();
    let mut response = serde_json::Map::new();
    response.insert("name".to_owned(), json!(name));
    if let Some(description) = namespace_descriptions.get(name) {
        response.insert("description".to_owned(), json!(description));
    }
    response.insert("tools".to_owned(), json!(tools_json));
    serde_json::to_string(&Value::Object(response)).unwrap_or_else(|_| "null".to_owned())
}

/// Render a tool's TypeScript declaration (pi's `describeTool` shape): a doc
/// comment with the description, and a function taking one typed `args`
/// object. Without a schema the argument type is an open record.
fn ts_declaration(name: &str, description: Option<&str>, schema: Option<&Value>) -> String {
    let callable = name.rsplit('.').next().unwrap_or(name);
    let callable = normalize_identifier(callable);
    let args_type = match schema {
        Some(schema) if schema.get("type").and_then(Value::as_str) == Some("object") => {
            ts_object_type(schema)
        }
        _ => "{ [k: string]: unknown }".to_owned(),
    };
    match description {
        Some(description) if !description.is_empty() => {
            format!(
                "/** {description} */\ndeclare function {callable}(args: {args_type}): Promise<unknown>;"
            )
        }
        _ => format!("declare function {callable}(args: {args_type}): Promise<unknown>;"),
    }
}

/// Render a JSON Schema object type as a TypeScript object type.
fn ts_object_type(schema: &Value) -> String {
    let properties = match schema.get("properties").and_then(Value::as_object) {
        Some(properties) if !properties.is_empty() => properties,
        _ => return "{ [k: string]: unknown }".to_owned(),
    };
    let members = ts_members(properties, schema);
    if members.is_empty() {
        "{ [k: string]: unknown }".to_owned()
    } else {
        format!("{{ {} }}", members.join(" "))
    }
}

/// Render the members of an object schema: `name: type;` with `?` markers
/// for properties absent from `required`, and doc comments when present.
fn ts_members(properties: &serde_json::Map<String, Value>, schema: &Value) -> Vec<String> {
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut members = Vec::new();
    for (property, property_schema) in properties {
        let optional = if required.contains(property) { "" } else { "?" };
        let member_type = ts_type(property_schema);
        let member = format!("{property}{optional}: {member_type};");
        match property_schema
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
        {
            Some(doc) => members.push(format!("/** {doc} */ {member}")),
            None => members.push(member),
        }
    }
    members
}

/// Render one JSON Schema value as a TypeScript type.
fn ts_type(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let literals: Vec<String> = values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned()))
            .collect();
        if !literals.is_empty() {
            return literals.join(" | ");
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "string".to_owned(),
        Some("number" | "integer") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => format!(
            "{}[]",
            schema
                .get("items")
                .map(ts_type)
                .unwrap_or_else(|| "unknown".to_owned())
        ),
        Some("object") => match schema.get("properties").and_then(Value::as_object) {
            Some(properties) if !properties.is_empty() => ts_object_type(schema),
            _ => "Record<string, unknown>".to_owned(),
        },
        _ => "unknown".to_owned(),
    }
}

/// Render a [`ScriptResult`] as the text a host hands back to the model, in
/// pi's codemode format: a `Script completed`/`Script failed` header with the
/// wall time, text items (from `text()` and the return value) in order under
/// `==> text N/M <==` markers when there are several, `console` lines in one
/// `<console_output>` block, and `Script error:` last for failures. Output is
/// truncated to the script's `max_output_tokens` budget, keeping its start
/// and end (pi truncates the same way, but also saves the full text to a
/// file; there is no file system here, so the full text is dropped).
pub fn render_result_text(result: &ScriptResult) -> String {
    let mut sections: Vec<String> = Vec::new();
    // Text and image items appear in output order (pi); the return value
    // joins as a final text item (pi: `return value` adds the value like
    // `text()`).
    let mut items: Vec<OutputItem> = result
        .output
        .iter()
        .filter(|item| !matches!(item, OutputItem::Console(_)))
        .cloned()
        .collect();
    if let Some(value) = &result.value {
        items.push(OutputItem::Text(
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()),
        ));
    }
    let text_count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Text(_)))
        .count();
    let image_count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Image { .. }))
        .count();
    let (mut text_index, mut image_index) = (0usize, 0usize);
    for item in &items {
        match item {
            OutputItem::Text(text) => {
                text_index += 1;
                if text_count > 1 {
                    sections.push(format!("==> text {text_index}/{text_count} <=="));
                }
                sections.push(text.clone());
            }
            OutputItem::Image { data, mime_type } => {
                image_index += 1;
                if image_count > 1 {
                    sections.push(format!("==> image {image_index}/{image_count} <=="));
                }
                sections.push(format!(
                    "[image: {mime_type}, {} bytes of base64]",
                    data.len()
                ));
            }
            OutputItem::Console(_) => unreachable!("filtered out above"),
        }
    }
    let console_lines: Vec<&String> = result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Console(line) => Some(line),
            OutputItem::Text(_) | OutputItem::Image { .. } => None,
        })
        .collect();
    if !console_lines.is_empty() {
        sections.push("<console_output>".to_owned());
        sections.extend(console_lines.iter().map(|line| (*line).clone()));
    }
    if let Some(error) = &result.error {
        sections.push(format!("Script error: {error}"));
    }
    let status = if result.ok { "completed" } else { "failed" };
    let rendered = format!(
        "Script {status} in {}ms\n{}",
        result.duration_ms,
        sections.join("\n")
    );
    truncate_to_budget(rendered, result.max_output_tokens)
}

/// Apply the `max_output_tokens` budget (estimated at 4 characters per
/// token): over-budget output keeps its start and end around a marker.
fn truncate_to_budget(rendered: String, max_output_tokens: u64) -> String {
    let budget = max_output_tokens.saturating_mul(CHARS_PER_TOKEN as u64) as usize;
    if rendered.len() <= budget {
        return rendered;
    }
    let marker = format!("\n... [output truncated: kept start and end; budget {budget} chars]\n");
    if budget <= marker.len() + 64 {
        return rendered.chars().take(budget).collect();
    }
    let keep = budget - marker.len();
    let head = keep - keep / 2;
    let tail = keep / 2;
    let head_text: String = rendered.chars().take(head).collect();
    let total_chars = rendered.chars().count();
    let tail_text: String = rendered.chars().skip(total_chars - tail).collect();
    format!("{head_text}{marker}{tail_text}")
}

/// Map a plain rquickjs error into a sandbox error.
fn rquickjs_error(err: rquickjs::Error) -> SandboxError {
    SandboxError::Engine(err.to_string())
}

/// Normalize a tool path into a valid JavaScript identifier (pi's
/// `toCodemodeIdentifier`): characters that are not valid in identifiers
/// become `_`, and a leading digit is prefixed so the result is a valid name.
/// Dots become `_`, so `github.list_issues` is also reachable as
/// `tools.github_list_issues`.
fn normalize_identifier(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    for ch in path.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// The error message for an unknown tool: a close-match suggestion when one
/// exists (pi: "Did you mean tools.web_search?"), otherwise an availability
/// list (pi: "Available: echo, web_search.").
fn unknown_tool_message(path: &str, tools: &BTreeMap<String, SandboxTool>) -> String {
    // Separator-insensitive identity first (pi: `websearch` suggests
    // `web-search`), then shared-token scoring.
    let squash = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let requested_squashed = squash(path);
    if let Some(known) = tools
        .keys()
        .find(|known| squash(known) == requested_squashed)
    {
        return format!("Unknown tool '{path}'. Did you mean '{known}'?");
    }
    let requested = tokenize_path(path);
    let mut best: Option<(&str, usize)> = None;
    for known in tools.keys() {
        let known_tokens = tokenize_path(known);
        let shared = known_tokens
            .iter()
            .filter(|t| requested.contains(t))
            .count();
        let score = shared * 2;
        if score > 0 && best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((known, score));
        }
    }
    match best {
        Some((known, _)) => format!("Unknown tool '{path}'. Did you mean '{known}'?"),
        None => {
            let available: Vec<&str> = tools.keys().take(3).map(String::as_str).collect();
            if available.is_empty() {
                format!("Unknown tool '{path}'. No tools are registered.")
            } else {
                format!(
                    "Unknown tool '{path}'. Available: {}.",
                    available.join(", ")
                )
            }
        }
    }
}

/// Split a tool path into lowercase word tokens: separators (`.`, `-`, `_`)
/// and camelCase boundaries both split, so `websearch` shares the token
/// `search` with `web-search` and `getZone` with `get_zone`.
fn tokenize_path(path: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in path.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && current.chars().last().is_some_and(|c| c.is_lowercase()) {
                tokens.push(std::mem::take(&mut current).to_lowercase());
            }
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// The `codemode` MCP tool: what a host sees.
///
/// One tool with a `script` parameter. Upstream tools are NOT listed to the
/// model — they are callable as `tools.<namespace>.<tool>(args)` inside the
/// script, discoverable via `ALL_TOOLS` (pi's semantics).
pub mod server {
    use std::{collections::BTreeMap, sync::Arc};

    use rmcp::{
        ErrorData as McpError,
        handler::server::ServerHandler,
        model::{
            CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
            ContentBlock, ListToolsResult, PaginatedRequestParams, ServerConfig, Tool,
        },
        service::{RequestContext, RoleServer},
    };
    use rmcp_codemode::exposure::ToolExposure;
    use serde_json::{Value, json};

    use super::{Sandbox, SandboxTool, Store, ToolFn};

    /// The single tool name exposed to hosts.
    pub const CODEMODE_TOOL_NAME: &str = "codemode";

    /// How many tool calls one script may make (safety bound).
    pub const DEFAULT_MAX_TOOL_CALLS: usize = 500;

    /// The inline budget for tool declarations in the `codemode` description,
    /// in estimated model tokens (pi's `codemode.inlineBudget`). Declarations
    /// beyond the budget are omitted; the model finds those tools with
    /// `searchTools` and `describeTool` inside the script.
    pub const INLINE_BUDGET_TOKENS: u64 = 3_000;

    /// How the server presents directly-exposed tools to the host.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[allow(clippy::exhaustive_enums)] // two fixed modes
    pub enum CodemodeMode {
        /// Direct tools stay in the host's `tools/list` and are additionally
        /// declared in the `codemode` description (pi's `codemode.mode:
        /// "on"`, the default).
        #[default]
        On,
        /// Direct tools are hidden from the host's `tools/list` and listed in
        /// the `codemode` description instead (pi's `codemode.mode: "only"`,
        /// OpenCode 2.0's default for MCP servers).
        Only,
    }

    /// An upstream tool dispatcher wired into the sandbox.
    #[allow(clippy::exhaustive_structs)] // user-constructed by design
    pub struct UpstreamTool {
        /// Registry path, e.g. `github.list_issues`.
        pub path: String,
        /// Description shown in `ALL_TOOLS` / `searchTools`.
        pub description: Option<String>,
        /// The tool's JSON Schema for its input, rendered into the TypeScript
        /// declarations `describeTool` / `describeNamespace` return.
        pub input_schema: Option<Value>,
        /// How the tool is exposed. Direct tools appear in the host's
        /// `tools/list` (mode [`CodemodeMode::On`]) and in the `codemode`
        /// description's inline declarations; deferred and codemode-only
        /// tools are discoverable only inside scripts.
        pub exposure: ToolExposure,
        /// Async dispatcher: JSON args string → JSON result string.
        pub execute: ToolFn,
    }

    /// Wire every registered tool of an [`Aggregator`](rmcp_codemode::aggregate::Aggregator)
    /// into sandbox tools dispatched through [`Aggregator::call_from_script`](rmcp_codemode::aggregate::Aggregator::call_from_script).
    pub fn tools_from_aggregator(
        aggregator: std::sync::Arc<rmcp_codemode::aggregate::Aggregator>,
    ) -> Vec<UpstreamTool> {
        aggregator
            .registry()
            .entries()
            .map(|entry| {
                let path = entry.path.clone();
                let description = entry.tool.description.as_ref().map(|d| d.to_string());
                let input_schema = Some(Value::Object((*entry.tool.input_schema).clone()));
                let exposure = entry.exposure;
                let aggregator = std::sync::Arc::clone(&aggregator);
                UpstreamTool {
                    path: path.clone(),
                    description,
                    input_schema,
                    exposure,
                    execute: Arc::new(move |args_json: String| {
                        let aggregator = std::sync::Arc::clone(&aggregator);
                        let path = path.clone();
                        Box::pin(async move {
                            let arguments: Option<serde_json::Map<String, Value>> =
                                serde_json::from_str(&args_json)
                                    .ok()
                                    .and_then(|v: Value| v.as_object().cloned());
                            let result = aggregator
                                .call_from_script(&path, arguments)
                                .await
                                .map_err(|e| e.to_string())?;
                            result
                                .content
                                .first()
                                .and_then(|c| c.as_text())
                                .map(|t| t.text.clone())
                                .ok_or_else(|| "upstream returned no text content".to_owned())
                        })
                    }),
                }
            })
            .collect()
    }

    /// A ready-to-serve codemode MCP server over a set of upstream tools.
    ///
    /// Construct with [`CodemodeServer::new`], then serve it over any rmcp
    /// transport; hosts see exactly one `codemode` tool. Each script gets a
    /// fresh QuickJS VM (pi's model): a runaway or failed script cannot poison
    /// the next run, and the handler stays `Send` without a shared runtime.
    pub struct CodemodeServer {
        tools: Arc<Vec<UpstreamTool>>,
        timeout: Option<std::time::Duration>,
        memory_limit: usize,
        mode: CodemodeMode,
        namespace_descriptions: Arc<BTreeMap<String, String>>,
        store: Store,
    }

    impl CodemodeServer {
        /// Create the server over a set of upstream tools.
        pub fn new(tools: Vec<UpstreamTool>) -> Self {
            Self {
                tools: Arc::new(tools),
                timeout: None,
                memory_limit: super::DEFAULT_MEMORY_LIMIT,
                mode: CodemodeMode::default(),
                namespace_descriptions: Arc::default(),
                store: Arc::default(),
            }
        }

        /// Create the server from a connected aggregator: every registered
        /// tool becomes callable from scripts, routed by registry path, with
        /// the aggregator's namespace descriptions served to
        /// `describeNamespace`.
        pub fn from_aggregator(
            aggregator: std::sync::Arc<rmcp_codemode::aggregate::Aggregator>,
        ) -> Self {
            let namespace_descriptions: BTreeMap<String, String> = aggregator
                .namespaces()
                .filter_map(|namespace| {
                    aggregator
                        .registry()
                        .namespace_description(namespace)
                        .map(|d| (namespace.to_owned(), d.to_owned()))
                })
                .collect();
            let mut server = Self::new(tools_from_aggregator(aggregator));
            server.namespace_descriptions = Arc::new(namespace_descriptions);
            server
        }

        /// Set how directly-exposed tools are presented (default
        /// [`CodemodeMode::On`]).
        pub fn with_mode(mut self, mode: CodemodeMode) -> Self {
            self.mode = mode;
            self
        }

        /// Set a default script deadline.
        pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
            self.timeout = Some(timeout);
            self
        }

        /// Set the per-run VM heap limit (bytes).
        pub fn with_memory_limit(mut self, memory_limit: usize) -> Self {
            self.memory_limit = memory_limit;
            self
        }

        /// The tool description shown to the model: the script environment's
        /// globals, plus TypeScript declarations for directly-exposed tools
        /// within the inline budget (pi: deferred tools are not listed, the
        /// model finds them with `searchTools`/`describeTool`).
        pub fn tool_definition(&self) -> Tool {
            let mut description = String::from(
                "Run a JavaScript script that calls tools. Upstream tools are available \
                 as `tools.<namespace>.<tool>(args)` returning promises; use \
                 `Promise.allSettled` to fan out and filter results before returning. \
                 Only the script's return value, `text()` output, and `console` lines \
                 reach the model — intermediate tool results stay inside the script.\n\n\
                 The script may start with an options line: \
                 `// @options: {\"max_output_tokens\": 2000, \"timeout_ms\": 60000}`.\n\n\
                 Globals: `tools.*`, `text(value)`, `image(dataUri)`, \
                 `console.log(...)`, `exit()`, `store(key, value)` / `load(key)`, \\
                 `ALL_TOOLS` (every tool as `{ name, description }`), \\
                 `searchTools(query, { limit?, namespace? })` (BM25 ranking, \
                 default limit 8), `describeTool(name)` (description plus \
                 TypeScript declaration), `describeNamespace(name)`. The script \
                 is the body of an async function: top-level `await` and `return` \
                 work. No timers, network, or file system: a script waiting on a \
                 promise nothing can settle fails immediately.",
            );
            let inline = self.inline_declarations();
            if !inline.is_empty() {
                description.push_str("\n\n## Tools\n\n");
                description.push_str(&inline);
            }
            Tool::new(
                CODEMODE_TOOL_NAME,
                description,
                json!({
                    "type": "object",
                    "properties": {
                        "script": {
                            "type": "string",
                            "description": "Raw JavaScript source (the body of an async function)."
                        }
                    },
                    "required": ["script"]
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            )
        }

        /// TypeScript declarations of directly-exposed tools, grouped by
        /// namespace, within the inline budget (pi's `codemode.inlineBudget`:
        /// declarations that do not fit are omitted — the model finds them
        /// with `searchTools` and `describeTool` inside the script).
        fn inline_declarations(&self) -> String {
            let budget = (INLINE_BUDGET_TOKENS as usize) * super::CHARS_PER_TOKEN;
            let mut used = 0usize;
            let mut omitted = 0usize;
            let mut by_namespace: BTreeMap<&str, Vec<&UpstreamTool>> = BTreeMap::new();
            for tool in self.tools.iter().filter(|t| t.exposure.is_listed()) {
                by_namespace
                    .entry(tool.path.split('.').next().unwrap_or(&tool.path))
                    .or_default()
                    .push(tool);
            }
            let mut sections = String::new();
            for (namespace, tools) in &by_namespace {
                let mut members = String::new();
                if let Some(description) = self.namespace_descriptions.get(*namespace) {
                    members.push_str(&format!("<!-- {description} -->\n"));
                }
                for tool in tools {
                    let declaration = super::ts_declaration(
                        &tool.path,
                        tool.description.as_deref(),
                        tool.input_schema.as_ref(),
                    );
                    let needed = declaration.len() + 1;
                    if used + needed > budget {
                        omitted += 1;
                        continue;
                    }
                    used += needed;
                    members.push_str(&declaration);
                    members.push('\n');
                }
                if members.is_empty() {
                    continue;
                }
                sections.push_str(&format!("### {namespace}\n\n{members}\n"));
            }
            if omitted > 0 {
                sections.push_str(&format!(
                    "{omitted} more tools exceed the inline declaration budget; call them via \
                     `tools.<path>(...)` and find them with `searchTools` or `describeTool`."
                ));
            }
            sections
        }

        /// Dispatch one script in a fresh VM, converting the sandbox result
        /// into an MCP `CallToolResult`.
        ///
        /// The VM is created inside a dedicated task that owns it, so this
        /// future stays `Send` (rmcp's `ServerHandler` requirement).
        pub async fn run_script(
            &self,
            script: &str,
        ) -> Result<CallToolResult, super::SandboxError> {
            let script = script.to_owned();
            let tools = Arc::clone(&self.tools);
            let timeout = self.timeout;
            let memory_limit = self.memory_limit;
            let store = Arc::clone(&self.store);
            let namespace_descriptions = Arc::clone(&self.namespace_descriptions);
            let outcome = tokio::spawn(async move {
                let sandbox_tools: Vec<SandboxTool> = tools
                    .iter()
                    .map(|t| {
                        let tool = SandboxTool::new(
                            t.path.clone(),
                            t.description.clone().unwrap_or_default(),
                            Arc::clone(&t.execute),
                        );
                        match &t.input_schema {
                            Some(schema) => tool.with_input_schema(schema.clone()),
                            None => tool,
                        }
                    })
                    .collect();
                let mut sandbox = Sandbox::with_memory_limit(memory_limit).await?;
                sandbox.set_namespace_descriptions((*namespace_descriptions).clone());
                sandbox
                    .execute_with_store(&script, sandbox_tools, timeout, Some(&store))
                    .await
            });
            let result = outcome
                .await
                .map_err(|e| super::SandboxError::Engine(format!("script task failed: {e}")))??;

            // pi's codemode result format: a header with the wall time, the
            // script's text/console output, and `Script error:` on failure.
            // Images additionally become MCP image content blocks, in output
            // order after the text.
            let text = super::render_result_text(&result);
            let mut content = vec![ContentBlock::text(text)];
            for item in &result.output {
                if let super::OutputItem::Image { data, mime_type } = item {
                    content.push(ContentBlock::image(data.clone(), mime_type.clone()));
                }
            }
            if result.ok {
                Ok(CallToolResult::success(content))
            } else {
                Ok(CallToolResult::error(content))
            }
        }
    }

    impl ServerHandler for CodemodeServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(Default::default()).with_instructions(
                "Code mode: upstream MCP tools are callable from scripts, not listed.",
            )
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            let mut tools = vec![self.tool_definition()];
            // Direct tools stay in the host's tools/list in `On` mode; `Only`
            // hides them behind the codemode tool entirely (pi's
            // `codemode.mode`, OpenCode 2.0's default).
            if self.mode == CodemodeMode::On {
                for tool in self.tools.iter().filter(|t| t.exposure.is_listed()) {
                    tools.push(Tool::new(
                        tool.path.clone(),
                        tool.description.clone().unwrap_or_default(),
                        tool.input_schema
                            .clone()
                            .and_then(|schema| schema.as_object().cloned())
                            .unwrap_or_default(),
                    ));
                }
            }
            Ok(ListToolsResult {
                tools,
                ..Default::default()
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, McpError> {
            if request.name != CODEMODE_TOOL_NAME {
                return self.call_upstream(&request.name, request.arguments).await;
            }
            let script = request
                .arguments
                .as_ref()
                .and_then(|args| args.get("script"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    McpError::invalid_params("missing required `script` string argument", None)
                })?
                .to_owned();

            self.run_script(&script)
                .await
                .map(CallToolResponse::Complete)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        }
    }

    impl CodemodeServer {
        /// Route a native host call to a directly-exposed tool. Deferred and
        /// codemode-only tools answer with an instructive error result (the
        /// self-correction shape from Zed discussion #64004): the model is
        /// told to call the tool from a script instead.
        async fn call_upstream(
            &self,
            name: &str,
            arguments: Option<rmcp::model::JsonObject>,
        ) -> Result<CallToolResponse, McpError> {
            let tool = self.tools.iter().find(|t| t.path == name);
            let Some(tool) = tool else {
                return Err(McpError::method_not_found::<CallToolRequestMethod>());
            };
            if !tool.exposure.is_listed() {
                let exposure = tool.exposure;
                let result = CallToolResult::error(vec![ContentBlock::text(format!(
                    "Tool '{name}' is {exposure} and not directly callable. Write a \
                     codemode script instead: call `tools.{name}(...)` inside a `codemode` \
                     tool call; discover it with `searchTools` inside the script."
                ))]);
                return Ok(CallToolResponse::Complete(result));
            }
            let args_json = arguments.map(Value::Object).unwrap_or(Value::Null);
            let args_json = serde_json::to_string(&args_json)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            match (tool.execute)(args_json).await {
                Ok(value) => Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(value),
                ]))),
                Err(message) => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                    ContentBlock::text(format!("{name}: {message}")),
                ]))),
            }
        }
    }
}
