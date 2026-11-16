//! The QuickJS engine: pi's sandbox lifecycle over rquickjs (SPEC §5). A
//! fresh VM per run (D3) on a dedicated thread with an explicit stack (pi's
//! worker isolation: a spinning script cannot hurt the host); preemption via
//! the interrupt handler (deadline + abort).
//!
//! Transport (internal, script-invisible): pi moves VM messages over worker
//! threads; here the verbatim prelude runs inside the runtime's JS lifetime.
//! Its `bridge(kind, a, b, c)` messages arrive in a Rust native; nested-call
//! answers flow through an answer queue drained by a pump that calls the
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
        Call, CallStatus, CallTarget, DeliverError, During, ErrorKind, ExecuteOptions, HostCall,
        Outcome, OutputItem, RunOptions, RunResult, SandboxOptions, ScriptError, StoreWrites,
        Success, Tool, ToolContext, ToolError,
    },
};
use rquickjs::{AsyncContext, AsyncRuntime, IntoJs, Undefined};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::{
    clock::{Mulberry32, VirtualClock},
    fingerprint::fingerprint,
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

/// Per-run state shared between the bridge native, the pump, and the tick
/// loop.
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
    /// Set by the interrupt handler once the poll budget is exhausted; the
    /// script then rejects and the tick loop classifies the rejection as
    /// `timeout` with pi's message (pi inline.ts:94).
    budget_message: Mutex<Option<String>>,
    /// Cancels in-flight tool futures when the script ends or the run stops.
    tool_signal: CancellationToken,
    /// The bridge's `done` parts, stored raw; assembled into the final
    /// RunResult after the grace period so cancelled calls are included.
    done_parts: Mutex<Option<DoneParts>>,
    /// A fail result stored by a stop-arm (abort/timeout), taking precedence
    /// over `done_parts`.
    fail_result: Mutex<Option<RunResult>>,
    /// The run has decided its outcome (done or fail). Outlives the consumed
    /// done_parts/fail_result so `finished()` stays true after `result()`
    /// (pi: the run is finished once it has its outcome).
    decided: AtomicBool,
    /// Start-mode callbacks (host answers calls) and the deterministic state.
    /// (The boxed-fn option types are spelled via aliases below to keep the
    /// struct readable.)
    on_call: OnCallSlot,
    on_idle: OnIdleSlot,
    on_started: OnStartedSlot,
    clock: Arc<Mutex<VirtualClock>>,
    rng: Arc<Mutex<Mulberry32>>,
    /// Delivered answers held until their seq arrives (pi: "after a gap it
    /// waits for the missing number").
    held: Mutex<HashMap<u64, (u64, u64, Outcome)>>,
    /// The next result number the VM will apply (1-based).
    next_seq: AtomicU64,
    /// The seq of the last applied result (0 = none yet) — `during` tracking.
    last_settled_seq: AtomicU64,
    /// The answer queue (pump drain target).
    answers: AnswerQueue,
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
            budget_message: Mutex::new(None),
            // Child of the run signal: caller aborts propagate; `done`
            // cancels this to stop unawaited tools (pi: "aborts unawaited
            // calls when the script returns").
            tool_signal: signal.child_token(),
            done_parts: Mutex::new(None),
            fail_result: Mutex::new(None),
            decided: AtomicBool::new(false),
            on_call: Mutex::new(None),
            on_idle: Mutex::new(None),
            on_started: Mutex::new(None),
            clock: Arc::new(Mutex::new(VirtualClock::new(0))),
            rng: Arc::new(Mutex::new(Mulberry32::new(1))),
            held: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(1),
            last_settled_seq: AtomicU64::new(0),
            answers: Arc::new((AsyncMutex::new(VecDeque::new()), Notify::new())),
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

    /// Assembles the final result. Call AFTER `cancel_outstanding()` so the
    /// calls snapshot includes cancelled entries.
    fn assemble(&self, ok: bool, payload: Option<String>, writes: Option<String>) -> RunResult {
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
                            if let (Some(k), Ok(parsed)) = (
                                key.as_str(),
                                serde_json::from_str::<Value>(json_text.as_str().unwrap()),
                            ) {
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
        self.decided.store(true, Ordering::Relaxed);
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

type OnCallSlot = Mutex<Option<Box<dyn Fn(HostCall) + Send>>>;
type OnIdleSlot = Mutex<Option<Box<dyn Fn(u64, u64) + Send>>>;
type OnStartedSlot = Mutex<Option<Box<dyn Fn(String) + Send>>>;

/// The bridge's raw `done` parts: (ok, payload JSON, writes JSON). (pi:
/// `done(ok, payloadJson, writesJson)`.)
type DoneParts = (bool, Option<String>, Option<String>);

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
            // Start mode: the host answers calls via deliver(); forward them
            // to on_call and return (no dispatch, no engine answer). Execute
            // mode has no on_call and calls dispatch below. Done/output
            // messages always dispatch.
            let forwarded = match (&message, shared.on_call.lock().unwrap().as_ref()) {
                (
                    BridgeMsg::Call {
                        id,
                        name,
                        args_json,
                    }
                    | BridgeMsg::Global {
                        id,
                        name,
                        args_json,
                    },
                    Some(on_call),
                ) => {
                    shared.record_issued(*id, name);
                    let args: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
                    on_call(HostCall {
                        id: *id,
                        target: if is_call {
                            CallTarget::Tool
                        } else {
                            CallTarget::Global
                        },
                        name: name.clone(),
                        args,
                    });
                    true
                }
                _ => false,
            };
            if forwarded {
                return;
            }
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
                    shared.decided.store(true, Ordering::Relaxed);
                    shared.tool_signal.cancel();
                    // Stored raw; assembled after the grace period so the
                    // calls snapshot includes cancelled entries.
                    *shared.done_parts.lock().unwrap() = Some((ok, payload, writes));
                }
            }
        },
    );
    bridge_fn.into_js(this)
}

