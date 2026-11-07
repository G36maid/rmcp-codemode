//! The QuickJS sandbox: one fresh VM per script run.
//!
//! The script is the body of an async function; the only capability it has
//! is calling registered tools (bound in [`crate::bridge`]). Lifecycle
//! semantics follow pi's `codemode`: deadline preemption via the VM's
//! interrupt handler, immediate failure on a promise nothing can settle,
//! cancellation of unawaited calls when the script ends.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use rmcp_codemode::{
    DEFAULT_MAX_OUTPUT_TOKENS, OutputItem, ScriptOptions, ScriptResult, ToolCall, len_of,
};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Promise};
use serde_json::{Value, json};

use crate::{
    bridge::setup_globals,
    types::{DEFAULT_MEMORY_LIMIT, SandboxError, SandboxTool, Store},
};

/// Output side-channel: the items plus a running character total, so the
/// output caps stay O(1) per call.
#[derive(Default)]
pub(crate) struct OutputBuffer {
    pub(crate) items: Vec<OutputItem>,
    pub(crate) chars: usize,
}

impl OutputBuffer {
    pub(crate) fn push(&mut self, item: OutputItem) {
        self.chars += len_of(&item);
        self.items.push(item);
    }
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
    /// Per-run VM heap limit in bytes.
    memory_limit: usize,
    /// Namespace descriptions served to `describeNamespace`; set by the host
    /// (an aggregator knows its upstreams' descriptions).
    namespace_descriptions: Arc<BTreeMap<String, String>>,
}

impl Sandbox {
    /// Create a sandbox.
    pub async fn new() -> Result<Self, SandboxError> {
        Self::with_memory_limit(DEFAULT_MEMORY_LIMIT).await
    }

    /// Create a sandbox with an explicit VM heap limit in bytes. An allocation
    /// past the limit throws `InternalError: out of memory` inside the script.
    pub async fn with_memory_limit(limit: usize) -> Result<Self, SandboxError> {
        Ok(Self {
            memory_limit: limit,
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

        // A fresh VM per script run (pi's model). The VM — runtime with its
        // job queue included — drops at the end of this call, which cancels
        // any upstream calls still in flight for promises the script did not
        // await, and guarantees nothing from a previous run can leak into
        // this one.
        let runtime = AsyncRuntime::new().map_err(|e| SandboxError::Engine(e.to_string()))?;
        runtime.set_memory_limit(self.memory_limit).await;
        runtime.set_max_stack_size(1024 * 1024).await;
        // The interrupt handler gives the host two hard stops inside pure-JS
        // execution: the deadline, and a script's own exit() that a try/catch
        // swallowed.
        {
            let exit_requested = Arc::clone(&exit_requested);
            runtime
                .set_interrupt_handler(Some(Box::new(move || {
                    exit_requested.load(Ordering::Relaxed)
                        || deadline_instant.is_some_and(|d| Instant::now() >= d)
                })))
                .await;
        }
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|e| SandboxError::Engine(e.to_string()))?;

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
            let envelope: Value = context
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
pub(crate) fn describe_caught(
    _ctx: &rquickjs::Ctx<'_>,
    caught: rquickjs::CaughtError<'_>,
) -> String {
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
/// Decrement the in-flight tool-call counter when the dispatch future ends,
/// however it ends.
pub(crate) struct InFlightGuard<'a>(pub(crate) &'a AtomicUsize);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
