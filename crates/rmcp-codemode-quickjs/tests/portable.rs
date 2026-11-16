//! Oracle suite: pi's `packages/codemode/test/portable.test.ts` — the
//! applicable subset ported mechanically; the rest recorded N/A with reasons
//! (SPEC §11.1).
//!
//! pi's file covers two sandboxes:
//!
//! - `InlineCodemodeSandbox` (2 cases, ported below): the interrupt budget —
//!   a sandbox-construction knob our `SandboxOptions.interrupt_budget`
//!   mirrors; the engine counts interrupt-handler polls and never resets the
//!   counter, so the budget accumulates across waits on tools (pi
//!   `runtime/inline.ts:76-94`).
//!
//! - `RemoteCodemodeSandbox` (5 cases, N/A): a Workers-RPC remote sandbox.
//!   v2 has no remote transport — the crate is a library and the wire/
//!   process boundary belongs to the embedder (SPEC D2, D14: the engine
//!   trait is local). Sub-behaviors the remote suite exercises are covered
//!   elsewhere: caller-side tool execution and call recording (sandbox
//!   suite), output batching (engine-side buffering, no exchange boundary),
//!   capped limits (sandbox suite's memory/timeout cases), caller aborts
//!   (sandbox suite's abort cases), and remote failure surfaces (no remote
//!   to fail).
//!
//! pi's `worker-lifetime.test.ts` (7 cases) is likewise N/A: our engine runs
//! each execution on a dedicated VM thread that is dropped when the run
//! settles — there is no persistent worker to dispose or reuse (SPEC D3).
//! Dispose/abort coverage lives in the sandbox suite (aborts unawaited calls
//! when the script returns, abort records cancelled calls, deadline expiry).

use std::sync::Arc;

use rmcp_codemode::{
    engine::Engine,
    types::{ErrorKind, ExecuteOptions, SandboxOptions, Tool},
};
use rmcp_codemode_quickjs::QuickJsEngine;
use serde_json::json;

fn budget_sandbox(tools: Vec<Tool>) -> QuickJsEngine {
    // pi: `new InlineCodemodeSandbox({ wasm, interruptBudget: 1_000 })` —
    // `timeoutMs: Number.POSITIVE_INFINITY` in the first case → no deadline.
    QuickJsEngine::new(SandboxOptions {
        tools,
        timeout_ms: None,
        interrupt_budget: Some(1_000),
        ..SandboxOptions::default()
    })
}

fn tick_tool() -> Tool {
    // pi: `{ name: "tick", execute: () => 1 }`
    Tool {
        name: "tick".into(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: false,
        signature: None,
        execute: Arc::new(|_args, _ctx| Box::pin(async { Ok(json!(1)) })),
    }
}

// pi portable.test.ts > InlineCodemodeSandbox > fails a script that computes
// past its interrupt budget as a timeout.
#[tokio::test(flavor = "current_thread")]
async fn fails_a_script_that_computes_past_its_interrupt_budget_as_a_timeout() {
    let sandbox = budget_sandbox(vec![]);
    for code in ["while (true) {}", "while (true) await null"] {
        let result = sandbox.execute(code, ExecuteOptions::default()).await;
        let error = result.outcome.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Timeout, "{code}: {}", error.message);
        assert_eq!(
            error.message, "Execution exceeded its interrupt budget of 1000",
            "{code}"
        );
    }
}

// pi portable.test.ts > InlineCodemodeSandbox > counts the budget across
// waits on tools.
#[tokio::test(flavor = "current_thread")]
async fn counts_the_budget_across_waits_on_tools() {
    let sandbox = budget_sandbox(vec![tick_tool()]);
    let result = sandbox
        .execute(
            "for (;;) { for (let i = 0; i < 1e5; i++); await tools.tick(); }",
            ExecuteOptions::default(),
        )
        .await;
    let error = result.outcome.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert!(result.calls.len() > 1, "calls: {:?}", result.calls);
}