// ---------------------------------------------------------------- execute ---

/// The dedicated VM thread's stack (pi: explicit worker stacks; see §5.1).
const VM_THREAD_STACK: usize = 16 * 1024 * 1024;

async fn run_execute(
    script: String,
    sandbox: SandboxOptions,
    options: ExecuteOptions,
) -> RunResult {
    // The whole VM lifecycle runs on a dedicated thread with a large stack
    // and its own single-threaded runtime; the caller just awaits the result.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<RunResult>();
    let shared = Shared::new(&options.signal.clone().unwrap_or_default());
    let run_signal = options.signal.clone().unwrap_or_default();
    std::thread::Builder::new()
        .name("codemode-vm".into())
        .stack_size(VM_THREAD_STACK)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("vm runtime");
            let deadline_override = options.timeout_ms;
            let result = rt.block_on(run_vm(
                script,
                sandbox,
                shared,
                run_signal,
                deadline_override,
                options.store.clone(),
                None,
            ));
            let _ = done_tx.send(result);
        })
        .expect("spawn codemode-vm");
    match done_rx.await {
        Ok(result) => result,
        // The VM thread panicked.
        Err(_) => {
            let mut fail = shared_fail_static(ErrorKind::Sandbox, "the VM thread panicked");
            fail.output = Vec::new();
            fail
        }
    }
}

fn shared_fail_static(kind: ErrorKind, message: &str) -> RunResult {
    RunResult {
        outcome: Err(ScriptError {
            kind,
            name: None,
            message: message.to_owned(),
            stack: None,
        }),
        output: vec![],
        calls: vec![],
        during: None,
    }
}

/// Host-controlled run configuration (SPEC §5.0b mode 2): the determinism
/// shim is enabled by this config's presence, and the JSON declarations
/// (tools/globals/store) are prepared by the host.
struct StartCfg {
    tools_json: String,
    globals_json: String,
    store_json: String,
}

