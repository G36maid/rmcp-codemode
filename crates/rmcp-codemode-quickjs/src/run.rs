//! The run loop: script execution and host-call routing.
//!
//! The dispatcher abstraction keeps one execution path for all three modes:
//! one-shot (calls execute inline), host-controlled (calls yield to the
//! host), and replay (recorded outcomes answer in order, unrecorded calls
//! fall through to inline execution).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use rmcp_codemode::{ToolHandle, Value};
use serde_json::json;

/// Decrement the in-flight counter when a dispatched call ends.
struct InFlightGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What a dispatcher does with one tool call.
#[derive(Clone)]
pub(crate) enum Dispatcher {
    /// Execute the tool now (one-shot runs).
    Inline,
    /// Yield to the host: send the call, wait for the answer on the
    /// deliveries channel.
    Host(Arc<HostChannel>),
    /// Answer from the recording (by issue order); calls past the recording
    /// yield to the wrapped dispatcher (boxed to break the async recursion).
    Replay {
        state: Arc<Mutex<ReplayState>>,
        past: PastDispatcher,
    },
}

/// The dispatcher for calls past a recording, boxed to break async
/// recursion ([`Dispatcher::dispatch`] is recursive through this).
pub(crate) type PastDispatcher = Arc<
    dyn Fn(ToolHandle, Value) -> futures::future::BoxFuture<'static, Result<Value, String>>
        + Send
        + Sync,
>;

/// The host channel pair for a suspending run.
pub(crate) struct HostChannel {
    pub calls: futures::channel::mpsc::UnboundedSender<HostCall>,
    /// In-flight counter for the never-settling detector (a yielded call
    /// is a pending tool call; waiting for the host is legal).
    pub in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// Answers pushed by the host, consumed by the dispatcher (a deque, so
    /// no lock is ever held across an await).
    pub answers: Arc<Mutex<std::collections::VecDeque<(u64, rmcp_codemode::Outcome)>>>,
    /// Ids for yielded calls, unique within the run.
    pub next_id: std::sync::atomic::AtomicU64,
}

/// Replay bookkeeping: recorded calls in issue order.
pub(crate) struct ReplayState {
    pub recorded: Vec<rmcp_codemode::AnsweredCall>,
    pub issued: u64,
}

use rmcp_codemode::engine::HostCall;

