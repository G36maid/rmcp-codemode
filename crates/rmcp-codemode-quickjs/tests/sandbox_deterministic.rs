//! Ported from pi's `packages/codemode/test/deterministic.test.ts` — the
//! determinism oracle (SPEC §11.1). Same scripts, same fixtures, same
//! assertions adapted to the Rust contract. Each test is labeled with its pi
//! source case.
//!
//! Port adaptations (documented): the random VALUES differ from pi's pins
//! (quickjs-wasi's Math.random pipeline is internal to that build); our
//! deterministic Math.random is mulberry32 seeded with `seed ^ 0x9e3779b9`
//! (pi's own WASI prng algorithm, line-for-line), so sequence-consistency
//! oracles (same seed → same values; different seed → different) are
//! preserved exactly.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp_codemode::{
    engine::{Engine, RunHandle},
    types::{
        DeclaredGlobal, DeclaredTool, DeliverError, ErrorKind, ExecuteOptions, HostCall, Outcome,
        OutputItem, RunOptions, SandboxOptions,
    },
};
use rmcp_codemode_quickjs::QuickJsEngine;
use serde_json::{Value, json};

const STARTED_AT: u64 = 1_768_471_200_000; // Date.UTC(2026, 0, 15, 10, 0, 0)

fn engine() -> QuickJsEngine {
    QuickJsEngine::new(SandboxOptions::default())
}

type Recorded = (Box<dyn RunHandle>, Arc<Mutex<Vec<HostCall>>>);

fn declared_tools(names: &[&str]) -> Vec<DeclaredTool> {
    names
        .iter()
        .map(|name| DeclaredTool {
            name: (*name).to_owned(),
            js_name: None,
            description: None,
        })
        .collect()
}

/// Start `code` deterministically with declared tools `echo` and `wait`;
/// calls are recorded but NOT answered — the test drives deliveries (pi's
/// `start()` answers non-wait calls via queueMicrotask; the Rust harness
/// returns the handle and the test drives deliveries, equivalent for these
/// single-threaded scripts).
fn start(sandbox: &QuickJsEngine, code: &str) -> Recorded {
    start_with(sandbox, code, declared_tools(&["echo", "wait"]), Vec::new())
}

fn start_with(
    sandbox: &QuickJsEngine,
    code: &str,
    tools: Vec<DeclaredTool>,
    globals: Vec<DeclaredGlobal>,
) -> Recorded {
    start_signal(sandbox, code, tools, globals, None)
}

/// `start` with explicit clock/seed overrides (pi's `start(sandbox, code, { startedAt, seed })`).
fn start_opts(sandbox: &QuickJsEngine, code: &str, started_at: u64, seed: u32) -> Recorded {
    let calls: Arc<Mutex<Vec<HostCall>>> = Arc::new(Mutex::new(Vec::new()));
    let calls2 = Arc::clone(&calls);
    let run = sandbox.start(
        code,
        RunOptions {
            started_at,
            seed,
            tools: declared_tools(&["echo", "wait"]),
            globals: Vec::new(),
            store: BTreeMap::new(),
            signal: None,
            timeout_ms: Some(20_000),
            on_started: None,
            on_idle: None,
            on_call: Box::new(move |call: HostCall| {
                calls2.lock().unwrap().push(call);
            }),
        },
    );
    (run, calls)
}

fn start_signal(
    sandbox: &QuickJsEngine,
    code: &str,
    tools: Vec<DeclaredTool>,
    globals: Vec<DeclaredGlobal>,
    signal: Option<tokio_util::sync::CancellationToken>,
) -> Recorded {
    let calls: Arc<Mutex<Vec<HostCall>>> = Arc::new(Mutex::new(Vec::new()));
    let calls2 = Arc::clone(&calls);
    let run = sandbox.start(
        code,
        RunOptions {
            started_at: STARTED_AT,
            seed: 1,
            tools,
            globals,
            store: BTreeMap::new(),
            signal,
            timeout_ms: Some(20_000),
            on_started: None,
            on_idle: None,
            on_call: Box::new(move |call: HostCall| {
                calls2.lock().unwrap().push(call);
            }),
        },
    );
    (run, calls)
}

