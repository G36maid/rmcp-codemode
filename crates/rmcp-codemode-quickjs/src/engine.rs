//! The engine implementations.
//!
//! [`QuickJsEngine`] runs one-shot scripts ([`Engine`]);
//! [`SuspendingQuickJsEngine`] adds the host-controlled mode
//! ([`SuspendingEngine`]): every tool call is yielded to the host through a
//! channel, the host answers with [`Outcome`]s, and the run can pause —
//! recording answered calls in a serializable [`SuspendState`] that a later
//! run replays (pi's `start()`/`deliver()` with CF's durable-log states).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use futures::StreamExt;
use rmcp_codemode::{
    Budget, Engine, EngineError, HostCall, Outcome, OutputItem, RunContext, RunHandle,
    ScriptOptions, ScriptResult, Space, SuspendState, SuspendingEngine, ToolHandle, Value,
    extract_options, render_result_text_with_overflow,
};
use rquickjs::{AsyncContext, AsyncRuntime};

use crate::run::Dispatcher;

/// The one-shot engine.
#[non_exhaustive]
pub struct QuickJsEngine {
    /// Where truncated output spills (pi: a temp file whose path is
    /// included in the result).
    pub overflow_dir: std::path::PathBuf,
}

impl Default for QuickJsEngine {
    fn default() -> Self {
        Self {
            overflow_dir: std::env::temp_dir(),
        }
    }
}

/// Extract the script's `@options` and apply them to the budget.
fn apply_options(code: &str, budget: &Budget) -> (String, Result<ScriptOptions, String>, Budget) {
    let (code, options) = extract_options(code);
    let applied = match &options {
        Ok(options) => {
            let mut budget = budget.clone();
            if let Some(tokens) = options.max_output_tokens {
                budget.max_output_tokens = tokens;
            }
            if let Some(ms) = options.timeout_ms {
                budget.timeout_ms = Some(ms);
            }
            budget
        }
        Err(_) => budget.clone(),
    };
    (code, options, applied)
}

impl QuickJsEngine {
    /// Core of every run: fresh VM, globals, script, and the deadline
    /// interrupt. `dispatcher` decides what a tool call does — execute
    /// inline (one-shot mode) or yield to the host (suspending mode).
    async fn run_with(
        &self,
        code: &str,
        context: &RunContext,
        dispatcher: Dispatcher,
    ) -> Result<ScriptResult, EngineError> {
        self.run_with_sink(code, context, dispatcher, None, Default::default())
            .await
    }

    /// [`run_with`](Self::run_with) with an explicit call sink (resume uses
    /// replay; one-shot and fresh suspending runs build theirs inline).
    async fn run_with_sink(
        &self,
        code: &str,
        context: &RunContext,
        dispatcher: Dispatcher,
        sink_override: Option<crate::proxy::AsyncCallSink>,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<ScriptResult, EngineError> {
        let started = std::time::Instant::now();
        let (code, options, budget) = apply_options(code, &context.budget);
        if let Err(message) = options {
            return Ok(ScriptResult::failed(
                message,
                context.budget.max_output_tokens,
                started.elapsed().as_millis() as u64,
            ));
        }
        let deadline_instant = budget
            .timeout_ms
            .map(|ms| started + std::time::Duration::from_millis(ms));

        let output: Arc<Mutex<Vec<OutputItem>>> = Arc::new(Mutex::new(Vec::new()));
        let calls: Arc<Mutex<Vec<rmcp_codemode::CallRecord>>> = Arc::new(Mutex::new(Vec::new()));
        // pi's store: reads see the initial snapshot, writes buffer and
        // commit only on success.
        let committed_store: Arc<Mutex<BTreeMap<String, Value>>> =
            Arc::new(Mutex::new(context.store.clone()));
        let initial_store = Arc::clone(&committed_store);
        let pending_store: Arc<Mutex<BTreeMap<String, Value>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let call_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let exit_requested = Arc::new(AtomicBool::new(false));

        let runtime = AsyncRuntime::new().map_err(|e| EngineError::Engine(e.to_string()))?;
        runtime.set_memory_limit(budget.memory_limit).await;
        runtime.set_max_stack_size(1024 * 1024).await;
        {
            let exit_requested = Arc::clone(&exit_requested);
            runtime
                .set_interrupt_handler(Some(Box::new(move || {
                    exit_requested.load(Ordering::Relaxed)
                        || deadline_instant.is_some_and(|d| std::time::Instant::now() >= d)
                })))
                .await;
        }
        let ctx = AsyncContext::full(&runtime)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))?;

