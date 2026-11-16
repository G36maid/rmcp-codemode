//! The engine contract: two execution modes, pi-exact. (pi:
//! `CodemodeSandbox` / `CodemodeRun` — SPEC.md §5.0.)
//!
//! Mode 1 [`Engine::execute`]: run-to-completion; the host supplies tool
//! closures and receives the full [`RunResult`].
//!
//! Mode 2 [`Engine::start`]: host-controlled; the engine receives only
//! declarations, reports calls via `RunOptions::on_call`, and the host answers
//! through [`RunHandle::deliver`]. Deterministic replay (SPEC §5.0b) is
//! host-layer: a resumed run is a fresh `start()` fed the recorded calls,
//! outcomes, `started_at`, `seed`, and fingerprint.

use futures::future::BoxFuture;

use crate::types::{DeliverError, ExecuteOptions, Outcome, RunOptions, RunResult};

/// A host-controlled run. (pi: `CodemodeRun`.)
pub trait RunHandle: Send {
    /// The run's final result. Set once [`Self::finished`] is true.
    fn result(&mut self) -> BoxFuture<'_, RunResult>;

    /// Whether the run has its outcome: the script ended, timed out, failed,
    /// or was aborted. Decided before `result` settles; calls can no longer be
    /// answered. Does not imply the VM has exited.
    fn finished(&self) -> bool;

    /// Answer call `id` as result number `seq`. Results are numbered 1, 2, …
    /// without gaps and applied strictly in seq order, whatever order they
    /// arrive in: after a gap the VM waits for the missing number. While it
    /// processes this one its clock reads `now` (ms since the epoch), or the
    /// previous value if `now` is earlier. (pi: `CodemodeRun.deliver`, which
    /// throws for these error cases.)
    ///
    /// Errors: [`DeliverError::NotWaiting`] for a call that is not waiting for
    /// an answer, [`DeliverError::SeqUsed`] for a reused `seq`.
    /// `now: u64` makes pi's non-finite check unrepresentable.
    fn deliver(&self, seq: u64, now: u64, id: u64, outcome: Outcome) -> Result<(), DeliverError>;
}

/// A codemode engine: runs untrusted scripts against declared tools. The VM
/// lifecycle is pi's (SPEC §5.1): a fresh VM per run, disposed on settle.
pub trait Engine: Send + Sync {
    /// Run to completion. The host-supplied tool closures (sandbox options)
    /// answer nested calls; deterministic configuration comes from
    /// `options`/sandbox defaults as specified in §5.4.
    fn execute(&self, script: &str, options: ExecuteOptions) -> BoxFuture<'static, RunResult>;

    /// Start a host-controlled run: report calls, receive answers.
    fn start(&self, script: &str, options: RunOptions) -> Box<dyn RunHandle>;
}