fn wait_for_calls(calls: &Mutex<Vec<HostCall>>, count: usize) {
    for _ in 0..200 {
        if calls.lock().unwrap().len() >= count {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Auto-answer every recorded non-`wait` call: result seq 1, 2, … with the
/// clock at STARTED_AT + 1000 × seq, value = args (pi's default answer).
#[allow(clippy::borrowed_box)] // Recorded's handle type is Box<dyn RunHandle>
fn auto_answer(
    run: &Box<dyn RunHandle>,
    calls: &Mutex<Vec<HostCall>>,
    answered: &mut std::collections::HashSet<u64>,
) {
    for call in calls.lock().unwrap().iter() {
        if call.name == "wait" {
            continue;
        }
        if answered.insert(call.id) {
            let seq = call.id; // calls issue in order: id == seq
            let _ = run.deliver(
                seq,
                STARTED_AT + 1000 * seq,
                call.id,
                Outcome::Ok(call.args.clone()),
            );
        }
    }
}

// pi: deterministic.test.ts > makes the same calls and output for the same
// start, seed, and results. Random values: our deterministic Math.random is
// mulberry32 (pi's WASI prng algorithm) — the pinned quickjs-wasi numbers are
// engine artifacts; sequence consistency is preserved exactly.
#[tokio::test(flavor = "current_thread")]
async fn same_calls_and_output_for_same_inputs() {
    let code = r#"
        const values = [Date.now(), performance.now(), Math.random(), Math.random(), new Date().toISOString()];
        const first = await tools.echo(values);
        text(JSON.stringify([Date.now(), performance.now(), Math.random()]));
        return await tools.echo(first.concat(Math.random()));
    "#;
    let statuses = |r: &rmcp_codemode::types::RunResult| {
        r.calls
            .iter()
            .map(|c| (c.name.clone(), c.status))
            .collect::<Vec<_>>()
    };

    let sandbox = engine();
    let (mut one, one_calls) = start(&sandbox, code);
    let (mut two, two_calls) = start(&sandbox, code);
    for _ in 0..100 {
        auto_answer(&one, &one_calls, &mut std::collections::HashSet::new());
        auto_answer(&two, &two_calls, &mut std::collections::HashSet::new());
        if one.finished() && two.finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    auto_answer(&one, &one_calls, &mut std::collections::HashSet::new());
    auto_answer(&two, &two_calls, &mut std::collections::HashSet::new());
    let a = one.result().await;
    let b = two.result().await;

    // Call durations are wall-clock time; everything else is the same.
    // Outcome comparison: value equality (ScriptError lacks PartialEq).
    match (&a.outcome, &b.outcome) {
        (Ok(a_ok), Ok(b_ok)) => {
            assert_eq!(a_ok.value, b_ok.value);
            assert_eq!(a_ok.store_writes, b_ok.store_writes);
        }
        (Err(a_err), Err(b_err)) => {
            assert_eq!(a_err.kind, b_err.kind);
            assert_eq!(a_err.message, b_err.message);
        }
        _ => panic!("one run failed: {:?} vs {:?}", a.outcome, b.outcome),
    }
    assert_eq!(a.output, b.output);
    assert_eq!(statuses(&a), statuses(&b));
    assert_eq!(
        one_calls.lock().unwrap().clone(),
        two_calls.lock().unwrap().clone()
    );

    // The clock: startedAt while processing result 0; performance.now counts
    // from startedAt; UTC-forced date formatting.
    let first_args = one_calls.lock().unwrap()[0].args.clone();
    assert_eq!(first_args[0], json!(STARTED_AT));
    assert_eq!(first_args[1], json!(0));
    assert_eq!(first_args[4], json!("2026-01-15T10:00:00.000Z"));

    // The printed values: clock advanced +1000 for result 1.
    let OutputItem::Text { text, .. } = &a.output[0] else {
        panic!("expected a text item");
    };
    let printed = serde_json::from_str::<Value>(text).unwrap();
    assert_eq!(printed[0], json!(STARTED_AT + 1000));
    assert_eq!(printed[1], json!(1000));

    // The seed decides the random values, not the start time: same seed with
    // a later startedAt reproduces the draw; a different seed does not (pi:
    // both later and reseeded runs; the pinned VALUES are quickjs-wasi
    // artifacts — see the header note).
    let random = first_args[2].clone();
    let sandbox2 = engine();
    let (later, later_calls) = start_opts(&sandbox2, code, STARTED_AT + 5000, 1);
    let (reseeded, reseeded_calls) = start_opts(&sandbox2, code, STARTED_AT, 2);
    let mut later_answered = std::collections::HashSet::new();
    let mut reseeded_answered = std::collections::HashSet::new();
    for _ in 0..100 {
        auto_answer(&later, &later_calls, &mut later_answered);
        auto_answer(&reseeded, &reseeded_calls, &mut reseeded_answered);
        if later.finished() && reseeded.finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        later_calls.lock().unwrap()[0].args[2].clone(),
        random,
        "same seed → same values regardless of startedAt"
    );
    assert_ne!(
        reseeded_calls.lock().unwrap()[0].args[2].clone(),
        random,
        "different seed → different values"
    );
}

// pi: deterministic.test.ts > runs in UTC whatever the host's timezone, and
// without WeakRef or FinalizationRegistry. Execute() is unchanged: the host's
// timezone and every intrinsic.
#[tokio::test(flavor = "current_thread")]
async fn utc_and_no_weak_ref_in_deterministic_mode() {
    let code = "return [new Date(0).getHours(), new Date(0).getTimezoneOffset(), typeof WeakRef, typeof FinalizationRegistry]";
    let sandbox = engine();
    let (mut run, _) = start(&sandbox, code);
    let result = run.result().await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([0, 0, "undefined", "undefined"])
    );
    // Execute() is unchanged: host timezone + every intrinsic.
    let result = sandbox.execute(code, ExecuteOptions::default()).await;
    let arr = result
        .outcome
        .as_ref()
        .unwrap()
        .value
        .as_array()
        .unwrap()
        .clone();
    assert_ne!(arr[0], json!(0), "execute() keeps the host timezone");
    assert_eq!(arr[2], json!("function"));
    assert_eq!(arr[3], json!("function"));
}

// pi: deterministic.test.ts > applies results in seq order, whatever order
// they are delivered in.
#[tokio::test(flavor = "current_thread")]
async fn applies_results_in_seq_order() {
    let sandbox = engine();
    let (mut run, calls) = start_with(
        &sandbox,
        r#"const order = [];
        await Promise.all([tools.wait("a").then(() => order.push("a")), tools.wait("b").then(() => order.push("b"))]);
        return order;"#,
        declared_tools(&["wait"]),
        Vec::new(),
    );
    wait_for_calls(&calls, 2);
    let ids: Vec<u64> = calls.lock().unwrap().iter().map(|call| call.id).collect();
    // b is result 1 and a result 2, but a arrives first.
    run.deliver(2, STARTED_AT + 2000, ids[0], Outcome::Ok(Value::Null))
        .expect("a delivers as result 2");
    run.deliver(1, STARTED_AT + 1000, ids[1], Outcome::Ok(Value::Null))
        .expect("b delivers as result 1");
    let result = run.result().await;
    // Applied in seq order: b (result 1) before a (result 2).
    assert_eq!(result.outcome.as_ref().unwrap().value, json!(["b", "a"]));
}

// pi: deterministic.test.ts > keeps the clock monotonic and rejects a clock
// value that is not finite. Port: `now: u64` makes non-finite
// unrepresentable; a clock behind the run's start stays monotonic.
#[tokio::test(flavor = "current_thread")]
async fn clock_is_monotonic() {
    let sandbox = engine();
    let (mut run, calls) = start(
        &sandbox,
        "const before = Date.now(); await tools.wait(1); return [before, Date.now(), performance.now()]",
    );
    wait_for_calls(&calls, 1);
    let id = calls.lock().unwrap()[0].id;
    // A clock behind the run's start stays monotonic.
    run.deliver(
        1,
        STARTED_AT.saturating_sub(2000),
        id,
        Outcome::Ok(Value::Null),
    )
    .expect("monotonic clock keeps the earlier value");
    let result = run.result().await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([STARTED_AT, STARTED_AT, 0])
    );
}

// pi: deterministic.test.ts > reports the result the VM was processing when
// the script ended (`during`), and start runs always report it. Execute mode
// has no `during`.
#[tokio::test(flavor = "current_thread")]
async fn during_reports_where_the_script_ended() {
    let sandbox = engine();
    let (mut returned, calls) = start(&sandbox, "await tools.echo(1); return await tools.echo(2)");
    wait_for_calls(&calls, 1);
    let mut answered = std::collections::HashSet::new();
    for _ in 0..100 {
        auto_answer(&returned, &calls, &mut answered);
        if returned.finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let result = returned.result().await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!(2));
    assert_eq!(
        result.during,
        Some(rmcp_codemode::types::During { seq: 2, issued: 2 })
    );

    let sandbox = engine();
    let (mut first, _) = start(&sandbox, "return 1");
    let result = first.result().await;
    assert_eq!(
        result.during,
        Some(rmcp_codemode::types::During { seq: 0, issued: 0 })
    );
    let result = sandbox.execute("return 1", ExecuteOptions::default()).await;
    assert_eq!(result.during, None);
}

// pi: deterministic.test.ts > rejects with an Error carrying the answer's
// properties (name/message/stack ignored; own properties carried).
#[tokio::test(flavor = "current_thread")]
async fn rejection_carries_answer_properties() {
    let sandbox = engine();
    let props: BTreeMap<String, Value> = BTreeMap::from([
        ("structuredOutput".to_owned(), json!({"code": 7})),
        ("name".to_owned(), json!("Ignored")),
        ("message".to_owned(), json!("ignored")),
    ]);
    let (mut run, calls) = start_with(
        &sandbox,
        "try { await tools.echo(1); } catch (e) { return [e instanceof Error, e.message, e.name, e.structuredOutput]; }",
        declared_tools(&["echo"]),
        Vec::new(),
    );
    // Deliver the error outcome with carried properties when the call is
    // issued (pi: answer() returns ok:false with properties).
    wait_for_calls(&calls, 1);
    let id = calls.lock().unwrap()[0].id;
    run.deliver(
        1,
        STARTED_AT,
        id,
        Outcome::Err {
            message: "denied".into(),
            properties: Some(props),
        },
    )
    .expect("delivers the denial");
    let result = run.result().await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([true, "denied", "Error", {"code": 7}])
    );
}

// pi: deterministic.test.ts > ends with an error the script cannot catch when
// the caller aborts. Port: the abort kind matches (the reason string is
// fixed — CancellationToken carries no reason); output is discarded.
#[tokio::test(flavor = "current_thread")]
async fn abort_is_uncatchable() {
    use tokio_util::sync::CancellationToken;
    let sandbox = engine();
    let token = CancellationToken::new();
    let (mut run, calls) = start_signal(
        &sandbox,
        r#"try { await tools.wait(1); } catch { text("caught"); }
        text("after");"#,
        declared_tools(&["wait"]),
        Vec::new(),
        Some(token.clone()),
    );
    wait_for_calls(&calls, 1);
    token.cancel();
    let result = run.result().await;
    // Uncatchable: the script's try/catch never saw it (output empty).
    assert!(result.output.is_empty());
    let error = result.outcome.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Aborted);
}

