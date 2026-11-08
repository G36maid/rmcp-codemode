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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    budget::{Budget, ScriptOptions},
    result::ScriptResult,
    space::{Space, ToolFn},
};

/// Everything one run needs: the tool space it may call, the resource
/// budgets, and the namespace metadata for discovery.
/// Deterministic-run parameters (pi's `deterministic: { startedAt, seed }`):
/// the same script, start time, seed, tools, and delivered results produce
/// the same observable behavior. The VM's clock starts at
/// [`Determinism::started_at_ms`] and advances only with delivered results,
/// never backwards; `Math.random` draws from a PRNG over [`Determinism::seed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Determinism {
    /// The virtual clock's start, in ms since the epoch.
    pub started_at_ms: u64,
    /// The PRNG seed for `Math.random` (pi: a 32-bit unsigned integer).
    pub seed: u32,
}

impl Determinism {
    /// A determinism config.
    pub fn new(started_at_ms: u64, seed: u32) -> Self {
        Self {
            started_at_ms,
            seed,
        }
    }
}

/// Everything a script run needs: its tool space, budgets, store, and
/// determinism settings.
#[derive(Clone)]
#[non_exhaustive]
pub struct RunContext {
    /// The tools the script may call and the namespaces they belong to.
    pub space: Space,
    /// Resource budgets. The script's `@options` override
    /// [`Budget::max_output_tokens`] and [`Budget::timeout_ms`].
    pub budget: Budget,
    /// The committed script store (`load` reads it; `store` writes commit
    /// here on success). Empty unless the host persists state.
    pub store: BTreeMap<String, Value>,
    /// Which store domain this run reads and writes (pi: one store per
    /// session branch — `codemode-store` entries applied along the branch).
    /// The host keys its persisted stores by domain; `None` is the default
    /// single-domain behavior.
    pub store_domain: Option<String>,
    /// Deterministic-run parameters. `None` (the default) means the
    /// script sees the host's real clocks.
    pub deterministic: Option<Determinism>,
}

impl RunContext {
    /// A run context over a space with default budgets.
    pub fn new(space: Space) -> Self {
        Self {
            space,
            budget: Budget::default(),
            store: BTreeMap::new(),
            store_domain: None,
            deterministic: None,
        }
    }

    /// Make the run deterministic (pi's replay mode).
    pub fn with_determinism(mut self, determinism: Determinism) -> Self {
        self.deterministic = Some(determinism);
        self
    }

    /// Set the committed store (`load`'s initial view).
    pub fn with_store(mut self, store: BTreeMap<String, Value>) -> Self {
        self.store = store;
        self
    }

    /// Run in the named store domain (`load`/`store` see that domain's
    /// values; pi's per-branch stores).
    pub fn with_store_domain(mut self, domain: impl Into<String>) -> Self {
        self.store_domain = Some(domain.into());
        self
    }

    /// Override the host budget.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
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

impl HostCall {
    /// A yielded call. Fields are public; this constructor keeps the
    /// `#[non_exhaustive]` struct buildable outside the crate.
    pub fn new(id: u64, name: String, args: Value, execute: ToolFn) -> Self {
        Self {
            id,
            name,
            args,
            execute,
        }
    }
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

/// One delivered answer: the call id, its outcome, and optionally the
/// clock value the deterministic VM reads while applying it (pi's
/// `deliver(seq, now, id, outcome)`).
#[non_exhaustive]
pub struct Delivery {
    /// The call's engine id.
    pub id: u64,
    /// The outcome to apply.
    pub outcome: Outcome,
    /// The clock value for the deterministic VM, ms since the epoch.
    /// Ignored in non-deterministic runs.
    pub now_ms: Option<u64>,
}

impl Delivery {
    /// Deliver an answer without a clock value.
    pub fn new(id: u64, outcome: Outcome) -> Self {
        Self {
            id,
            outcome,
            now_ms: None,
        }
    }

    /// Deliver an answer with the clock value the VM reads.
    pub fn with_now(id: u64, outcome: Outcome, now_ms: u64) -> Self {
        Self {
            id,
            outcome,
            now_ms: Some(now_ms),
        }
    }
}

impl RunHandle {
    /// Assemble a run handle from its channel halves.
    pub fn new(
        next_call: futures::channel::mpsc::UnboundedReceiver<HostCall>,
        deliveries: futures::channel::mpsc::UnboundedSender<Delivery>,
        result: futures::channel::oneshot::Receiver<ScriptResult>,
    ) -> Self {
        Self {
            next_call,
            deliveries,
            result,
        }
    }
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
    pub next_call: futures::channel::mpsc::UnboundedReceiver<HostCall>,
    /// Answers flow back to the engine; a dropped sender cancels the
    /// unanswered calls (pi: the record stays `cancelled`).
    pub deliveries: futures::channel::mpsc::UnboundedSender<Delivery>,
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
    /// The run's deterministic parameters, so a resumed run observes the
    /// same clocks and randomness as the original (pi: the host saves
    /// startedAt and seed with the session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deterministic: Option<Determinism>,
}

impl SuspendState {
    /// A state for `code` with the given answered calls.
    pub fn new(code: impl Into<String>, answered: Vec<AnsweredCall>) -> Self {
        Self {
            code: code.into(),
            answered,
            deterministic: None,
        }
    }

    /// Record the run's deterministic parameters with the state.
    pub fn with_determinism(mut self, determinism: Determinism) -> Self {
        self.deterministic = Some(determinism);
        self
    }
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
    /// The clock value the deterministic VM read while applying this
    /// outcome; `None` in non-deterministic runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ms: Option<u64>,
}

impl AnsweredCall {
    /// A recorded call.
    pub fn new(id: u64, name: impl Into<String>, outcome: RecordedOutcome) -> Self {
        Self {
            id,
            name: name.into(),
            outcome,
            now_ms: None,
        }
    }

    /// Record the clock value the deterministic VM read while applying
    /// this outcome.
    pub fn with_now(mut self, now_ms: u64) -> Self {
        self.now_ms = Some(now_ms);
        self
    }
}

impl RecordedOutcome {
    /// A recorded success.
    pub fn ok(value: Value) -> Self {
        Self::Ok(value)
    }
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
