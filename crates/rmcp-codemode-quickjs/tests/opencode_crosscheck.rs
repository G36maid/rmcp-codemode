//! Cross-validation oracle: the applicable subset of OpenCode's codemode test
//! files (`packages/codemode/test/{codemode,errors,limits,tool-paths}.test.ts`,
//! cloned 2026-11-15 from `anomalyco/opencode`, post #62642 VM-only refactor).
//! The behaviors OpenCode contributed to our surface are contract-checked here;
//! everything else is recorded N/A with the reason (SPEC §11.3).
//!
//! Adopted-behavior decisions (2026-11-15):
//! - console stays **pi-style** (JSON formatting, never throws, NaN/Infinity
//!   literal at the top level) — OpenCode's DevTools-style total formatter,
//!   `console.table`, and `console.dir` are NOT adopted; the cases below
//!   assert our pi-style output and label where OC diverges.
//! - OC's dotted tool namespaces / `tools.search` alias / stale-signature
//!   hints are NOT adopted (D4: pi's flat `mcp__server__tool` naming + pi's
//!   guard proxy); those OC cases are N/A.
//! - OC's VM-internal caps (`MAX_STRING_LENGTH`/`MAX_ARRAY_LENGTH`/
//!   `MAX_PENDING_PROMISES`) are NOT adopted (D10: pi budgets); the
//!   deadline-stops-runaway cases port as cross-checks.
//! - OC's counted `searchTools` cases target the discovery surface, which is
//!   server-layer (§10) — deferred there with OC cross-references.

use std::sync::Arc;

use rmcp_codemode::{
    engine::Engine,
    types::{ExecuteOptions, OutputItem, SandboxOptions, Tool},
};
use rmcp_codemode_quickjs::QuickJsEngine;
use serde_json::{Value, json};

fn sandbox_with(tools: Vec<Tool>) -> QuickJsEngine {
    QuickJsEngine::new(SandboxOptions {
        tools,
        timeout_ms: Some(10_000),
        ..SandboxOptions::default()
    })
}

