//! Sandbox conformance tests, ported from pi's `sandbox.test.ts` scenarios.

use std::{sync::Arc, time::Duration};

use rmcp_codemode_quickjs::{OutputItem, Sandbox, SandboxError, SandboxTool};
use serde_json::{Value, json};

fn sync_tool(name: &str, description: &str, result: Value) -> SandboxTool {
    SandboxTool::new(
        name,
        description,
        Arc::new(move |_| {
            let result = result.clone();
            Box::pin(async move { serde_json::to_string(&result).map_err(|e| e.to_string()) })
        }),
    )
}

fn slow_tool(name: &str, description: &str, delay_ms: u64, result: Value) -> SandboxTool {
    SandboxTool::new(
        name,
        description,
        Arc::new(move |_| {
            let result = result.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                serde_json::to_string(&result).map_err(|e| e.to_string())
            })
        }),
    )
}

fn text_of(result: &rmcp_codemode_quickjs::ScriptResult) -> Vec<String> {
    result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Text(t) => Some(t.clone()),
            OutputItem::Console(_) => None,
        })
        .collect()
}

fn console_of(result: &rmcp_codemode_quickjs::ScriptResult) -> Vec<String> {
    result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Console(t) => Some(t.clone()),
            OutputItem::Text(_) => None,
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn return_value_and_await() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "return 1 + await Promise.resolve(41);",
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.value, Some(json!(42)));
}

#[tokio::test(flavor = "current_thread")]
async fn returned_awaited_object_is_value() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "const x = await Promise.resolve({ a: 1 });\nreturn x;",
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.value, Some(json!({"a": 1})));
}

#[tokio::test(flavor = "current_thread")]
async fn tool_call_json_round_trip() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "const r = await tools.echo.echo({ msg: \"hi\", n: [1, 2] });\nreturn r;",
            vec![sync_tool(
                "echo.echo",
                "echoes",
                json!({"msg": "hi", "n": [1, 2]}),
            )],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "script failed");
    assert_eq!(result.value, Some(json!({"msg": "hi", "n": [1, 2]})));
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "echo.echo");
}

#[tokio::test(flavor = "current_thread")]
async fn nested_and_parallel_tool_calls() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            const [a, b] = await Promise.all([
                tools.slow.one({}),
                tools.slow.two({}),
            ]);
            return { sum: a.v + b.v };
            "#,
            vec![
                slow_tool("slow.one", "returns 1", 50, json!({"v": 1})),
                slow_tool("slow.two", "returns 2", 50, json!({"v": 2})),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.value, Some(json!({"sum": 3})));
    assert_eq!(result.calls.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_tool_fails_with_message() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "await tools.github.list_issue({});",
            vec![sync_tool("github.list_issues", "lists", json!(null))],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("Unknown tool"), "got: {error}");
}

