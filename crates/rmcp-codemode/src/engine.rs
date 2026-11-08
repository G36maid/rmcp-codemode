//! The engine contract: what a script backend must honor.
//!
//! This module is the single home of the "pi clauses" — the lifecycle
//! semantics every backend implements once and every test verifies against
//! the contract, not an implementation:
//!
//! - Each run executes in a **fresh VM**; nothing leaks between runs.
//! - The script is the **body of an async function**: top-level `await` and
//!   `return` work.
//! - The only capability is calling tools through [`RunContext`]. No timers,
//!   network, file system, or host built-ins.
//! - A run waiting on a **promise nothing can settle** fails immediately
//!   (there are no timers to resume it); returning while a call is pending
//!   is not a stall.
//! - **Unawaited calls are cancelled** when the run ends — not waited out.
//! - The **deadline preempts pure-JS loops** via the VM's interrupt handler.
//! - Output caps ([`Budget`]) fail the run, keeping partial output.
//! - [`ScriptResult`] carries cancelled calls: calls are real and not undone.
//!
//! [`SuspendingEngine`] adds the host-controlled mode that approval and
//! replay build on (pi's `start()`/`deliver()`, CF's `runtime.decide`): the
//! engine yields every tool call to the host instead of executing it, so the
//! host can allow, deny, pause, or record.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    budget::{Budget, ScriptOptions},
    result::ScriptResult,
    space::{Space, ToolFn},
};

/// Everything one run needs: the tool space it may call, the resource
/// budgets, and the namespace metadata for discovery.
#[derive(Clone)]
#[non_exhaustive]
pub struct RunContext {
    /// The tools the script may call and the namespaces they belong to.
    pub space: Space,
    /// Resource budgets. The script's `@options` override
    /// [`Budget::max_output_tokens`] and [`Budget::timeout_ms`].
    pub budget: Budget,
}

impl RunContext {
    /// A run context over a space with default budgets.
    pub fn new(space: Space) -> Self {
        Self {
            space,
            budget: Budget::default(),
        }
    }

    /// Apply the script's `@options` on top of the host budget (the
    /// script's voice wins for these two fields, pi's rule).
    pub fn with_script_options(mut self, options: ScriptOptions) -> Self {
        if let Some(tokens) = options.max_output_tokens {
            self.budget.max_output_tokens = tokens;
        }
        if let Some(ms) = options.timeout_ms {
            self.budget.timeout_ms = Some(ms);
        }
        self
    }
}

/// A one-shot script engine.
///
/// Implementations must uphold the lifecycle clauses documented on this
/// module. `execute` never rejects for script failures — those come back
/// inside [`ScriptResult`] with `ok: false` (pi's contract: partial output
/// survives).
pub trait Engine: Send + Sync {
    /// Run one script to completion.
    fn execute(
        &self,
        code: &str,
        context: &RunContext,
    ) -> impl Future<Output = Result<ScriptResult, EngineError>> + Send;
}

/// The host-controlled execution mode (pi's `CodemodeRun`, CF's
/// `runtime.decide`). Instead of executing tool calls itself, the engine
/// yields each call to the host, which answers with [`Outcome::Deliver`]:
/// allow (with a result), deny (with an error), or pause.
///
/// This is the seam where approval and durable replay live: a host records
/// answered calls, pauses at an `requiresApproval` tool, and resumes a later
/// run by replaying recorded results through the same interface.
pub trait SuspendingEngine: Engine {
    /// Start a run. The script begins executing; every tool call surfaces
    /// through [`RunHandle::next_call`] instead of running.
    fn start(
        &self,
        code: &str,
        context: &RunContext,
    ) -> impl Future<Output = Result<RunHandle, EngineError>> + Send;
}

/// What the host decides for one yielded call.
#[non_exhaustive]
pub enum Decision {
    /// Run the tool now; the script receives its result.
    Allow,
    /// Reject the call; the script receives this error.
    Deny(String),
}

/// One tool call the script issued, awaiting the host's decision.
#[non_exhaustive]
pub struct HostCall {
    /// The engine's id for this call, unique within the run.
    pub id: u64,
    /// The canonical path the script called.
    pub name: String,
    /// The parsed argument object (or `null`).
    pub args: Value,
    /// The tool's dispatcher, for [`Decision::Allow`].
    pub execute: ToolFn,
}

/// The host's answer to a yielded call (pi's `CodemodeOutcome`).
#[non_exhaustive]
pub enum Outcome {
    /// The call resolved with this JSON value.
    Deliver(Value),
    /// The call rejected with this error text, carrying optional own
    /// properties the script sees on the error object.
    Fail {
        /// The error text the script's `Error` carries.
        message: String,
        /// Own properties attached to the error object.
        properties: Value,
    },
    /// The call was not answered; the record stays `cancelled` (pi:
    /// `deliver` is optional for calls that outlive the run).
    Cancel,
}

/// A running host-controlled execution. Concrete: engines hand these out
/// already driven by channels, so hosts need no generics.
#[non_exhaustive]
pub struct RunHandle {
    /// The next call the script issued, or `None` when the run ended.
    pub next_call: futures::channel::mpsc::Receiver<HostCall>,
    /// Answers flow back to the engine through this sender; a dropped
    /// receiver cancels the unanswered calls (pi: the record stays
    /// `cancelled`).
    pub deliveries: futures::channel::mpsc::Sender<(u64, Outcome)>,
    /// The run's result, once it ends.
    pub result: futures::channel::oneshot::Receiver<ScriptResult>,
}

/// A suspended run's serializable state (pi: a resumed session replays the
/// recorded calls; CF: the durable log). The engine records the answered
/// calls; persistence is the host's job — this is JSON-serializable exactly
/// so a session file can hold it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuspendState {
    /// The script source, unchanged.
    pub code: String,
    /// Calls answered before the pause, in order: id, name, and the outcome
    /// to replay. A resumed run replays these instead of executing.
    pub answered: Vec<AnsweredCall>,
}

/// One answered call, recorded for replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub struct AnsweredCall {
    /// The call's engine id within the original run.
    pub id: u64,
    /// The canonical path the script called.
    pub name: String,
    /// The recorded outcome.
    pub outcome: RecordedOutcome,
}

/// A serializable outcome (the replayable subset of [`Outcome`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecordedOutcome {
    /// The call resolved with this JSON value.
    Ok(Value),
    /// The call rejected with this message.
    Err(String),
}

/// Engine-side failures (the VM could not start, the bridge broke) —
/// distinct from script failures, which live in [`ScriptResult`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The engine failed outside the script's control.
    #[error("engine error: {0}")]
    Engine(String),
}