fn echo_tool(name: &str, marker: &str) -> Tool {
    let marker = marker.to_string();
    Tool {
        name: name.into(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: false,
        signature: None,
        execute: Arc::new(move |_args, _ctx| {
            let marker = marker.clone();
            Box::pin(async move { Ok(json!(marker)) })
        }),
    }
}

fn output_text(result: &rmcp_codemode::RunResult) -> Vec<&str> {
    result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Text {
                text,
                console: true,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---- OC codemode.test.ts > "CodeMode console capture" ----------------------
// Console stays pi-style: these cases pin our formatter and label the
// intentional OC divergences.

/// OC: "prints NaN and Infinity literally instead of the JSON null" — at the
/// TOP level pi's `format()` prints literals too (sandbox suite covers it).
/// **OC diverges** inside containers: OC renders `{"ratio":NaN}`; pi's JSON
/// stringify nulls non-finite numbers there. We keep pi.
#[tokio::test(flavor = "current_thread")]
async fn non_finite_numbers_inside_containers_render_as_json_null() {
    let sandbox = sandbox_with(vec![]);
    let result = sandbox
        .execute(
            "console.log(NaN); console.log(Infinity, -Infinity); console.log({ ratio: NaN, bounds: [Infinity] }); return null",
            ExecuteOptions::default(),
        )
        .await;
    assert!(result.outcome.is_ok());
    assert_eq!(
        output_text(&result),
        vec![
            "NaN",
            "Infinity -Infinity",
            r#"{"ratio":null,"bounds":[null]}"#,
        ]
    );
}

/// OC: "keeps logs captured before failures". Our engine keeps the whole
/// output on failure (pi sandbox: partial-output-on-failure); this pins the
/// console item specifically.
#[tokio::test(flavor = "current_thread")]
async fn console_output_kept_before_failure() {
    let sandbox = sandbox_with(vec![]);
    let result = sandbox
        .execute(
            r#"console.log("before failure"); throw new Error("boom")"#,
            ExecuteOptions::default(),
        )
        .await;
    assert!(result.outcome.is_err());
    assert_eq!(output_text(&result), vec!["before failure"]);
    assert_eq!(result.outcome.unwrap_err().message, "boom");
}

/// OC: "console formatting is total: cycles … render as markers" — OC renders
/// `[Circular]`. **OC diverges**: pi's `format()` catches the stringify throw
/// and falls back to `String(value)` (`[object Object]`), never throwing. We
/// keep pi.
#[tokio::test(flavor = "current_thread")]
async fn cyclic_object_never_throws_falls_back_to_string() {
    let sandbox = sandbox_with(vec![]);
    let result = sandbox
        .execute(
            "const box = { box: null }; box.box = box; console.log(box); return null",
            ExecuteOptions::default(),
        )
        .await;
    assert!(result.outcome.is_ok());
    assert_eq!(output_text(&result), vec!["[object Object]"]);
}

// ---- OC tool-paths.test.ts > "blocked member names on tool paths" ----------

/// OC: "tools may use blocked member names" — our tools object is a frozen
/// null-prototype object (pi `prelude-source.ts:215`), so a tool literally
/// named `prototype` is an ordinary own property, not the real prototype.
#[tokio::test(flavor = "current_thread")]
async fn tool_named_prototype_is_an_ordinary_own_property() {
    let sandbox = sandbox_with(vec![echo_tool("prototype", "proto")]);
    let result = sandbox
        .execute(
            "return [typeof tools.prototype, tools.prototype === Object.prototype, await tools.prototype({})]",
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!(["function", false, "proto"])
    );
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "prototype");
}

/// OC: "a literal `__proto__` key cannot poison a namespace into a fake tool".
/// The tools object is a null-prototype frozen object, so a declared
/// `__proto__` tool is an own property — no prototype switching, and the
/// guard's `in` check sees the own property.
#[tokio::test(flavor = "current_thread")]
async fn tool_named_dunder_proto_cannot_poison_the_tools_object() {
    let sandbox = sandbox_with(vec![
        echo_tool("__proto__", "dunder"),
        echo_tool("real", "real"),
    ]);
    let result = sandbox
        .execute(
            "return [await tools.__proto__({}), await tools.real({}), Object.keys(tools).sort()]",
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!(["dunder", "real", ["__proto__", "real"]])
    );
}

/// OC: "Prototype mutation is confined to one run: the next program starts
/// from fresh intrinsics." **OC diverges**: OC's VM allows in-run mutation and
/// confines it across runs; pi's lockdown makes built-in prototypes read-only
/// outright (stronger — sandbox suite `builtin_patches_are_ignored`), so the
/// assignment is silently ignored even in-run.
#[tokio::test(flavor = "current_thread")]
async fn prototype_pollution_is_confined_to_one_run() {
    let sandbox = sandbox_with(vec![]);
    let polluted = sandbox
        .execute(
            "Object.prototype.polluted = 1; Array.prototype.push = 2; return [({}).polluted, typeof [].push]",
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        polluted.outcome.as_ref().unwrap().value,
        json!([null, "function"])
    );

    let fresh = sandbox
        .execute(
            "return [({}).polluted, typeof [].push]",
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        fresh.outcome.as_ref().unwrap().value,
        json!([null, "function"])
    );
}

/// OC: "prototypes are program objects" — the standard intrinsic identities
/// hold inside the VM (the OC escape probes themselves are N/A: OC blocks the
/// `Function` constructor entirely, pi cages it but leaves it working —
/// sandbox suite `eval_and_function_stay_in_the_vm`).
#[tokio::test(flavor = "current_thread")]
async fn builtin_prototype_identities_hold_inside_the_vm() {
    let sandbox = sandbox_with(vec![]);
    let result = sandbox
        .execute(
            r#"return [
                ({}).constructor === Object, [].constructor === Array, "".constructor === String,
                Math.constructor === Object, new Object().constructor === Object,
                ({}).constructor.constructor === Function, (() => 1).constructor === Function,
            ]"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([true, true, true, true, true, true, true])
    );
}

// ---- OC errors.test.ts > "error identity" ----------------------------------
// Pure-JS rejection identity: standard semantics our VM must preserve.

/// OC: "awaiting the same rejected promise twice yields the same error object".
#[tokio::test(flavor = "current_thread")]
async fn awaiting_the_same_rejected_promise_twice_yields_the_same_reason() {
    let sandbox = sandbox_with(vec![]);
    let result = sandbox
        .execute(
            r#"const marker = { code: 42 };
const dead = Promise.reject(marker);
const first = await dead.catch((error) => error);
const second = await dead.catch((error) => error);
return [first === second, first === marker, first.code];"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([true, true, 42])
    );
}

/// OC: "allSettled reasons for the same failure are identical" + "Promise.any
/// collects the same object a direct catch would" (standard `Promise.any`:
/// the rejection is an `AggregateError` wrapping the same reason).
#[tokio::test(flavor = "current_thread")]
async fn all_settled_and_any_collect_the_same_rejection_reason() {
    let sandbox = sandbox_with(vec![]);
    let result = sandbox
        .execute(
            r#"const marker = { code: 7 };
const dead = Promise.reject(marker);
const settled = await Promise.allSettled([dead]);
const any = await Promise.any([dead]).catch((error) => error);
return [settled[0].reason === marker, any.errors[0] === marker, settled[0].status, any.name];"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([true, true, "rejected", "AggregateError"])
    );
}

// ---- OC limits.test.ts > "the host stays in control" -----------------------
// OC's VM-internal caps are N/A (D10: pi budgets); the deadline-stops-runaway
// cases port as cross-checks (OC asserts TimeoutExceeded at 100 ms; ours uses
// the pi-style deadline → `timeout`).

/// OC: "serializing shared references still times out" — a 2^20-leaf shared
/// DAG must not outrun the host deadline (OC: 100 ms + TimeoutExceeded; ours:
/// 300 ms deadline → `timeout`). Note: quickjs's native `JSON.stringify`
/// recursion doesn't poll the interrupt handler — the interruption lands when
/// the interpreter resumes, and the outcome is still the deadline.
#[tokio::test(flavor = "current_thread")]
async fn serializing_shared_references_still_times_out() {
    let sandbox = QuickJsEngine::new(SandboxOptions {
        timeout_ms: Some(300),
        ..SandboxOptions::default()
    });
    let result = sandbox
        .execute(
            "let a = [0]; for (let i = 0; i < 20; i++) a = [a, a]; return JSON.stringify(a).length",
            ExecuteOptions::default(),
        )
        .await;
    let error = result.outcome.unwrap_err();
    assert_eq!(error.kind, rmcp_codemode::types::ErrorKind::Timeout);
}

// ---- N/A ledger (SPEC §11.3) -----------------------------------------------
// - tool-paths: dotted tool names, callable namespaces, namespace metadata,
//   `tools.search` alias, search scoping — OC's dotted-namespace architecture;
//   we adopt pi's flat `mcp__server__tool` naming (D4). N/A by design.
// - tool-paths: "tool input diagnostics" (stale-signature hints) — OC host
//   call() feature; our contract surfaces tool errors as plain Errors. N/A.
// - tool-paths: "The Function constructor is not supported" escape probe — OC
//   blocks Function entirely; pi cages it but leaves it working (sandbox
//   suite). N/A: pi surface.
// - limits: MAX_STRING_LENGTH / MAX_ARRAY_LENGTH / MAX_PENDING_PROMISES caps —
//   OC bytecode-VM internals; D10 keeps pi's budgets (memory + stack +
//   deadline). N/A by design.
// - codemode.test.ts: counted `searchTools` cases — the discovery surface is
//   server-layer (§10, OQ6); deferred there with OC as the cross-reference.
#[allow(dead_code)]
fn na_ledger_keeps_imports_used(_value: Value) {}