async fn run_vm(
    script: String,
    sandbox: SandboxOptions,
    shared: Arc<Shared>,
    signal: CancellationToken,
    timeout_ms: Option<u64>,
    store: BTreeMap<String, Value>,
    start: Option<StartCfg>,
) -> RunResult {
    let run_signal = signal.clone();
    // Tool lookup: by canonical name or normalized js name (pi exposes both).
    let mut by_name: HashMap<String, Tool> = HashMap::new();
    // Declarations: start mode uses the run's DECLARED tools (name + jsName
    // only — the host answers calls); execute mode uses the sandbox's tool
    // closures.
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

    let deadline = timeout_ms
        .or(sandbox.timeout_ms)
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    // One answer queue shared with the RunHandle: execute-mode dispatch
    // answers flow through it, and start-mode deliveries notify the pump
    // through it (VmRun::deliver pushes into `shared.held` and notifies this
    // same Notify).
    let answers: AnswerQueue = Arc::clone(&shared.answers);

    let runtime = AsyncRuntime::new().expect("rquickjs runtime");
    runtime
        .set_memory_limit(sandbox.memory_limit_bytes.unwrap_or(0) as usize)
        .await;
    runtime
        .set_max_stack_size(crate::fingerprint::MAX_STACK_SIZE)
        .await;
    {
        let run_signal = signal.clone();
        let shared = Arc::clone(&shared);
        // pi inline.ts: the interrupt budget counts handler polls and never
        // resets, so it accumulates across waits on tools.
        let budget_polls = Arc::new(AtomicU64::new(0));
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
                // pi inline.ts:79-94: `if (++polls <= interruptBudget) return false;`
                // else fail with `Execution exceeded its interrupt budget of N`.
                if let Some(budget) = sandbox.interrupt_budget {
                    let polls = budget_polls.fetch_add(1, Ordering::Relaxed) + 1;
                    if polls > budget {
                        let mut slot = shared.budget_message.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(format!(
                                "Execution exceeded its interrupt budget of {budget}"
                            ));
                        }
                        return true;
                    }
                }
                shared.finished.load(Ordering::Relaxed)
            })))
            .await;
    }
    let ctx = AsyncContext::full(&runtime)
        .await
        .expect("rquickjs context");

    // Setup, pump, and tick loop all run INSIDE one async_with: awaiting the
    // script's promise future drives the VM's job queue (v1's mechanism), and
    // the tick timeout detects a never-settling script (pi `stalled()`).
    let is_start = start.is_some();
    let result: RunResult = ctx
        .async_with(async |vctx| {
            let setup: Result<rquickjs::Promise, String> = (|| {
                // Fingerprint precedes any call (§5.5).
                if let Some(on_started) = shared.on_started.lock().unwrap().as_ref() {
                    on_started(fingerprint(sandbox.memory_limit_bytes));
                }
                // The determinism shim: virtual clock Date/performance, seeded
                // Math.random, no WeakRef/FinalizationRegistry, UTC-forced
                // local-time methods. Runs BEFORE the prelude so the lockdown
                // freezes the shimmed intrinsics.
                if start.is_some() {
                    let mut eval_opts = rquickjs::context::EvalOptions::default();
                    eval_opts.filename = Some("codemode-determinism-shim.js".into());
                    let (): () = vctx
                        .eval_with_options(
                            r#"(function () {
							const RealDate = Date;
							class ShimDate extends RealDate {
								constructor(...args) {
									if (args.length === 0) super(__pi_clock_now());
									else super(...args);
								}
								static now() { return __pi_clock_now(); }
							}
							for (const [local, utc] of [
								["getHours", "getUTCHours"], ["getMinutes", "getUTCMinutes"],
								["getSeconds", "getUTCSeconds"], ["getMilliseconds", "getUTCMilliseconds"],
								["getDate", "getUTCDate"], ["getDay", "getUTCDay"],
								["getFullYear", "getUTCFullYear"], ["getMonth", "getUTCMonth"],
							]) {
								ShimDate.prototype[local] = RealDate.prototype[utc];
							}
							ShimDate.prototype.getTimezoneOffset = () => 0;
							ShimDate.UTC = RealDate.UTC;
							ShimDate.parse = RealDate.parse;
							globalThis.Date = ShimDate;
							globalThis.performance = { now: () => __pi_clock_elapsed() };
							globalThis.Math.random = () => __pi_random();
							globalThis.WeakRef = undefined;
							globalThis.FinalizationRegistry = undefined;
						})();"#,
                            eval_opts,
                        )
                        .map_err(|e| format!("determinism shim failed: {e}"))?;
                    let clock = Arc::clone(&shared.clock);
                    let rng = Arc::clone(&shared.rng);
                    vctx
                        .globals()
                        .set(
                            "__pi_clock_now",
                            rquickjs::function::Func::from(move || {
                                clock.lock().unwrap().now() as f64
                            }),
                        )
                        .map_err(|e| format!("shim native failed: {e}"))?;
                    let clock = Arc::clone(&shared.clock);
                    vctx
                        .globals()
                        .set(
                            "__pi_clock_elapsed",
                            rquickjs::function::Func::from(move || {
                                clock.lock().unwrap().elapsed() as f64
                            }),
                        )
                        .map_err(|e| format!("shim native failed: {e}"))?;
                    let rng = Arc::clone(&rng);
                    vctx
                        .globals()
                        .set(
                            "__pi_random",
                            rquickjs::function::Func::from(move || rng.lock().unwrap().next_f64()),
                        )
                        .map_err(|e| format!("shim native failed: {e}"))?;
                }

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
                let tools_json = start
                    .as_ref()
                    .map(|start| start.tools_json.clone())
                    .unwrap_or_else(|| json!(declarations).to_string());
                let globals_json = start
                    .as_ref()
                    .map(|start| start.globals_json.clone())
                    .unwrap_or_else(|| json!(global_declarations).to_string());
                // pi: storeJson is a snapshot of JSON TEXT — each value
                // crosses as its JSON encoding, and the prelude parses on
                // `load`. Start mode uses the host-prepared snapshot.
                let store_json = start
                    .as_ref()
                    .map(|start| start.store_json.clone())
                    .unwrap_or_else(|| {
                        let store_text: BTreeMap<&String, String> = store
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k,
                                    serde_json::to_string(v)
                                        .unwrap_or_else(|_| "null".into()),
                                )
                            })
                            .collect();
                        json!(store_text).to_string()
                    });
                let api_obj: rquickjs::Object = prelude_fn
                    .call((bridge, tools_json, globals_json, store_json))
                    .map_err(|e| format!("prelude init failed: {e}"))?;
                let settle: rquickjs::Function =
                    api_obj.get("settle").map_err(|e| e.to_string())?;
                vctx
                    .globals()
                    .set("__pi_settle", settle)
                    .map_err(|e| e.to_string())?;
                let stalled: rquickjs::Function =
                    api_obj.get("stalled").map_err(|e| e.to_string())?;
                vctx
                    .globals()
                    .set("__pi_stalled", stalled)
                    .map_err(|e| e.to_string())?;
                let mut eval_opts = rquickjs::context::EvalOptions::default();
                // pi runs scripts as sloppy-mode global code; rquickjs's
                // default eval is strict, which would make silent built-in
                // patch assignments throw instead (pi escape-hatches case).
                eval_opts.strict = false;
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
                    // A done may already be stored (output-limit failures fire
                    // `done` from inside run's synchronous prefix) — prefer it,
                    // then classify interrupts by cause.
                    if let Some((ok, payload, writes)) = shared.done_parts.lock().unwrap().take() {
                        return shared.assemble(ok, payload, writes);
                    }
                    if shared.deadline_hit.load(Ordering::Relaxed) {
                        let fail = shared.fail(ErrorKind::Timeout, "the deadline expired");
                        *shared.fail_result.lock().unwrap() = Some(fail.clone());
                        return fail;
                    }
                    if shared.abort_hit.load(Ordering::Relaxed) {
                        let fail = shared.fail(ErrorKind::Aborted, "the run was aborted");
                        *shared.fail_result.lock().unwrap() = Some(fail.clone());
                        return fail;
                    }
                    // The budget interrupt can surface from run's synchronous
                    // prefix itself (a script that computes before its first
                    // await throws inside `run.call`) — pi classifies it as
                    // the budget timeout (inline.ts:94).
                    if let Some(message) = shared.budget_message.lock().unwrap().clone() {
                        let fail = shared.fail(ErrorKind::Timeout, &message);
                        *shared.fail_result.lock().unwrap() = Some(fail.clone());
                        return fail;
                    }
                    let fail = shared.fail(ErrorKind::Sandbox, &e);
                    *shared.fail_result.lock().unwrap() = Some(fail.clone());
                    return fail;
                }
            });

            // The pump: settle answers in seq order. Execute mode: tool
            // dispatch answers arrive through the queue; start mode: host
            // deliveries arrive through `held` (seq-ordered; a gap waits for
            // the missing number — pi: "after a gap it waits").
            let mut pump = Box::pin(async {
                let (queue, notify) = &*answers;
                loop {
                    // Execute-mode queue answers settle immediately.
                    let queue_answer = { queue.lock().await.pop_front() };
                    if let Some(answer) = queue_answer {
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
                        continue;
                    }

                    // Start mode: wait for deliveries; apply strictly in seq
                    // order, advancing the clock while each result processes.
                    if start.is_some() {
                        let next = shared.next_seq.load(Ordering::Relaxed);
                        let held_entry = shared.held.lock().unwrap().get(&next).cloned();
                        let Some((now, id, outcome)) = held_entry else {
                            notify.notified().await;
                            continue;
                        };
                        shared.last_settled_seq.store(next, Ordering::Relaxed);
                        shared.clock.lock().unwrap().advance_to(now);
                        let Some(settle) = vctx
                            .globals()
                            .get::<_, rquickjs::Function>("__pi_settle")
                            .ok()
                        else {
                            return;
                        };
                        let settled = match outcome {
                            Outcome::Ok(value) => {
                                let payload = serde_json::to_string(&value)
                                    .unwrap_or_else(|_| "null".into());
                                settle.call::<_, ()>((id as f64, true, payload, Undefined))
                            }
                            Outcome::Err { message, properties } => {
                                let props_json = properties
                                    .as_ref()
                                    .map(|props| serde_json::to_string(props).unwrap())
                                    .unwrap_or_else(|| "undefined".into());
                                settle.call::<_, ()>((
                                    id as f64,
                                    false,
                                    message,
                                    props_json,
                                ))
                            }
                        };
                        let _ = settled;
                        shared.next_seq.store(next + 1, Ordering::Relaxed);
                        continue;
                    }

                    // Execute mode: nothing queued — wait.
                    notify.notified().await;
                }
            });

            // The tick loop. Each iteration is pi's `drain()` (vm.ts:243-250):
            // run queued jobs, then ask the prelude's `stalled()` — done fires
            // immediately when the script waits on nothing that can resume it
            // (no tool call pending, no timers), instead of a wall-clock idle
            // heuristic.
            let tick = Duration::from_millis(25);
            let done_parts: Option<DoneParts> = loop {
                // Stop-arms first (pi: an interrupt is uncatchable — a budget
                // exhaustion rewrites any race it wins, inline.ts:94).
                if run_signal.is_cancelled() {
                    let fail = shared.fail(ErrorKind::Aborted, "the run was aborted");
                    *shared.fail_result.lock().unwrap() = Some(fail);
                    break None;
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    let fail = shared.fail(ErrorKind::Timeout, "the deadline expired");
                    *shared.fail_result.lock().unwrap() = Some(fail);
                    break None;
                }
                if let Some(message) = shared.budget_message.lock().unwrap().clone() {
                    let fail = shared.fail(ErrorKind::Timeout, &message);
                    *shared.fail_result.lock().unwrap() = Some(fail);
                    break None;
                }
                // Drive the VM's pending jobs (promise reactions: the body,
                // the `then` that fires `done`, tool continuations).
                while vctx.execute_pending_job() {}
                // The budget interrupt fired during the drain (pi: a failed
                // job is the interrupt — the budget rewrites it, inline.ts:94).
                if let Some(message) = shared.budget_message.lock().unwrap().clone() {
                    let fail = shared.fail(ErrorKind::Timeout, &message);
                    *shared.fail_result.lock().unwrap() = Some(fail);
                    break None;
                }
                // pi vm.ts:249: after the drain, `stalled()` — it calls done
                // internally when the script waits on nothing that can resume
                // it; pending tool calls keep it harmless.
                let stalled = vctx
                    .globals()
                    .get::<_, rquickjs::Function>("__pi_stalled")
                    .ok()
                    .and_then(|stalled| stalled.call::<_, bool>(()).ok())
                    .unwrap_or(false);
                if stalled
                    && let Some(parts) = shared.done_parts.lock().unwrap().take()
                {
                    break Some(parts);
                }
                // `done` through the bridge?
                if let Some(parts) = shared.done_parts.lock().unwrap().take() {
                    break Some(parts);
                }
                tokio::select! {
                    settled = &mut script_future => {
                        // The script promise settled — Ok (script returned) or
                        // Err (an interrupt/deadline abort propagated as a
                        // rejection). Classify by the stop flags; without a
                        // flag, a settled promise with no call in flight is
                        // waiting on something that can never settle.
                        let failed = settled.is_err();
                        let fail_message = match &settled {
                            Err(err) => err.to_string(),
                            Ok(_) => String::new(),
                        };
                        if shared.deadline_hit.load(Ordering::Relaxed) {
                            let fail =
                                shared.fail(ErrorKind::Timeout, "the deadline expired");
                            *shared.fail_result.lock().unwrap() = Some(fail);
                            break None;
                        }
                        if shared.abort_hit.load(Ordering::Relaxed) {
                            let fail =
                                shared.fail(ErrorKind::Aborted, "the run was aborted");
                            *shared.fail_result.lock().unwrap() = Some(fail);
                            break None;
                        }
                        if let Some(message) = shared.budget_message.lock().unwrap().clone() {
                            let fail = shared.fail(ErrorKind::Timeout, &message);
                            *shared.fail_result.lock().unwrap() = Some(fail);
                            break None;
                        }
                        if let Some(parts) = shared.done_parts.lock().unwrap().take() {
                            break Some(parts);
                        }
                        if failed {
                            let fail = shared.fail(ErrorKind::Script, &fail_message);
                            *shared.fail_result.lock().unwrap() = Some(fail);
                            break None;
                        }
                    }
                    _ = &mut pump => {
                        let fail = shared.fail(ErrorKind::Sandbox, "the answer pump stopped");
                        *shared.fail_result.lock().unwrap() = Some(fail);
                        break None;
                    }
                    _ = tokio::time::sleep(tick) => {
                        // Idle tick: no progress in 25 ms. Fire onIdle while
                        // calls wait (pi: onIdle per drain).
                        if is_start
                            && !shared.outstanding.lock().unwrap().is_empty()
                            && let Some(on_idle) = shared.on_idle.lock().unwrap().as_ref()
                        {
                            on_idle(
                                shared.last_settled_seq.load(Ordering::Relaxed),
                                shared.issued.load(Ordering::Relaxed),
                            );
                        }
                        if shared.finished.load(Ordering::Relaxed)
                            && let Some(parts) = shared.done_parts.lock().unwrap().take()
                        {
                            break Some(parts);
                        }
                    }
                }
            };

            // Grace period: cancelled in-flight tool tasks observe their abort
            // signal on the VM runtime; keep it alive briefly so they settle
            // (pi keeps the worker alive until calls settle).
            let grace = Instant::now() + Duration::from_millis(100);
            while !shared.outstanding.lock().unwrap().is_empty() && Instant::now() < grace {
                while vctx.execute_pending_job() {}
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            shared.cancel_outstanding();
            let mut vm_result = match done_parts {
                Some((ok, payload, writes)) => shared.assemble(ok, payload, writes),
                None => match shared.fail_result.lock().unwrap().take() {
                    Some(fail) => fail,
                    None => shared.fail(
                        ErrorKind::Script,
                        "script waits on a promise that can never settle (no timers; no tool call pending)",
                    ),
                },
            };
            // Start runs always report `during` (pi: "0 for its first run").
            if is_start {
                vm_result.during = Some(During {
                    seq: shared.last_settled_seq.load(Ordering::Relaxed),
                    issued: shared.issued.load(Ordering::Relaxed),
                });
            }
            vm_result
        })
        .await;
    shared.cancel_outstanding();
    result
}

