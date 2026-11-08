//! Engine contract tests: the lifecycle clauses from
//! `rmcp_codemode::engine` verified against the QuickJS backend.

use std::sync::Arc;

use futures::StreamExt;
use rmcp_codemode::{
    Engine, HostCall, Outcome, RunContext, Space, SuspendState, SuspendingEngine, ToolHandle,
    Value, flat_identifier,
};
use rmcp_codemode_quickjs::{QuickJsEngine, SuspendingQuickJsEngine};
use serde_json::json;

fn echo_tool(path: &str, result: Value) -> ToolHandle {
    ToolHandle::new(
        path.split('.').map(str::to_owned).collect(),
        format!("Echo tool at {path}"),
        Arc::new(move |_| {
            let result = result.clone();
            Box::pin(async move { Ok(result.clone()) }) as rmcp_codemode::ToolFnFut
        }),
    )
}

fn failing_tool(path: &str, message: &str) -> ToolHandle {
    let message = message.to_owned();
    ToolHandle::new(
        path.split('.').map(str::to_owned).collect(),
        "Always fails",
        Arc::new(move |_| {
            let message = message.clone();
            Box::pin(async move { Err(message) }) as rmcp_codemode::ToolFnFut
        }),
    )
}

fn space_with(tools: Vec<ToolHandle>) -> Space {
    let mut space = Space::new();
    for tool in tools {
        space.insert(tool).unwrap();
    }
    space
}

#[tokio::test(flavor = "current_thread")]
async fn script_calls_tools_and_returns_values() {
    let engine = QuickJsEngine::default();
    let space = space_with(vec![echo_tool("github.create_issue", json!({ "id": 7 }))]);
    let result = engine
        .execute(
            "return await tools.github.create_issue({ title: \"x\" });",
            &RunContext::new(space),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(result.value, Some(json!({ "id": 7 })));
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "github.create_issue");
}

#[tokio::test(flavor = "current_thread")]
async fn flat_identifier_projection_reaches_the_same_tool() {
    // pi's spelling: a flat identifier reaches a nested tool.
    let engine = QuickJsEngine::default();
    let space = space_with(vec![echo_tool("my-tool", json!("dashed"))]);
    let result = engine
        .execute("return await tools.my_tool();", &RunContext::new(space))
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(result.value, Some(json!("dashed")));
}

#[tokio::test(flavor = "current_thread")]
async fn tool_failure_rejects_inside_the_script() {
    let engine = QuickJsEngine::default();
    let space = space_with(vec![failing_tool("flaky.call", "upstream down")]);
    let result = engine
        .execute(
            r#"
            try { await tools.flaky.call({}); return "no error"; }
            catch (e) { return e.message; }
            "#,
            &RunContext::new(space),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.value, Some(json!("upstream down")));
}

