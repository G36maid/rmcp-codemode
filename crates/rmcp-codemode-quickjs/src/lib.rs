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
    sync::{Arc, Mutex},
    time::Instant,
};

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
    /// The async dispatcher.
    pub execute: ToolFn,
}

impl SandboxTool {
    /// A tool from a path, description, and dispatcher.
    pub fn new(name: impl Into<String>, description: impl Into<String>, execute: ToolFn) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            execute,
        }
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
}

/// The sandbox: one QuickJS runtime, reused across script executions.
pub struct Sandbox {
    runtime: AsyncRuntime,
    context: AsyncContext,
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
        Ok(Self { runtime, context })
    }

    /// Execute a script.
    ///
    /// `deadline` is a hard wall-clock limit for the whole script, including
    /// upstream tool calls; scripts that exceed it fail without their partial
    /// output being distinguishable from other failures. `None` means no
    /// deadline. Note that a script spinning in a pure-JS infinite loop
    /// without awaiting cannot be preempted by this sandbox build; a deadline
    /// still bounds how long `execute` waits before reporting failure.
    pub async fn execute(
        &self,
        source: &str,
        tools: Vec<SandboxTool>,
        deadline: Option<std::time::Duration>,
    ) -> Result<ScriptResult, SandboxError> {
        let output: Arc<Mutex<Vec<OutputItem>>> = Arc::new(Mutex::new(Vec::new()));
        let calls: Arc<Mutex<Vec<ToolCall>>> = Arc::new(Mutex::new(Vec::new()));

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

        let source = source.to_owned();
        let output_arc = Arc::clone(&output);
        let calls_arc = Arc::clone(&calls);
        let tool_map_arc = Arc::clone(&tool_map);
        // The deadline covers the whole execution — the script's own awaits
        // and upstream tool calls included.
        let run = async {
            let envelope: Value = self
                .context
                .async_with(async |ctx| {
                    run_script(ctx, &source, output_arc, calls_arc, tool_map_arc).await
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
        Ok(finalize(envelope, &output, &calls))
    }
}

/// Bind globals, run the script, and return its result envelope.
async fn run_script<'js>(
    ctx: rquickjs::Ctx<'js>,
    source: &str,
    output: Arc<Mutex<Vec<OutputItem>>>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    tool_map: Arc<BTreeMap<String, SandboxTool>>,
) -> Result<Value, SandboxError> {
    setup_globals(&ctx, output, calls, tool_map)?;
    // The script is the body of an async function. The IIFE's result crosses
    // as a JSON envelope string so neither side needs native value conversion.
    let wrapped = format!(
        "(async () => {{\n{source}\n}})().then(\n\x20 (v) => JSON.stringify({{ ok: true, value: v === undefined ? null : v }}),\n\x20 (e) => JSON.stringify({{ ok: false, error: e && e.message !== undefined ? String(e.message) : String(e) }})\n )"
    );
    // A parse error or an unsettled promise surfaces as a failed result, not
    // an Err: pi reports script failures through the result, and partial
    // output (text before the failure) is kept.
    let promise: Promise = match ctx.eval(wrapped.as_bytes()).catch(&ctx) {
        Ok(promise) => promise,
        Err(caught) => return Ok(failed_envelope(describe_caught(&ctx, caught))),
    };
    let envelope: String = match promise.into_future::<String>().await.catch(&ctx) {
        Ok(envelope) => envelope,
        Err(caught) => return Ok(failed_envelope(describe_caught(&ctx, caught))),
    };

    serde_json::from_str(&envelope)
        .map_err(|e| SandboxError::Engine(format!("bad result envelope: {e}")))
}

/// An envelope for a script that failed before producing one.
fn failed_envelope(message: String) -> Value {
    serde_json::json!({ "ok": false, "error": message })
}

/// Build the final [`ScriptResult`] from the result envelope and side channels.
fn finalize(
    envelope: Value,
    output: &Mutex<Vec<OutputItem>>,
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
        output: std::mem::take(&mut *output.lock().unwrap()),
        calls: std::mem::take(&mut *calls.lock().unwrap()),
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

/// Bind `__emitText`, `__emitConsole`, `__callTool`, `__searchTools`, and the
/// script-facing globals (`tools`, `text`, `console`, `ALL_TOOLS`,
/// `searchTools`).
fn setup_globals<'js>(
    ctx: &rquickjs::Ctx<'js>,
    output: Arc<Mutex<Vec<OutputItem>>>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    tools: Arc<BTreeMap<String, SandboxTool>>,
) -> Result<(), SandboxError> {
    let globals = ctx.globals();

    // ---- output emitters (strings already; JS stringifies non-strings) ----
    {
        let output = Arc::clone(&output);
        globals
            .set(
                "__emitText",
                Func::from(move |text: String| {
                    output.lock().unwrap().push(OutputItem::Text(text));
                }),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let output = Arc::clone(&output);
        globals
            .set(
                "__emitConsole",
                Func::from(move |line: String| {
                    output.lock().unwrap().push(OutputItem::Console(line));
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
        let call_tool = move |path: String, args_json: String| {
            let calls = Arc::clone(&calls);
            let tools = Arc::clone(&tools);
            async move {
                let dispatch =
                    async |calls: Arc<Mutex<Vec<ToolCall>>>,
                           tools: Arc<BTreeMap<String, SandboxTool>>| {
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
                            return Ok(json!({ "ok": false, "error": unknown_tool_message(&path, &tools) })
                                .to_string());
                        };
                        // Validate the argument shape only when the script sent one;
                        // `JSON.stringify(undefined)` arrives as the string "undefined".
                        if args_json != "null" && serde_json::from_str::<Value>(&args_json).is_err()
                        {
                            return Ok(json!({ "ok": false, "error": format!("Invalid arguments for tool '{path}': not valid JSON") }).to_string());
                        }
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
                dispatch(calls, tools).await
            }
        };
        globals
            .set("__callTool", Func::from(AsyncFn(call_tool)))
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
    globalThis.__emitText(typeof value === "string" ? value : JSON.stringify(value));
}};
globalThis.console = {{
    log: (...parts) => globalThis.__emitConsole(parts.map((p) => (typeof p === "string" ? p : JSON.stringify(p))).join(" ")),
}};
globalThis.console.info = globalThis.console.log;
globalThis.console.warn = globalThis.console.log;
globalThis.console.error = globalThis.console.log;
globalThis.console.debug = globalThis.console.log;
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
    use std::sync::Arc;

    use rmcp::{
        ErrorData as McpError,
        handler::server::ServerHandler,
        model::{
            CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
            ContentBlock, ListToolsResult, PaginatedRequestParams, ServerConfig, Tool,
        },
        service::{RequestContext, RoleServer},
    };
    use serde_json::{Value, json};

    use super::{Sandbox, SandboxTool, ToolFn};

    /// The single tool name exposed to hosts.
    pub const CODEMODE_TOOL_NAME: &str = "codemode";

    /// How many tool calls one script may make (safety bound).
    pub const DEFAULT_MAX_TOOL_CALLS: usize = 500;

    /// An upstream tool dispatcher wired into the sandbox.
    #[allow(clippy::exhaustive_structs)] // user-constructed by design
    pub struct UpstreamTool {
        /// Registry path, e.g. `github.list_issues`.
        pub path: String,
        /// Description shown in `ALL_TOOLS` / `searchTools`.
        pub description: Option<String>,
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
                let aggregator = std::sync::Arc::clone(&aggregator);
                UpstreamTool {
                    path: path.clone(),
                    description,
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
    }

    impl CodemodeServer {
        /// Create the server over a set of upstream tools.
        pub fn new(tools: Vec<UpstreamTool>) -> Self {
            Self {
                tools: Arc::new(tools),
                timeout: None,
                memory_limit: super::DEFAULT_MEMORY_LIMIT,
            }
        }

        /// Create the server from a connected aggregator: every registered
        /// tool becomes callable from scripts, routed by registry path.
        pub fn from_aggregator(
            aggregator: std::sync::Arc<rmcp_codemode::aggregate::Aggregator>,
        ) -> Self {
            Self::new(tools_from_aggregator(aggregator))
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
        /// TypeScript-flavored declaration, minus upstream tools (deferred by
        /// default; the model finds them with `ALL_TOOLS`).
        pub fn tool_definition() -> Tool {
            Tool::new(
                CODEMODE_TOOL_NAME,
                "Run a JavaScript script that calls tools. Upstream tools are available \
                 as `tools.<namespace>.<tool>(args)` returning promises; use \
                 `Promise.allSettled` to fan out and filter results before returning. \
                 Discover tools with `ALL_TOOLS`. Only the script's return value, \
                 `text()` output, and `console` lines reach the model — intermediate \
                 tool results stay inside the script.\n\n\
                 Globals: `tools.*`, `text(value)`, `console.log(...)`, `ALL_TOOLS`. \
                 The script is the body of an async function: top-level `await` and \
                 `return` work. No timers, network, or file system.",
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
            let outcome = tokio::spawn(async move {
                let sandbox_tools: Vec<SandboxTool> = tools
                    .iter()
                    .map(|t| {
                        SandboxTool::new(
                            t.path.clone(),
                            t.description.clone().unwrap_or_default(),
                            Arc::clone(&t.execute),
                        )
                    })
                    .collect();
                let sandbox = Sandbox::with_memory_limit(memory_limit).await?;
                sandbox.execute(&script, sandbox_tools, timeout).await
            });
            let result = outcome
                .await
                .map_err(|e| super::SandboxError::Engine(format!("script task failed: {e}")))??;

            let mut content = Vec::new();
            for item in &result.output {
                let line = match item {
                    super::OutputItem::Text(t) => t.clone(),
                    super::OutputItem::Console(t) => format!("[console] {t}"),
                };
                content.push(ContentBlock::text(line));
            }
            if let Some(error) = &result.error {
                content.push(ContentBlock::text(format!("Script failed: {error}")));
            }
            if let Some(value) = &result.value {
                content.push(ContentBlock::text(format!(
                    "Returned: {}",
                    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
                )));
            }
            if content.is_empty() {
                content.push(ContentBlock::text("(no output)"));
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
            Ok(ListToolsResult {
                tools: vec![Self::tool_definition()],
                ..Default::default()
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, McpError> {
            if request.name != CODEMODE_TOOL_NAME {
                return Err(McpError::method_not_found::<CallToolRequestMethod>());
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
}