        let run = async {
            let envelope: Value = ctx
                .async_with(async |vctx| {
                    let sink: crate::proxy::AsyncCallSink = match &sink_override {
                        Some(sink) => Arc::clone(sink),
                        None => {
                            let sink_dispatcher = dispatcher.clone();
                            Arc::new(move |tool: ToolHandle, args: Value| {
                                let dispatcher = sink_dispatcher.clone();
                                Box::pin(async move { dispatcher.dispatch(&tool, args).await })
                            })
                        }
                    };
                    let result = crate::proxy::setup(
                        &vctx,
                        &context.space,
                        &namespace_metas(&context.space),
                        &budget,
                        &output,
                        &calls,
                        &call_counter,
                        &exit_requested,
                        &sink,
                        &initial_store,
                        &pending_store,
                    );
                    if let Err(error) = result {
                        return Err(EngineError::Engine(error.to_string()));
                    }
                    crate::run::execute_script(
                        vctx,
                        &code,
                        &exit_requested,
                        deadline_instant,
                        in_flight,
                    )
                    .await
                })
                .await?;
            Ok::<Value, EngineError>(envelope)
        };
        let envelope = match budget.timeout_ms {
            Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), run)
                .await
                .map_err(|_| EngineError::Engine("script exceeded its deadline".to_owned()))??,
            None => run.await?,
        };

        let mut result = finalize(envelope, &output, &calls, &pending_store, &committed_store);
        if exit_requested.load(Ordering::Relaxed) {
            result.ok = true;
            result.error = None;
            result.value = None;
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        result.max_output_tokens = budget.max_output_tokens;
        // Truncation spill is the engine's job (pi writes the full text and
        // names its path).
        let text = render_result_text_with_overflow(&result, &self.overflow_dir);
        result.full_output_path = text
            .lines()
            .find(|line| line.starts_with("Full output: "))
            .map(|line| line["Full output: ".len()..].to_owned());
        Ok(result)
    }
}

/// The suspending engine: one type implementing both [`Engine`] and
/// [`SuspendingEngine`], plus replay-based resumption (pi's model: a resumed
/// run replays recorded outcomes and continues).
pub struct SuspendingQuickJsEngine {
    inner: QuickJsEngine,
}

impl SuspendingQuickJsEngine {
    /// A suspending engine with the overflow directory of the given engine.
    pub fn from_engine(engine: QuickJsEngine) -> Self {
        Self { inner: engine }
    }

    /// Resume a suspended run: recorded outcomes answer the script's calls
    /// in issue order; calls past the recording execute for real. The
    /// returned handle is host-controlled again, so a resumed run can pause
    /// a second time.
    pub async fn resume(
        &self,
        state: SuspendState,
        context: &RunContext,
    ) -> Result<RunHandle, EngineError> {
        let (call_tx, call_rx) = futures::channel::mpsc::unbounded::<HostCall>();
        let (delivery_tx, delivery_rx) = futures::channel::mpsc::unbounded::<(u64, Outcome)>();
        let (result_tx, result_rx) = futures::channel::oneshot::channel::<ScriptResult>();
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let answer_deque: Arc<Mutex<std::collections::VecDeque<(u64, Outcome)>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let pump_sink = Arc::clone(&answer_deque);
        tokio::spawn(async move {
            let mut delivery_rx = delivery_rx;
            while let Some(answer) = delivery_rx.next().await {
                pump_sink.lock().unwrap().push_back(answer);
            }
        });

        let replay = resumed_dispatcher(
            crate::run::ReplayState {
                recorded: state.answered,
                issued: 0,
            },
            call_tx.clone(),
            Arc::clone(&answer_deque),
            Arc::clone(&in_flight),
        );
        let sink_dispatcher = replay.clone();
        let engine = QuickJsEngine {
            overflow_dir: self.inner.overflow_dir.clone(),
        };
        let budget = context.budget.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let sink: crate::proxy::AsyncCallSink =
                Arc::new(move |tool: ToolHandle, args: Value| {
                    let dispatcher = sink_dispatcher.clone();
                    Box::pin(async move { dispatcher.dispatch(&tool, args).await })
                });
            let result = engine
                .run_with_sink(&state.code, &context, replay, Some(sink), in_flight)
                .await;
            let outcome = match result {
                Ok(result) => result,
                Err(error) => ScriptResult::failed(error.to_string(), budget.max_output_tokens, 0),
            };
            let _ = result_tx.send(outcome);
        });
        Ok(RunHandle::new(call_rx, delivery_tx, result_rx))
    }
}

/// Build the dispatcher for a resumed run: the recording answers first,
/// then the host channel takes over.
fn resumed_dispatcher(
    state: crate::run::ReplayState,
    call_tx: futures::channel::mpsc::UnboundedSender<HostCall>,
    answers: Arc<Mutex<std::collections::VecDeque<(u64, Outcome)>>>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
) -> Dispatcher {
    let host = Arc::new(crate::run::HostChannel {
        calls: call_tx,
        answers,
        next_id: std::sync::atomic::AtomicU64::new(0),
        in_flight,
    });
    let past: crate::run::PastDispatcher = Arc::new(move |tool: ToolHandle, args: Value| {
        let host = Arc::clone(&host);
        Box::pin(async move { Dispatcher::Host(host).dispatch(&tool, args).await })
    });
    Dispatcher::Replay {
        state: Arc::new(Mutex::new(state)),
        past,
    }
}