// ------------------------------------------------------------- start mode ---

fn run_start(script: String, sandbox: SandboxOptions, options: RunOptions) -> Box<dyn RunHandle> {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<RunResult>();
    let shared = Shared::new(&options.signal.clone().unwrap_or_default());
    let vm_shared = Arc::clone(&shared);
    // Deterministic state: the virtual clock starts at `started_at`; the PRNG
    // seeds from `seed ^ 0x9e3779b9` (pi seeds its WASI prng with the same
    // XOR of the golden ratio).
    {
        *shared.clock.lock().unwrap() = VirtualClock::new(options.started_at);
        *shared.rng.lock().unwrap() = Mulberry32::new(options.seed ^ 0x9e3779b9);
    }
    let options_on_call = options.on_call;
    {
        let mut on_call = shared.on_call.lock().unwrap();
        *on_call = Some(Box::new(move |call: HostCall| {
            (options_on_call)(call);
        }));
    }
    if let Some(on_started) = options.on_started {
        *shared.on_started.lock().unwrap() = Some(on_started);
    }
    if let Some(on_idle) = options.on_idle {
        *shared.on_idle.lock().unwrap() = Some(on_idle);
    }

    let tools_json = json!(options
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "jsName": tool.js_name.clone().unwrap_or_else(|| to_codemode_identifier(&tool.name)),
                "description": tool.description.clone().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>())
    .to_string();
    let globals_json = json!(
        options
            .globals
            .iter()
            .map(|global| json!({"name": global.name, "spread": global.spread}))
            .collect::<Vec<_>>()
    )
    .to_string();
    let store_json = json!(
        options
            .store
            .iter()
            .map(|(k, v)| (
                k.clone(),
                serde_json::to_string(v).unwrap_or_else(|_| "null".into())
            ))
            .collect::<BTreeMap<_, _>>()
    )
    .to_string();
    let store: BTreeMap<String, Value> = options
        .store
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let vm_signal = options.signal.clone().unwrap_or_default();
    let vm_timeout = options.timeout_ms;

    std::thread::Builder::new()
        .name("codemode-vm".into())
        .stack_size(VM_THREAD_STACK)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("vm runtime");
            let result = rt.block_on(run_vm(
                script,
                sandbox,
                vm_shared,
                vm_signal,
                vm_timeout,
                store,
                Some(StartCfg {
                    tools_json,
                    globals_json,
                    store_json,
                }),
            ));
            let _ = done_tx.send(result);
        })
        .expect("spawn codemode-vm");

    Box::new(VmRun {
        shared: Arc::clone(&shared),
        done_rx: Mutex::new(Some(done_rx)),
    })
}
/// A running [`Engine::start`] execution. (pi: `CodemodeRun`.)
struct VmRun {
    shared: Arc<Shared>,
    done_rx: Mutex<Option<tokio::sync::oneshot::Receiver<RunResult>>>,
}