#[tokio::test(flavor = "current_thread")]
async fn partial_output_survives_failure() {
    let engine = QuickJsEngine::default();
    let space = space_with(vec![failing_tool("boom", "exploded")]);
    let result = engine
        .execute(
            "text(\"before\");\nawait tools.boom({});",
            &RunContext::new(space),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    assert_eq!(
        result.output.first(),
        Some(&rmcp_codemode::OutputItem::Text {
            text: "before".to_owned()
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn never_settling_promise_fails_immediately() {
    // The contract: no timers exist, so a promise nothing can settle fails
    // the run right away — no deadline needed.
    let engine = QuickJsEngine::default();
    let started = std::time::Instant::now();
    let result = engine
        .execute(
            "await new Promise(() => {}); return 'never';",
            &RunContext::new(Space::new()),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|m| m.contains("can never settle")),
        "{:?}",
        result.error
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_preempts_a_pure_js_loop() {
    let engine = QuickJsEngine::default();
    let context = RunContext::new(Space::new());
    let mut context = context;
    context.budget.timeout_ms = Some(200);
    let result = engine.execute("while (true) {}", &context).await.unwrap();
    assert!(!result.ok);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|m| m.contains("deadline")),
        "{:?}",
        result.error
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unawaited_calls_are_cancelled_not_waited_out() {
    // pi: "calls still running when the script ends are cancelled." A slow
    // tool the script never awaits must not delay the result.
    let slow = ToolHandle::new(
        vec!["slow".to_owned()],
        "Resolves after 500ms",
        Arc::new(|_| {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                Ok(json!("late"))
            })
        }),
    );
    let engine = QuickJsEngine::default();
    let started = std::time::Instant::now();
    let result = engine
        .execute(
            "tools.slow(); return \"early\";",
            &RunContext::new(space_with(vec![slow])),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.value, Some(json!("early")));
    assert!(started.elapsed() < std::time::Duration::from_millis(300));
}

#[tokio::test(flavor = "current_thread")]
async fn suspending_run_yields_calls_to_the_host() {
    // CF's runtime.decide, pi's start()/deliver(): the host sees every call
    // and decides.
    let suspending = SuspendingQuickJsEngine::from_engine(QuickJsEngine::default());
    let space = space_with(vec![echo_tool("github.create_issue", json!({ "id": 42 }))]);
    let mut handle = suspending
        .start(
            "return await tools.github.create_issue({ title: \"t\" });",
            &RunContext::new(space),
        )
        .await
        .unwrap();

    let call: HostCall = handle.next_call.next().await.expect("a yielded call");
    assert_eq!(call.name, "github.create_issue");
    assert_eq!(call.args, json!({ "title": "t" }));

    // Deny: the script sees the error.
    handle
        .deliveries
        .unbounded_send(rmcp_codemode::Delivery::new(
            call.id,
            Outcome::Fail {
                message: "denied by user".to_owned(),
                properties: Value::Null,
            },
        ))
        .unwrap();
    let result = handle.result.await.unwrap();
    assert!(!result.ok);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|m| m.contains("denied")),
        "{:?}",
        result.error
    );
}

#[tokio::test(flavor = "current_thread")]
async fn suspending_run_can_allow_calls() {
    let suspending = SuspendingQuickJsEngine::from_engine(QuickJsEngine::default());
    let space = space_with(vec![echo_tool("weather.get", json!({ "celsius": 20 }))]);
    let mut handle = suspending
        .start(
            "const a = await tools.weather.get({}); const b = await tools.weather.get({}); return { a, b };",
            &RunContext::new(space),
        )
        .await
        .unwrap();

    // Allow both calls by running their dispatchers.
    while let Some(call) = handle.next_call.next().await {
        let outcome = (call.execute)(call.args.clone()).await;
        handle
            .deliveries
            .unbounded_send(rmcp_codemode::Delivery::new(
                call.id,
                Outcome::Deliver(outcome.unwrap()),
            ))
            .unwrap();
    }
    let result = handle.result.await.unwrap();
    assert!(result.ok, "{:?}", result.error);
    assert_eq!(
        result.value,
        Some(json!({ "a": { "celsius": 20 }, "b": { "celsius": 20 } }))
    );
    assert_eq!(result.calls.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn suspended_state_replays_recorded_calls() {
    // The replay loop: run once under host control, record the answers,
    // then resume from the state — recorded calls replay from the recording
    // (no second execution), calls past the recording execute for real.
    let suspending = SuspendingQuickJsEngine::from_engine(QuickJsEngine::default());
    let space = space_with(vec![
        echo_tool("a.first", json!("first-result")),
        echo_tool("a.second", json!("second-result")),
    ]);
    let mut handle = suspending
        .start(
            "const x = await tools.a.first({}); const y = await tools.a.second({}); return { x, y };",
            &RunContext::new(space.clone()),
        )
        .await
        .unwrap();

    // Record the first call's answer, then "pause" by dropping the handle:
    // the second call is never answered, so the run ends cancelled.
    let first = handle.next_call.next().await.unwrap();
    assert_eq!(first.name, "a.first");
    handle
        .deliveries
        .unbounded_send(rmcp_codemode::Delivery::new(
            first.id,
            Outcome::Deliver(json!("first-result")),
        ))
        .unwrap();
    // Give the dispatcher a moment to consume the answer, then stop.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop(handle);

    let state = SuspendState::new(
        "const x = await tools.a.first({}); const y = await tools.a.second({}); return { x, y };",
        vec![rmcp_codemode::AnsweredCall::new(
            0,
            "a.first",
            rmcp_codemode::RecordedOutcome::Ok(json!("first-result")),
        )],
    );

    // Resume: the first call replays from the recording, the second runs.
    let mut resumed = suspending
        .resume(state, &RunContext::new(space))
        .await
        .unwrap();
    let second = resumed.next_call.next().await.unwrap();
    assert_eq!(
        second.name, "a.second",
        "the first call replays, the second yields"
    );
    resumed
        .deliveries
        .unbounded_send(rmcp_codemode::Delivery::new(
            second.id,
            Outcome::Deliver(json!("second-result")),
        ))
        .unwrap();
    let result = resumed.result.await.unwrap();
    assert!(result.ok, "{:?}", result.error);
    assert_eq!(
        result.value,
        Some(json!({ "x": "first-result", "y": "second-result" }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn options_line_overrides_the_budget() {
    let engine = QuickJsEngine::default();
    let result = engine
        .execute(
            "// @options: {\"max_output_tokens\": 100}\nreturn \"z\".repeat(2000);",
            &RunContext::new(Space::new()),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.max_output_tokens, 100);
    let rendered = rmcp_codemode::render_result_text(&result);
    assert!(rendered.contains("output truncated"), "{rendered}");
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_options_fail_before_the_script_runs() {
    let engine = QuickJsEngine::default();
    let result = engine
        .execute(
            "// @options: {\"bogus\": 1}\nreturn 1;",
            &RunContext::new(Space::new()),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|m| m.contains("@options"))
    );
    assert!(result.calls.is_empty(), "no calls before the script ran");
}

#[tokio::test(flavor = "current_thread")]
async fn discovery_describe_namespace_carries_instructions() {
    let engine = QuickJsEngine::default();
    let mut space = space_with(vec![echo_tool("mcp__docs.search", json!("doc hits"))]);
    space.set_namespace(
        "mcp__docs".to_owned(),
        rmcp_codemode::NamespaceMeta::new(
            Some("Documentation search".to_owned()),
            Some("Use for repository docs.".to_owned()),
            vec!["docs".to_owned()],
        ),
    );
    let result = engine
        .execute(
            "return await describeNamespace(\"docs\");",
            &RunContext::new(space),
        )
        .await
        .unwrap();
    let namespace = result.value.unwrap();
    assert_eq!(namespace["name"], "mcp__docs");
    assert_eq!(namespace["instructions"], "Use for repository docs.");
    assert_eq!(namespace["tools"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_member_gets_pi_guard_message() {
    let engine = QuickJsEngine::default();
    let space = space_with(vec![echo_tool("web_search", json!("web hits"))]);
    let result = engine
        .execute(
            r#"
            try { await tools.websearch(); return "no error"; }
            catch (e) { return e.message; }
            "#,
            &RunContext::new(space),
        )
        .await
        .unwrap();
    let message = result
        .value
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    assert!(
        message.contains("Did you mean tools.web_search?"),
        "{message}"
    );
    assert!(message.contains("Check for a member with"), "{message}");
}

#[tokio::test(flavor = "current_thread")]
async fn exit_ends_successfully_even_when_swallowed() {
    let engine = QuickJsEngine::default();
    let result = engine
        .execute(
            r#"
            text("before");
            try { exit(); } catch {}
            text("after");
            return "unreachable";
            "#,
            &RunContext::new(Space::new()),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(result.value, None);
    assert_eq!(
        result.output,
        vec![rmcp_codemode::OutputItem::Text {
            text: "before".to_owned()
        }]
    );
}

#[allow(dead_code)]
fn flat_helper(segments: &[&str]) -> String {
    let owned: Vec<String> = segments.iter().map(|s| (*s).to_owned()).collect();
    flat_identifier(&owned)
}

#[tokio::test(flavor = "current_thread")]
async fn determinism_same_seed_same_clock_same_script_same_results() {
    // pi's replay contract: the same script, startedAt, seed, tools, and
    // delivered results produce the same observable behavior.
    let script = r#"
        const a = Math.random();
        const t1 = Date.now();
        const x = await tools.a.first({});
        const b = Math.random();
        const t2 = Date.now();
        return { a, t1, x, b, t2 };
    "#;
    let run = || async {
        let space = space_with(vec![echo_tool("a.first", json!({ "v": 1 }))]);
        let context =
            RunContext::new(space).with_determinism(rmcp_codemode::Determinism::new(1_000_000, 42));
        let mut handle = SuspendingQuickJsEngine::from_engine(QuickJsEngine::default())
            .start(script, &context)
            .await
            .unwrap();
        let call = handle.next_call.next().await.unwrap();
        // The host delivers clock values with its answers (pi's
        // deliver(seq, now, id, outcome)).
        handle
            .deliveries
            .unbounded_send(rmcp_codemode::Delivery::with_now(
                call.id,
                Outcome::Deliver(json!({ "v": 1 })),
                1_000_500,
            ))
            .unwrap();
        handle.result.await.unwrap()
    };
    let first = run().await;
    let second = run().await;
    assert!(
        first.ok && second.ok,
        "{:?} {:?}",
        first.error,
        second.error
    );
    assert_eq!(first.value, second.value, "replay must align exactly");
    let value = first.value.clone().unwrap();
    assert_eq!(value["t1"], 1_000_000, "clock starts at started_at");
    assert_eq!(value["t2"], 1_000_500, "clock advances with the delivery");
    assert!(value["a"].as_f64().unwrap() != value["b"].as_f64().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn determinism_seed_controls_randomness_and_clock_never_goes_backwards() {
    let script = r#"return [Math.random(), Math.random(), Date.now()];"#;
    let observe = |seed: u32| async move {
        let engine = QuickJsEngine::default();
        let context = RunContext::new(Space::new())
            .with_determinism(rmcp_codemode::Determinism::new(7, seed));
        engine.execute(script, &context).await.unwrap()
    };
    let a = observe(42).await;
    let b = observe(42).await;
    let c = observe(43).await;
    assert_eq!(a.value, b.value, "same seed, same draws");
    assert_ne!(a.value, c.value, "different seed, different draws");
    // The clock without deliveries stays at started_at.
    assert_eq!(a.value.unwrap()[2], 7);
}

#[tokio::test(flavor = "current_thread")]
async fn resumed_run_replays_recorded_clock_values() {
    // A paused run records the clock values of its answered calls; the
    // resumed run's virtual clock follows the same trajectory (pi: the
    // host saves startedAt and seed with the session).
    let script = r#"
        const t1 = Date.now();
        await tools.a.first({});
        const t2 = Date.now();
        await tools.a.second({});
        const t3 = Date.now();
        return { t1, t2, t3 };
    "#;
    let engine = SuspendingQuickJsEngine::from_engine(QuickJsEngine::default());
    let space = space_with(vec![
        echo_tool("a.first", json!(1)),
        echo_tool("a.second", json!(2)),
    ]);
    let determinism = rmcp_codemode::Determinism::new(5_000, 9);
    let context = RunContext::new(space).with_determinism(determinism);

    // Run 1: answer both calls with clock values, keeping the recording.
    let mut handle = engine.start(script, &context).await.unwrap();
    let mut answered = Vec::new();
    for (id, now) in [(0u64, 5_100u64), (1, 5_300)] {
        let call = handle.next_call.next().await.unwrap();
        answered.push(
            rmcp_codemode::AnsweredCall::new(
                id,
                call.name.clone(),
                rmcp_codemode::RecordedOutcome::Ok(json!(id + 1)),
            )
            .with_now(now),
        );
        handle
            .deliveries
            .unbounded_send(rmcp_codemode::Delivery::with_now(
                call.id,
                Outcome::Deliver(json!(id + 1)),
                now,
            ))
            .unwrap();
    }
    let first = handle.result.await.unwrap();
    assert!(first.ok, "{:?}", first.error);
    let observed: Vec<_> = first
        .value
        .clone()
        .unwrap()
        .as_object()
        .unwrap()
        .values()
        .cloned()
        .collect();
    assert_eq!(observed, vec![json!(5_000), json!(5_100), json!(5_300)]);

    // Run 2: replay from a state carrying the recording. Both calls are
    // in the recording, so the replay needs no further answers.
    let state = SuspendState::new(script, answered).with_determinism(determinism);
    let resumed = engine.resume(state, &context).await.unwrap();
    let second = resumed.result.await.unwrap();
    assert!(second.ok, "{:?}", second.error);
    assert_eq!(first.value, second.value, "replay observes the same clocks");
}