use std::collections::BTreeMap;

/// Namespace metadata for the run: declared namespaces plus every
/// intermediate segment of the tree (OpenCode: the shape is the namespace).
fn namespace_metas(space: &Space) -> BTreeMap<String, rmcp_codemode::NamespaceMeta> {
    let mut metas = BTreeMap::new();
    for key in space.namespace_keys() {
        if let Some(meta) = space.namespace(key) {
            metas.insert(key.clone(), meta.clone());
        }
    }
    metas
}

/// Build the final [`ScriptResult`] from the envelope and side channels.
#[allow(clippy::too_many_arguments)]
fn finalize(
    envelope: Value,
    output: &Arc<Mutex<Vec<OutputItem>>>,
    calls: &Arc<Mutex<Vec<rmcp_codemode::CallRecord>>>,
    pending_store: &Arc<Mutex<BTreeMap<String, Value>>>,
    committed_store: &Arc<Mutex<BTreeMap<String, Value>>>,
) -> ScriptResult {
    let ok = envelope.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let value = envelope.get("value").cloned().filter(|v| !v.is_null());
    let error = envelope
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|_| !ok);
    // Commit buffered store writes on success (pi's semantics).
    let store_writes = if ok {
        let mut pending = pending_store.lock().unwrap();
        let mut writes = rmcp_codemode::StoreWrites::default();
        for (key, value) in std::mem::take(&mut *pending) {
            if value.is_null() {
                writes.delete.push(key);
            } else {
                committed_store
                    .lock()
                    .unwrap()
                    .insert(key.clone(), value.clone());
                writes.set.push((key, value));
            }
        }
        writes
    } else {
        Default::default()
    };
    let mut result = ScriptResult::default();
    result.ok = ok;
    result.value = value;
    result.error = error;
    result.output = std::mem::take(&mut *output.lock().unwrap());
    result.calls = std::mem::take(&mut *calls.lock().unwrap());
    result.store_writes = store_writes;
    result
}

impl Engine for QuickJsEngine {
    async fn execute(&self, code: &str, context: &RunContext) -> Result<ScriptResult, EngineError> {
        self.run_with(code, context, Dispatcher::Inline).await
    }
}

impl Engine for SuspendingQuickJsEngine {
    async fn execute(&self, code: &str, context: &RunContext) -> Result<ScriptResult, EngineError> {
        Engine::execute(&self.inner, code, context).await
    }
}

impl SuspendingEngine for SuspendingQuickJsEngine {
    async fn start(&self, code: &str, context: &RunContext) -> Result<RunHandle, EngineError> {
        SuspendingEngine::start(&self.inner, code, context).await
    }
}

impl SuspendingEngine for QuickJsEngine {
    async fn start(&self, code: &str, context: &RunContext) -> Result<RunHandle, EngineError> {
        let (call_tx, call_rx) = futures::channel::mpsc::unbounded::<HostCall>();
        let (delivery_tx, delivery_rx) = futures::channel::mpsc::unbounded::<(u64, Outcome)>();
        let (result_tx, result_rx) = futures::channel::oneshot::channel::<ScriptResult>();
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Drive the run in the background: the dispatcher yields calls
        // through call_tx and waits for answers on delivery_rx.
        let engine_ctx = context.clone();
        let code = code.to_owned();
        // The host pushes answers through the RunHandle's mpsc sender; a
        // pump task forwards them into the dispatcher's deque (no lock held
        // across an await anywhere).
        let answer_deque: Arc<Mutex<std::collections::VecDeque<(u64, Outcome)>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let pump_sink = Arc::clone(&answer_deque);
        tokio::spawn(async move {
            let mut delivery_rx = delivery_rx;
            while let Some(answer) = delivery_rx.next().await {
                pump_sink.lock().unwrap().push_back(answer);
            }
        });
        let dispatcher_channel: Dispatcher = Dispatcher::Host(Arc::new(crate::run::HostChannel {
            calls: call_tx,
            answers: Arc::clone(&answer_deque),
            next_id: std::sync::atomic::AtomicU64::new(0),
            in_flight: Arc::clone(&in_flight),
        }));
        let engine = QuickJsEngine {
            overflow_dir: self.overflow_dir.clone(),
        };
        tokio::spawn(async move {
            let result = match engine
                .run_with(&code, &engine_ctx, dispatcher_channel)
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    ScriptResult::failed(error.to_string(), engine_ctx.budget.max_output_tokens, 0)
                }
            };
            let _ = result_tx.send(result);
        });

        Ok(RunHandle::new(call_rx, delivery_tx, result_rx))
    }
}
