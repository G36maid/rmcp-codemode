//! The QuickJS engine: pi's sandbox lifecycle over rquickjs, in-thread
//! (SPEC §5). A fresh VM per run (D3), disposed on settle; preemption via the
//! interrupt handler (deadline + abort), matching pi's in-thread backend.
//!
//! Transport (internal, script-invisible): pi moves VM messages over worker
//! threads; here the verbatim prelude runs inside the runtime's JS lifetime.
//! Its `bridge(kind, a, b, c)` messages arrive in a Rust native; nested-call
//! answers flow through an answer queue drained by a pump task that calls the
//! prelude's own `settle` — the prelude stays a verbatim port, and `stalled()`
//! keeps its never-settling semantics.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures::{FutureExt, future::BoxFuture};
use rmcp_codemode::{
    engine::{Engine, RunHandle},
    naming::to_codemode_identifier,
    types::{
        Call, CallStatus, DeliverError, ErrorKind, ExecuteOptions, Outcome, OutputItem, RunOptions,
        RunResult, SandboxOptions, ScriptError, StoreWrites, Success, Tool, ToolContext, ToolError,
    },
};
use rquickjs::{AsyncContext, AsyncRuntime, IntoJs, Undefined};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::{
    clock::{Mulberry32, VirtualClock},
    prelude::{PRELUDE_SOURCE, SCRIPT_WRAPPER_PREFIX},
};

/// The sandbox: construction options plus the engine entry points. (pi:
/// `CodemodeSandboxBase`.)
pub struct QuickJsEngine {
    sandbox: SandboxOptions,
}

/// Names a global may not use: the built-in globals (pi: "must not shadow
/// the built-in globals: `tools`, `ALL_TOOLS`, `console`, `text`, `image`,
/// `exit`, `store`, `load`") and the built-in namespace prefixes
/// (pi rejects `tools.x`, `store.x`).
const RESERVED_GLOBALS: [&str; 8] = [
    "tools",
    "ALL_TOOLS",
    "console",
    "text",
    "image",
    "exit",
    "store",
    "load",
];
const RESERVED_NAMESPACES: [&str; 8] = [
    "tools",
    "ALL_TOOLS",
    "console",
    "text",
    "image",
    "exit",
    "store",
    "load",
];

/// A valid global name: an identifier, or `<namespace>.<member>` with one dot
/// and valid identifier parts (pi: `a.b.c`, `a.`, `.a`, `a.not-valid` are
/// all rejected).
fn validate_global_name(name: &str) -> Result<(), String> {
    let valid_ident = |s: &str| {
        let mut chars = s.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    };
    match name.split_once('.') {
        None => {
            if valid_ident(name) {
                Ok(())
            } else {
                Err(format!("Invalid global name \"{name}\""))
            }
        }
        Some((namespace, member)) => {
            if !valid_ident(namespace)
                || !valid_ident(member)
                || RESERVED_NAMESPACES.contains(&namespace)
            {
                Err(format!("Invalid global name \"{name}\""))
            } else {
                Ok(())
            }
        }
    }
}

impl QuickJsEngine {
    /// Validates globals (pi throws at construction for invalid or reserved
    /// names: `newSandbox({globals: [{name: "store", ...}]})` throws
    /// /Invalid global/).
    ///
    /// # Panics
    /// Panics on an invalid or reserved global name — a construction-time
    /// configuration error, mirroring pi's throw.
    pub fn new(sandbox: SandboxOptions) -> Self {
        for global in &sandbox.globals {
            if let Err(message) = validate_global_name(&global.name) {
                panic!("{message}");
            }
            if RESERVED_GLOBALS.contains(&global.name.as_str()) {
                panic!("Invalid global name \"{}\"", global.name);
            }
        }
        Self { sandbox }
    }
}

impl Engine for QuickJsEngine {
    fn execute(&self, script: &str, options: ExecuteOptions) -> BoxFuture<'static, RunResult> {
        run_execute(script.to_owned(), self.sandbox.clone(), options).boxed()
    }

    fn start(&self, script: &str, options: RunOptions) -> Box<dyn RunHandle> {
        run_start(script.to_owned(), self.sandbox.clone(), options)
    }
}