impl Dispatcher {
    /// Route one call. Returns the script-visible outcome.
    pub(crate) async fn dispatch(&self, tool: &ToolHandle, args: Value) -> Result<Value, String> {
        match self {
            Self::Inline => (tool.execute)(args).await,
            Self::Host(channel) => {
                // A yielded call is a pending tool call: the never-settling
                // detector must not fire while the host decides.
                channel.in_flight.fetch_add(1, Ordering::Relaxed);
                let _guard = InFlightGuard(Arc::clone(&channel.in_flight));
                let id = channel.next_id.fetch_add(1, Ordering::Relaxed);
                channel
                    .calls
                    .unbounded_send(HostCall::new(
                        id,
                        tool.canonical(),
                        args,
                        Arc::clone(&tool.execute),
                    ))
                    .map_err(|_| "the host stopped answering".to_owned())?;
                loop {
                    // The answer queue is a plain mutex-protected deque:
                    // pop under the lock, never hold it across an await.
                    let popped = channel.answers.lock().unwrap().pop_front();
                    match popped {
                        Some((answered_id, rmcp_codemode::Outcome::Deliver(value)))
                            if answered_id == id =>
                        {
                            return Ok(value);
                        }
                        Some((answered_id, rmcp_codemode::Outcome::Fail { message, .. }))
                            if answered_id == id =>
                        {
                            return Err(message);
                        }
                        Some(_) => continue,
                        None => {}
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
            Self::Replay { state, past } => {
                // Read and advance under the lock; never hold the guard
                // across the tool await.
                let recorded = {
                    let mut guard = state.lock().unwrap();
                    let index = guard.issued as usize;
                    guard.issued += 1;
                    guard.recorded.get(index).cloned()
                };
                let Some(answered) = recorded else {
                    // Past the recording: yield to the wrapped dispatcher
                    // (a resumed run stays host-controlled so it can pause
                    // a second time).
                    return past(tool.clone(), args).await;
                };
                // RecordedOutcome is non_exhaustive: inspect through its
                // serialized (externally tagged) shape so unknown variants
                // degrade to a failure rather than a panic.
                let json = serde_json::to_value(&answered.outcome).unwrap_or(Value::Null);
                match json.as_object().map(|object| object.keys().next().cloned()) {
                    Some(Some(key)) if key == "ok" => {
                        Ok(json.get("ok").cloned().unwrap_or(Value::Null))
                    }
                    Some(Some(key)) if key == "err" => Err(json
                        .get("err")
                        .and_then(Value::as_str)
                        .unwrap_or("recorded failure")
                        .to_owned()),
                    _ => Err("recorded outcome of an unknown shape".to_owned()),
                }
            }
        }
    }
}

/// Execute the script and drive its tool calls through `dispatcher`.
///
/// Returns the result envelope (`{ ok, value | error }`). A swallowed
/// `exit()` resolves the run successfully with no value; a run waiting on a
/// promise nothing can settle fails immediately.
pub(crate) async fn execute_script(
    ctx: rquickjs::Ctx<'_>,
    code: &str,
    exit_requested: &Arc<AtomicBool>,
    deadline_instant: Option<std::time::Instant>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<Value, rmcp_codemode::EngineError> {
    // The IIFE's result crosses as a JSON envelope string. A thrown
    // `{ __codemode_exit: true }` sentinel (from `exit()`) resolves the run
    // successfully with no value.
    let wrapped = format!(
        "(async () => {{\n{code}\n}})().then(\n (v) => JSON.stringify({{ ok: true, value: v === undefined ? null : v }}),\n (e) => e && e.__codemode_exit\n   ? JSON.stringify({{ ok: true, value: null }})\n   : JSON.stringify({{ ok: false, error: e && e.message !== undefined ? String(e.message) : String(e) }})\n )"
    );
    if std::env::var("CODEMODE_DEBUG").is_ok() {
        eprintln!(
            "DBG execute_script entered, deadline={:?}",
            deadline_instant.is_some()
        );
    }
    let promise: rquickjs::Promise = match ctx.eval(wrapped.as_bytes()) {
        Ok(promise) => promise,
        Err(error) => {
            // A parse error surfaces as Exception with the pending exception
            // holding the message. A deadline interrupt during execution
            // surfaces the same way (QuickJS's InternalError("interrupted")
            // is uncatchable inside the VM, so it lands here) — classified
            // below.
            let message = if matches!(error, rquickjs::Error::Exception) {
                let caught = ctx.catch();
                describe_caught(&ctx, rquickjs::CaughtError::Value(caught))
            } else {
                error.to_string()
            };
            return Ok(failed_envelope(classify_deadline(
                message,
                deadline_instant,
            )));
        }
    };

    // Poll in short ticks so a never-settling promise fails immediately
    // (pi): with no pending tool call, nothing can resume the script.
    let mut future = Box::pin(promise.into_future::<String>());
    let tick = std::time::Duration::from_millis(25);
    let mut idle_ticks = 0u32;
    let envelope: String = loop {
        match tokio::time::timeout(tick, future.as_mut()).await {
            Ok(Ok(envelope)) => {
                break envelope;
            }
            Ok(Err(err)) => {
                if exit_requested.load(Ordering::Relaxed) {
                    return Ok(json_exit_envelope());
                }
                let message = if matches!(err, rquickjs::Error::Exception) {
                    let caught = ctx.catch();
                    describe_caught(&ctx, rquickjs::CaughtError::Value(caught))
                } else {
                    err.to_string()
                };
                return Ok(failed_envelope(classify_deadline(
                    message,
                    deadline_instant,
                )));
            }
            Err(_) => {
                if exit_requested.load(Ordering::Relaxed) {
                    return Ok(json_exit_envelope());
                }
                if in_flight.load(Ordering::Relaxed) == 0 {
                    idle_ticks += 1;
                    if idle_ticks >= 3 {
                        return Ok(failed_envelope(
                            "script waits on a promise that can never settle (no timers; no tool call pending)"
                                .to_owned(),
                        ));
                    }
                } else {
                    idle_ticks = 0;
                }
            }
        }
    };
    let mut envelope: Value = serde_json::from_str(&envelope)
        .map_err(|e| rmcp_codemode::EngineError::Engine(format!("bad result envelope: {e}")))?;
    // The IIFE's rejection handler can catch the interrupt as a normal JS
    // rejection; classify the envelope error too.
    if envelope.get("ok").and_then(Value::as_bool) == Some(false) {
        if let Some(message) = envelope.get("error").and_then(Value::as_str) {
            envelope["error"] = json!(classify_deadline(message.to_owned(), deadline_instant));
        }
    }
    // QuickJS's interrupt error is an InternalError("interrupted") that the
    // promise machinery surfaces as a normal rejection; when the deadline
    // expired, report the deadline (pi's timeout kind).
    Ok(envelope)
}

/// QuickJS's interrupt error is `InternalError("interrupted")`; when the
/// deadline expired, report the deadline (pi's timeout kind).
fn classify_deadline(message: String, deadline_instant: Option<std::time::Instant>) -> String {
    if std::env::var("CODEMODE_DEBUG").is_ok() {
        eprintln!(
            "DBG classify: message={:?} expired={}",
            message,
            deadline_instant.is_some_and(|d| std::time::Instant::now() >= d)
        );
    }
    if message == "interrupted" && deadline_instant.is_some_and(|d| std::time::Instant::now() >= d)
    {
        "script exceeded its deadline".to_owned()
    } else {
        message
    }
}

fn json_exit_envelope() -> Value {
    serde_json::json!({ "ok": true, "value": null })
}

fn failed_envelope(message: String) -> Value {
    serde_json::json!({ "ok": false, "error": message })
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
                "script threw".to_owned()
            } else {
                message
            }
        }
        rquickjs::CaughtError::Value(value) => {
            if let Some(exc) = value
                .as_object()
                .and_then(|o| rquickjs::Exception::from_object(o.clone()))
            {
                exc.message().unwrap_or_else(|| "script threw".to_owned())
            } else {
                "script threw".to_owned()
            }
        }
        rquickjs::CaughtError::Error(err) => err.to_string(),
    }
}