#[tokio::test(flavor = "current_thread")]
async fn tool_error_rejects_as_js_error() {
    let sandbox = Sandbox::new().await.unwrap();
    let boom = SandboxTool::new(
        "boom.boom",
        "fails",
        Arc::new(move |_| Box::pin(async { Err("upstream exploded".to_owned()) })),
    );
    let result = sandbox
        .execute(
            r#"
            try {
                await tools.boom.boom({});
                return "no throw";
            } catch (e) {
                return { caught: true, message: e.message };
            }
            "#,
            vec![boom],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(
        result.value,
        Some(json!({"caught": true, "message": "boom.boom: upstream exploded"}))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn text_and_console_output() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            text("plain");
            text({ json: true });
            console.log("log", 42, { a: 1 });
            console.warn("warned");
            return "done";
            "#,
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.value, Some(json!("done")));
    assert_eq!(text_of(&result), ["plain", r#"{"json":true}"#]);
    assert_eq!(console_of(&result), ["log 42 {\"a\":1}", "warned"]);
}

#[tokio::test(flavor = "current_thread")]
async fn script_error_keeps_partial_output() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            text("before");
            throw new Error("kaboom");
            "#,
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    assert_eq!(text_of(&result), ["before"]);
}

#[tokio::test(flavor = "current_thread")]
async fn syntax_error_fails_cleanly() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute("const const =", vec![], Some(Duration::from_secs(5)))
        .await
        .unwrap();
    assert!(!result.ok);
    assert!(result.error.unwrap_or_default().contains("expected"));
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_stops_hanging_script() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            // A promise nothing can settle: no timers exist in the sandbox.
            "await new Promise(() => {});",
            vec![],
            Some(Duration::from_millis(200)),
        )
        .await;
    match result {
        Err(SandboxError::Script(message)) => assert!(message.contains("deadline")),
        other => panic!("expected deadline error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn script_thrown_error_is_reported_in_result() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "throw new Error(\"kaboom\");",
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    assert_eq!(result.error.as_deref(), Some("kaboom"));
    assert_eq!(result.value, None);
}

#[tokio::test(flavor = "current_thread")]
async fn no_timer_or_fetch_or_fs_reachable() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            return {
                setTimeout: typeof setTimeout,
                fetch: typeof fetch,
                require: typeof require,
                process: typeof process,
                setInterval: typeof setInterval,
            };
            "#,
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(json!({
            "setTimeout": "undefined",
            "fetch": "undefined",
            "require": "undefined",
            "process": "undefined",
            "setInterval": "undefined",
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn all_tools_lists_paths_and_descriptions() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "return ALL_TOOLS;",
            vec![
                sync_tool("github.list_issues", "List issues", json!(null)),
                sync_tool("github.search_code", "Search code", json!(null)),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(json!([
            {"name": "github.list_issues", "description": "List issues"},
            {"name": "github.search_code", "description": "Search code"},
        ]))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn memory_limit_raises_out_of_memory() {
    // 4 MB heap; a 16 MB string blows it.
    let sandbox = Sandbox::with_memory_limit(4 * 1024 * 1024).await.unwrap();
    let result = sandbox
        .execute(
            r#"
            let s = "x".repeat(16 * 1024 * 1024);
            return s.length;
            "#,
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(!result.ok);
}

// ---- Ported: pi sandbox.test.ts + OpenCode tool-paths.test.ts ----

#[tokio::test(flavor = "current_thread")]
async fn normalized_identifiers_reach_dashed_paths() {
    // Ported from pi's "exposes tools under normalized identifiers": a tool
    // registered as `my-tool` is callable as `tools.my_tool` and as
    // `tools["my-tool"]`; a flat identifier alias works too.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            return {
                dashed: await tools.my_tool(),
                bracket: await tools["my-tool"],
            };
            "#
            .replace("tools[\"my-tool\"]", "await tools[\"my-tool\"]()")
            .as_str(),
            vec![sync_tool("my-tool", "Dashes", json!("dash"))],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(
        result.value,
        Some(json!({"dashed": "dash", "bracket": "dash"}))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn all_tools_uses_canonical_paths() {
    // pi lists identifier names in ALL_TOOLS; our registry paths are already
    // valid namespace access, so ALL_TOOLS mirrors the canonical paths.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "return ALL_TOOLS.map((t) => t.name);",
            vec![
                sync_tool("github.list_issues", "List issues", json!(null)),
                sync_tool("my-tool", "Dashes", json!(null)),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(result.value, Some(json!(["github.list_issues", "my-tool"])));
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_tool_names_close_match() {
    // Ported from pi's "names close matches when a script reads a tool that
    // does not exist": token overlap suggests the real path; no overlap lists
    // what is available.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            try { await tools.websearch(); return "no error"; }
            catch (e) { return e.message; }
            "#,
            vec![
                sync_tool("echo", "Echo", json!(null)),
                sync_tool("web-search", "Web search", json!(null)),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let message = result
        .value
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    assert!(
        message.contains("Did you mean 'web-search'?"),
        "got: {message}"
    );

    let result = sandbox
        .execute(
            r#"
            try { await tools.nothing(); return "no error"; }
            catch (e) { return e.message; }
            "#,
            vec![
                sync_tool("echo", "Echo", json!(null)),
                sync_tool("web-search", "Web search", json!(null)),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let message = result
        .value
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    assert!(
        message.contains("Available: echo, web-search."),
        "got: {message}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn in_operator_and_tojson_stay_host_shaped() {
    // Ported from pi: `'echo' in tools` works, `JSON.stringify(tools)` is the
    // empty object, and `tools.toString` is undefined.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            return [
                "echo" in tools,
                "nothing" in tools,
                String(tools.toString),
                JSON.stringify(tools),
            ];
            "#,
            vec![sync_tool("echo", "Echo", json!(null))],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(json!([
            true,
            false,
            "undefined", // String(tools.toString) — toString is undefined
            "{}"
        ]))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn namespaces_enumerate_children() {
    // Ported from OpenCode tool-paths.test.ts: "intermediate segments
    // enumerate like ordinary namespaces" — Object.keys on a namespace lists
    // its child segments.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            return {
                top: Object.keys(tools),
                nested: Object.keys(tools.api),
                leaf: Object.keys(tools.api.issues),
            };
            "#,
            vec![
                sync_tool("api.issues.list", "List issues", json!("listed")),
                sync_tool("api.users.get", "Get user", json!("user")),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(json!({
            "top": ["api"],
            "nested": ["issues", "users"],
            "leaf": ["list"],
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn namespace_without_own_tool_is_not_callable() {
    // Ported from OpenCode tool-paths.test.ts: "a namespace without its own
    // tool stays non-callable".
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            try { await tools.issues({}); return "no error"; }
            catch (e) { return e.message; }
            "#,
            vec![sync_tool("issues.list", "List issues", json!(null))],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let message = result
        .value
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    assert!(
        message.contains("Tool 'issues' is not callable"),
        "got: {message}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_member_names_never_break_paths() {
    // Ported from OpenCode tool-paths.test.ts: tools may use names like
    // `constructor` or `__proto__` because path segments never touch real
    // properties, and a script cannot escape through Function.constructor.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            const object = {};
            object.__proto__ = { polluted: true };
            let escape = "no throw";
            try { ({}).constructor.constructor("return 1 + 1")(); } catch (e) { escape = "blocked"; }
            return {
                ctor: await tools.issues.constructor({}),
                proto: await tools.prototype({}),
                polluted: object.__proto__.polluted,
                objectCtor: object.constructor === Object,
                escape,
            };
            "#,
            vec![
                sync_tool("issues.constructor", "Constructor tool", json!("ctor")),
                sync_tool("prototype", "Prototype tool", json!("proto")),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(
        result.value,
        Some(json!({
            "ctor": "ctor",
            "proto": "proto",
            "polluted": true,
            "objectCtor": true,
            "escape": "no throw",
        }))
    );
}