// ---------------------------------------------------------------- shared ---

struct Shared {
    output: Mutex<Vec<OutputItem>>,
    calls: Mutex<Vec<Call>>,
    /// call id -> dispatch instant (for `duration_ms`).
    call_starts: Mutex<HashMap<u64, Instant>>,
    /// Calls issued but not yet settled: id -> name.
    outstanding: Mutex<HashMap<u64, String>>,
    issued: AtomicU64,
    /// Set once the prelude reported `done`: no further output/calls accepted;
    /// the interrupt handler asks the VM to stop.
    finished: AtomicBool,
    /// Which stop cause pre-empted the VM, if any (deadline/abort override the
    /// script error the interruption surfaces as).
    deadline_hit: AtomicBool,
    abort_hit: AtomicBool,
    /// Cancels in-flight tool futures when the script ends or the run stops.
    tool_signal: CancellationToken,
    /// The final result, stored by the bridge's `done` (or a fail path).
    result: Mutex<Option<RunResult>>,
}

impl Shared {
    fn new(signal: &CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            output: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            call_starts: Mutex::new(HashMap::new()),
            outstanding: Mutex::new(HashMap::new()),
            issued: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            deadline_hit: AtomicBool::new(false),
            abort_hit: AtomicBool::new(false),
            // Child of the run signal: caller aborts propagate; `done`
            // cancels this to stop unawaited tools (pi: "aborts unawaited
            // calls when the script returns").
            tool_signal: signal.child_token(),
            result: Mutex::new(None),
        })
    }

    fn push_output(&self, item: OutputItem) {
        self.output.lock().unwrap().push(item);
    }

    fn record_issued(&self, id: u64, name: &str) {
        self.call_starts.lock().unwrap().insert(id, Instant::now());
        self.outstanding.lock().unwrap().insert(id, name.to_owned());
        self.issued.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a settled call and return its duration in ms.
    fn record_settled(&self, id: u64, name: &str, status: CallStatus) -> u64 {
        self.outstanding.lock().unwrap().remove(&id);
        let started = self.call_starts.lock().unwrap().remove(&id);
        let duration = started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
        self.calls.lock().unwrap().push(Call {
            name: name.to_owned(),
            status,
            duration_ms: duration,
        });
        duration
    }

    /// A script that returned or was stopped with calls still outstanding:
    /// they are cancelled (pi: "aborts unawaited calls when the script
    /// returns").
    fn cancel_outstanding(&self) {
        let mut outstanding = self.outstanding.lock().unwrap();
        let mut starts = self.call_starts.lock().unwrap();
        for (id, name) in outstanding.drain() {
            let started = starts.remove(&id);
            let duration = started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
            self.calls.lock().unwrap().push(Call {
                name,
                status: CallStatus::Cancelled,
                duration_ms: duration,
            });
        }
    }

    fn assemble(&self, ok: bool, payload: Option<String>, writes: Option<String>) -> RunResult {
        self.cancel_outstanding();
        let outcome = if self.deadline_hit.load(Ordering::Relaxed) {
            Err(ScriptError {
                kind: ErrorKind::Timeout,
                name: None,
                message: "the deadline expired".into(),
                stack: None,
            })
        } else if self.abort_hit.load(Ordering::Relaxed) {
            Err(ScriptError {
                kind: ErrorKind::Aborted,
                name: None,
                message: "the run was aborted".into(),
                stack: None,
            })
        } else if ok {
            let value = payload
                .as_deref()
                .and_then(|p| serde_json::from_str(p).ok())
                .unwrap_or(Value::Null);
            let mut set = BTreeMap::new();
            let mut delete = Vec::new();
            if let Some(entries) = writes
                .as_deref()
                .and_then(|w| serde_json::from_str::<Value>(w).ok())
                .and_then(|w| w.as_array().cloned())
            {
                for entry in entries {
                    match entry.as_array().map(|a| a.as_slice()) {
                        Some([key]) => {
                            if let Some(k) = key.as_str() {
                                delete.push(k.to_owned());
                            }
                        }
                        Some([key, json_text]) if json_text.is_string() => {
                            // The write crosses as JSON TEXT; parse it into
                            // the typed write.
                            if let Some(k) = key.as_str()
                                && let Ok(parsed) =
                                    serde_json::from_str::<Value>(json_text.as_str().unwrap())
                            {
                                set.insert(k.to_owned(), parsed);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Success {
                value,
                store_writes: StoreWrites { set, delete },
            })
        } else {
            let error = payload
                .as_deref()
                .and_then(|p| serde_json::from_str::<Value>(p).ok())
                .unwrap_or(Value::Null);
            let name = error.get("name").and_then(Value::as_str).map(str::to_owned);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let stack = error
                .get("stack")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Err(ScriptError {
                kind: ErrorKind::Script,
                name,
                message,
                stack,
            })
        };
        RunResult {
            outcome,
            output: self.output.lock().unwrap().clone(),
            calls: self.calls.lock().unwrap().clone(),
            during: None,
        }
    }

    fn fail(&self, kind: ErrorKind, message: &str) -> RunResult {
        self.cancel_outstanding();
        RunResult {
            outcome: Err(ScriptError {
                kind,
                name: None,
                message: message.to_owned(),
                stack: None,
            }),
            output: self.output.lock().unwrap().clone(),
            calls: self.calls.lock().unwrap().clone(),
            during: None,
        }
    }
}

// ------------------------------------------------------- bridge dispatch ---

enum Answer {
    Ok { id: u64, payload: String },
    Err { id: u64, message: String },
}

type AnswerQueue = Arc<(AsyncMutex<VecDeque<Answer>>, Notify)>;

/// One bridge message, parsed. (pi: `bridge(kind, a, b, c)`.)
enum BridgeMsg {
    Call {
        id: u64,
        name: String,
        args_json: String,
    },
    Global {
        id: u64,
        name: String,
        args_json: String,
    },
    OutputText {
        console: bool,
        text: String,
    },
    OutputImage {
        data: String,
        mime_type: String,
    },
    Done {
        ok: bool,
        payload: Option<String>,
        writes: Option<String>,
    },
}

fn parse_bridge(
    kind: &str,
    a: &rquickjs::Value<'_>,
    b: &rquickjs::Value<'_>,
    c: &rquickjs::Value<'_>,
) -> Option<BridgeMsg> {
    match kind {
        "call" | "global" => {
            let id = a.as_number()? as u64;
            let name = b.as_string()?.to_string().ok()?;
            let args_json = c
                .as_string()
                .and_then(|s| s.to_string().ok())
                .unwrap_or_else(|| "null".into());
            Some(if kind == "call" {
                BridgeMsg::Call {
                    id,
                    name,
                    args_json,
                }
            } else {
                BridgeMsg::Global {
                    id,
                    name,
                    args_json,
                }
            })
        }
        "output" => {
            let kind = a.as_string()?.to_string().ok()?;
            if kind == "image" {
                Some(BridgeMsg::OutputImage {
                    data: b.as_string()?.to_string().ok()?,
                    mime_type: c
                        .as_string()
                        .and_then(|s| s.to_string().ok())
                        .unwrap_or_default(),
                })
            } else {
                Some(BridgeMsg::OutputText {
                    console: kind == "console",
                    text: b.as_string()?.to_string().ok()?,
                })
            }
        }
        "done" => Some(BridgeMsg::Done {
            ok: a.as_bool()?,
            payload: b.as_string().and_then(|s| s.to_string().ok()),
            writes: c.as_string().and_then(|s| s.to_string().ok()),
        }),
        _ => None,
    }
}

/// Registers the `__bridge` native on the context and returns it as a JS
/// value (it becomes the prelude's `bridge` argument).
fn register_bridge<'js>(
    this: &rquickjs::Ctx<'js>,
    shared: Arc<Shared>,
    answers: AnswerQueue,
    tools: HashMap<String, Tool>,
    globals: HashMap<String, Tool>,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    let bridge_fn = rquickjs::function::Func::from(
        move |kind: String,
              a: rquickjs::Value<'js>,
              b: rquickjs::Value<'js>,
              c: rquickjs::Value<'js>| {
            let Some(message) = parse_bridge(&kind, &a, &b, &c) else {
                return;
            };
            let is_call = matches!(message, BridgeMsg::Call { .. });
            match message {
                BridgeMsg::Call {
                    id,
                    name,
                    args_json,
                }
                | BridgeMsg::Global {
                    id,
                    name,
                    args_json,
                } => {
                    shared.record_issued(id, &name);
                    let tool = if is_call {
                        tools.get(&name).cloned()
                    } else {
                        globals.get(&name).cloned()
                    };
                    let shared2 = Arc::clone(&shared);
                    let answers2 = Arc::clone(&answers);
                    match tool {
                        Some(tool) => {
                            let args: Value =
                                serde_json::from_str(&args_json).unwrap_or(Value::Null);
                            let fut = (tool.execute)(
                                args,
                                ToolContext {
                                    signal: shared.tool_signal.clone(),
                                },
                            );
                            tokio::spawn(async move {
                                let result = fut.await;
                                // A call that settles after the script ended is
                                // cancelled, whatever its outcome (pi: "aborts
                                // unawaited calls when the script returns").
                                let status = if shared2.finished.load(Ordering::Relaxed) {
                                    CallStatus::Cancelled
                                } else if result.is_ok() {
                                    CallStatus::Ok
                                } else {
                                    CallStatus::Error
                                };
                                shared2.record_settled(id, &name, status);
                                let mut queue = answers2.0.lock().await;
                                match result {
                                    Ok(value) => queue.push_back(Answer::Ok {
                                        id,
                                        payload: serde_json::to_string(&value)
                                            .unwrap_or_else(|_| "null".into()),
                                    }),
                                    Err(ToolError { message }) => {
                                        queue.push_back(Answer::Err { id, message })
                                    }
                                }
                                answers2.1.notify_one();
                            });
                        }
                        None => {
                            shared.record_settled(id, &name, CallStatus::Error);
                            let answers2 = Arc::clone(&answers);
                            tokio::spawn(async move {
                                let mut queue = answers2.0.lock().await;
                                queue.push_back(Answer::Err {
                                    id,
                                    message: format!("tool `{name}` is not available"),
                                });
                                answers2.1.notify_one();
                            });
                        }
                    }
                }
                BridgeMsg::OutputText { console, text } => {
                    shared.push_output(OutputItem::Text { text, console });
                }
                BridgeMsg::OutputImage { data, mime_type } => {
                    shared.push_output(OutputItem::Image { data, mime_type });
                }
                BridgeMsg::Done {
                    ok,
                    payload,
                    writes,
                } => {
                    shared.finished.store(true, Ordering::Relaxed);
                    shared.tool_signal.cancel();
                    let result = shared.assemble(ok, payload, writes);
                    *shared.result.lock().unwrap() = Some(result);
                }
            }
        },
    );
    bridge_fn.into_js(this)
}

// ---------------------------------------------------------------- execute ---

async fn run_execute(
    script: String,
    sandbox: SandboxOptions,
    options: ExecuteOptions,
) -> RunResult {
    let shared = Shared::new(&options.signal.clone().unwrap_or_default());
    let run_signal = options.signal.clone().unwrap_or_default();

    // Tool lookup: by canonical name or normalized js name (pi exposes both).
    let mut by_name: HashMap<String, Tool> = HashMap::new();
    let mut declarations: Vec<Value> = Vec::new();
    for tool in &sandbox.tools {
        let js_name = to_codemode_identifier(&tool.name);
        by_name
            .entry(js_name.clone())
            .or_insert_with(|| tool.clone());
        by_name
            .entry(tool.name.clone())
            .or_insert_with(|| tool.clone());
        declarations.push(json!({
            "name": tool.name,
            "jsName": js_name,
            // pi's toolsJson defaults the description to "".
            "description": tool.description.clone().unwrap_or_default(),
        }));
    }
    let mut globals_by_name: HashMap<String, Tool> = HashMap::new();
    let mut global_declarations: Vec<Value> = Vec::new();
    for global in &sandbox.globals {
        let js_name = to_codemode_identifier(&global.name);
        globals_by_name.insert(js_name.clone(), global.clone());
        globals_by_name
            .entry(global.name.clone())
            .or_insert_with(|| global.clone());
        global_declarations.push(json!({"name": global.name, "spread": global.spread}));
    }

    let deadline = options
        .timeout_ms
        .or(sandbox.timeout_ms)
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let answers: AnswerQueue = Arc::new((AsyncMutex::new(VecDeque::new()), Notify::new()));

    let runtime = AsyncRuntime::new().expect("rquickjs runtime");
    runtime
        .set_memory_limit(sandbox.memory_limit_bytes.unwrap_or(0) as usize)
        .await;
    runtime.set_max_stack_size(1024 * 1024).await;
    {
        let run_signal = run_signal.clone();
        let shared = Arc::clone(&shared);
        runtime
            .set_interrupt_handler(Some(Box::new(move || {
                if run_signal.is_cancelled() {
                    shared.abort_hit.store(true, Ordering::Relaxed);
                    return true;
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    shared.deadline_hit.store(true, Ordering::Relaxed);
                    return true;
                }
                shared.finished.load(Ordering::Relaxed)
            })))
            .await;
    }
    let ctx = AsyncContext::full(&runtime)
        .await
        .expect("rquickjs context");

    // Setup runs inside the VM lock; the script's promise comes back so the
    // tick loop below can drive the VM's job queue while we wait (pi: the
    // worker drives the VM until the script settles).
    // Setup, pump, and tick loop all run INSIDE one async_with: awaiting the
    // script's promise future drives the VM's job queue (v1's mechanism), and
    // the tick timeout detects a never-settling script (pi `stalled()`).
    let result: RunResult = ctx
        .async_with(async |vctx| {
            // ---- setup: bridge, prelude, script; returns the script promise.
            let setup: Result<rquickjs::Promise, String> = (|| {
                let bridge = register_bridge(
                    &vctx,
                    Arc::clone(&shared),
                    Arc::clone(&answers),
                    by_name,
                    globals_by_name,
                )
                .map_err(|e| format!("bridge registration failed: {e}"))?;
                let mut eval_opts = rquickjs::context::EvalOptions::default();
                eval_opts.filename = Some("codemode-prelude.js".into());
                let prelude_fn: rquickjs::Function = vctx
                    .eval_with_options(PRELUDE_SOURCE, eval_opts)
                    .map_err(|e| format!("prelude failed: {e}"))?;
                let tools_json = json!(declarations).to_string();
                let globals_json = json!(global_declarations).to_string();
                // pi: storeJson is a snapshot of JSON TEXT — each value crosses as
            // its JSON encoding, and the prelude parses on `load`.
            let store_text: BTreeMap<&String, String> = options
                .store
                .iter()
                .map(|(k, v)| (k, serde_json::to_string(v).unwrap_or_else(|_| "null".into())))
                .collect();
            let store_json = json!(store_text).to_string();
                let api_obj: rquickjs::Object = prelude_fn
                    .call((bridge, tools_json, globals_json, store_json))
                    .map_err(|e| format!("prelude init failed: {e}"))?;
                let settle: rquickjs::Function =
                    api_obj.get("settle").map_err(|e| e.to_string())?;
                vctx
                    .globals()
                    .set("__pi_settle", settle)
                    .map_err(|e| e.to_string())?;
                let mut eval_opts = rquickjs::context::EvalOptions::default();
                eval_opts.filename = Some("codemode.js".into());
                let wrapped = format!("{SCRIPT_WRAPPER_PREFIX}{script}\n}})");
                let script_fn: rquickjs::Function = vctx
                    .eval_with_options(wrapped, eval_opts)
                    .map_err(|e| format!("script failed to parse: {e}"))?;
                let run: rquickjs::Function = api_obj.get("run").map_err(|e| e.to_string())?;
                run.call::<_, rquickjs::Promise>((script_fn,))
                    .map_err(|e| format!("script failed to start: {e}"))
            })();
            let mut script_future = Box::pin(match setup {
                Ok(promise) => promise.into_future::<rquickjs::Value>(),
                Err(e) => {
                    shared.finished.store(true, Ordering::Relaxed);
                    shared.tool_signal.cancel();
                    let fail = shared.fail(ErrorKind::Sandbox, &e);
                    *shared.result.lock().unwrap() = Some(fail.clone());
                    return fail;
                }
            });

            // The pump: drain tool answers into the prelude's settle.
            let mut pump = Box::pin(async {
                let (queue, notify) = &*answers;
                loop {
                    let mut guard = queue.lock().await;
                    let answer = loop {
                        if let Some(answer) = guard.pop_front() {
                            break answer;
                        }
                        drop(guard);
                        notify.notified().await;
                        guard = queue.lock().await;
                    };
                    drop(guard);
                    let Some(settle) = vctx
                        .globals()
                        .get::<_, rquickjs::Function>("__pi_settle")
                        .ok()
                    else {
                        return;
                    };
                    let settled = match answer {
                        Answer::Ok { id, payload } => {
                            settle.call::<_, ()>((id as f64, true, payload, Undefined))
                        }
                        Answer::Err { id, message } => {
                            settle.call::<_, ()>((id as f64, false, message, Undefined))
                        }
                    };
                    let _ = settled;
                }
            });

            // The tick loop (v1's pattern; pi: the worker drives the VM; a
            // never-settling script with no call in flight fails after a few
            // idle ticks).
            let tick = Duration::from_millis(25);
            let mut idle_ticks = 0u32;
            loop {
                // Drive the VM's pending jobs (promise reactions: the body,
                // the `then` that fires `done`, tool continuations).
                while vctx.execute_pending_job() {}
                // Stop-arms first: deadline/abort pre-empt everything.
                if let Some(result) = shared.result.lock().unwrap().take() {
                    break result;
                }
                if run_signal.is_cancelled() {
                    break shared.fail(ErrorKind::Aborted, "the run was aborted");
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break shared.fail(ErrorKind::Timeout, "the deadline expired");
                }
                tokio::select! {
                    _settled = &mut script_future => {
                        // The script promise settled. `done` normally fired
                        // through the bridge during the same poll; if it did
                        // not, the idle logic below reports the never-settling
                        // error on the next ticks.
                        if let Some(result) = shared.result.lock().unwrap().take() {
                            break result;
                        }
                        if shared.outstanding.lock().unwrap().is_empty() {
                            idle_ticks += 1;
                            if idle_ticks >= 3 {
                                break shared.fail(
                                    ErrorKind::Script,
                                    "script waits on a promise that can never settle (no timers; no tool call pending)",
                                );
                            }
                        }
                    }
                    _ = &mut pump => {
                        break shared.fail(ErrorKind::Sandbox, "the answer pump stopped");
                    }
                    _ = tokio::time::sleep(tick) => {
                        // Idle tick: no progress in 25 ms.
                        if shared.finished.load(Ordering::Relaxed)
                            && let Some(result) = shared.result.lock().unwrap().take()
                        {
                            break result;
                        }
                        if !shared.outstanding.lock().unwrap().is_empty() {
                            idle_ticks = 0;
                        }
                    }
                }
            }
        })
        .await;
    shared.cancel_outstanding();
    shared.cancel_outstanding();
    result
}
// ------------------------------------------------------------- start mode ---

fn run_start(
    _script: String,
    _sandbox: SandboxOptions,
    _options: RunOptions,
) -> Box<dyn RunHandle> {
    // Lands with the deterministic phase (SPEC §11.5 step 3); the stub keeps
    // the trait honest until the ported deterministic tests drive it.
    Box::new(StubRun)
}

struct StubRun;
impl RunHandle for StubRun {
    fn result(&mut self) -> BoxFuture<'_, RunResult> {
        async {
            RunResult {
                outcome: Err(ScriptError {
                    kind: ErrorKind::Sandbox,
                    name: None,
                    message: "start() lands with the deterministic phase".into(),
                    stack: None,
                }),
                output: vec![],
                calls: vec![],
                during: None,
            }
        }
        .boxed()
    }
    fn finished(&self) -> bool {
        true
    }
    fn deliver(
        &self,
        _seq: u64,
        _now: u64,
        _id: u64,
        _outcome: Outcome,
    ) -> Result<(), DeliverError> {
        Err(DeliverError::NotWaiting)
    }
}

// Deterministic phase will consume: the virtual clock and PRNG (§5.4).
const _: Option<fn(u64, u32)> = Some(|_started_at, _seed| {
    let _ = VirtualClock::new(0);
    let _ = Mulberry32::new(0);
});