impl RunHandle for VmRun {
    fn result(&mut self) -> BoxFuture<'_, RunResult> {
        let shared = Arc::clone(&self.shared);
        let done_rx = self.done_rx.lock().unwrap().take();
        Box::pin(async move {
            if let Some(done_rx) = done_rx
                && let Ok(result) = done_rx.await
            {
                return result;
            }
            shared.fail(
                ErrorKind::Sandbox,
                "the VM ended without reporting a result",
            )
        })
    }

    /// Whether the run has its outcome: decided before `result` settles, and
    /// stays decided after the parts are consumed (pi: "is finished once it
    /// has its outcome").
    fn finished(&self) -> bool {
        self.shared.decided.load(Ordering::Relaxed)
            || self.shared.done_parts.lock().unwrap().is_some()
            || self.shared.fail_result.lock().unwrap().is_some()
    }

    /// Answer call `id` as result number `seq`. Results apply strictly in seq
    /// order, whatever order they arrive in; while one is processed the clock
    /// reads `now`, or the previous value if `now` is earlier. A finished run
    /// accepts deliveries as no-ops (pi: after abort, deliver does not
    /// throw).
    fn deliver(&self, seq: u64, now: u64, id: u64, outcome: Outcome) -> Result<(), DeliverError> {
        // A finished run: the VM is gone; deliveries are no-ops (pi: does not
        // throw after the caller aborts).
        if self.finished() {
            return Ok(());
        }
        let mut held = self.shared.held.lock().unwrap();
        // The call must be waiting (pi: "Call 99 is not waiting").
        if !self.shared.outstanding.lock().unwrap().contains_key(&id) {
            return Err(DeliverError::NotWaiting);
        }
        // The result number must not be reused (pi: "used once").
        let next_seq = self.shared.next_seq.load(Ordering::Relaxed);
        if seq < next_seq || held.contains_key(&seq) {
            return Err(DeliverError::SeqUsed);
        }
        held.insert(seq, (now, id, outcome));
        drop(held);
        self.shared.answers.1.notify_one();
        Ok(())
    }
}

// Deterministic phase will consume: the virtual clock and PRNG (§5.4).
const _: Option<fn(u64, u32)> = Some(|_started_at, _seed| {
    let _ = VirtualClock::new(0);
    let _ = Mulberry32::new(0);
});