// pi: deterministic.test.ts > is finished once it has its outcome.
#[tokio::test(flavor = "current_thread")]
async fn finished_once_it_has_its_outcome() {
    let sandbox = engine();
    let (mut run, calls) = start(&sandbox, "await tools.wait(1); return 2");
    wait_for_calls(&calls, 1);
    assert!(!run.finished());
    let id = calls.lock().unwrap()[0].id;
    run.deliver(1, STARTED_AT, id, Outcome::Ok(Value::Null))
        .expect("delivers");
    let _ = run.result().await;
    assert!(run.finished());
}

// pi: deterministic.test.ts > rejects a call that is not waiting and a result
// number used twice.
#[tokio::test(flavor = "current_thread")]
async fn delivery_errors() {
    let sandbox = engine();
    let (mut run, calls) = start(
        &sandbox,
        "await Promise.all([tools.wait(1), tools.wait(2)])",
    );
    wait_for_calls(&calls, 2);
    let ids: Vec<u64> = calls.lock().unwrap().iter().map(|call| call.id).collect();
    // Unknown call id.
    assert_eq!(
        run.deliver(1, STARTED_AT, 99, Outcome::Ok(Value::Null)),
        Err(DeliverError::NotWaiting)
    );
    run.deliver(1, STARTED_AT, ids[0], Outcome::Ok(Value::Null))
        .expect("first delivery");
    // Seq 1 used again; seq 0 never valid (pi: "used once").
    assert_eq!(
        run.deliver(1, STARTED_AT, ids[1], Outcome::Ok(Value::Null)),
        Err(DeliverError::SeqUsed)
    );
    assert_eq!(
        run.deliver(0, STARTED_AT, ids[1], Outcome::Ok(Value::Null)),
        Err(DeliverError::SeqUsed)
    );
    run.deliver(2, STARTED_AT, ids[1], Outcome::Ok(Value::Null))
        .expect("second delivery");
    let result = run.result().await;
    assert!(result.outcome.is_ok());
}

// pi: deterministic.test.ts > uses the script names the run passes (jsName).
#[tokio::test(flavor = "current_thread")]
async fn uses_declared_script_names() {
    let sandbox = engine();
    let declared = DeclaredTool {
        name: "my-tool".into(),
        js_name: Some("renamed".into()),
        description: None,
    };
    let (mut run, _) = start_with(
        &sandbox,
        "return [typeof tools.renamed, Object.keys(tools), ALL_TOOLS.map((tool) => tool.name)]",
        vec![declared],
        Vec::new(),
    );
    let result = run.result().await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!(["function", ["renamed", "my-tool"], ["renamed"]])
    );
}
