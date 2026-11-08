//! Sandbox conformance tests, ported from pi's `sandbox.test.ts` scenarios.

use std::{sync::Arc, time::Duration};

use rmcp_codemode_quickjs::{OutputItem, Sandbox, SandboxTool};
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
            OutputItem::Console(_) | OutputItem::Image { .. } => None,
        })
        .collect()
}

fn console_of(result: &rmcp_codemode_quickjs::ScriptResult) -> Vec<String> {
    result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Console(t) => Some(t.clone()),
            OutputItem::Text(_) | OutputItem::Image { .. } => None,
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
    // A pure-JS busy loop is preempted by the VM's interrupt handler and
    // reported as a deadline failure.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "text(\"before\"); while (true) {}",
            vec![],
            Some(Duration::from_millis(200)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.error.as_deref(),
        Some("script exceeded its deadline")
    );
    // Partial output produced before the deadline is kept.
    assert_eq!(result.output.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn never_settling_promise_fails_immediately() {
    // Ported from pi's "fails a script that waits on a promise nothing can
    // settle": with no timers in the sandbox, a promise with no tool call
    // pending can never resolve, so the script fails right away — even
    // without a deadline. Returning while a call is still pending is not a
    // stall.
    let sandbox = Sandbox::new().await.unwrap();
    let started = std::time::Instant::now();
    let result = sandbox
        .execute("await new Promise(() => {}); return 'never'", vec![], None)
        .await
        .unwrap();
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|m| m.contains("can never settle")),
        "got: {:?}",
        result.error
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    // With a pending tool call, waiting is legitimate and succeeds.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "return await tools.slow()",
            vec![slow_tool(
                "slow",
                "Resolves after a delay",
                200,
                json!("late"),
            )],
            None,
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(result.value, Some(json!("late")));
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

// ---- Ported from pi's codemode design (docs/codemode.md): discovery
// globals, exit(), the @options line, output caps, and result rendering. ----

fn schema_tool(name: &str, description: &str, schema: Value, result: Value) -> SandboxTool {
    SandboxTool::new(
        name,
        description,
        Arc::new(move |_| {
            let result = result.clone();
            Box::pin(async move { serde_json::to_string(&result).map_err(|e| e.to_string()) })
        }),
    )
    .with_input_schema(schema)
}

#[tokio::test(flavor = "current_thread")]
async fn search_tools_ranks_by_query() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "return (await searchTools(\"web\")).items.map((h) => h.name);",
            vec![
                sync_tool("echo", "Echo a value back", json!("echo")),
                sync_tool("web-search", "Search the web", json!("s")),
                sync_tool("webscraper", "Scrape web pages", json!("w")),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    // Both name-matching tools rank; the unrelated echo does not.
    let names = result.value.unwrap();
    assert!(names.as_array().unwrap().iter().all(|n| n != "echo"));
    assert_eq!(names.as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn search_tools_limit_and_namespace_and_empty_query() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            return {
                limited: (await searchTools("", { limit: 1 })).items.length,
                empty: (await searchTools("")).items.map((h) => h.name),
                namespace: (await searchTools("", { namespace: "github" })).items.map((h) => h.name),
            };
            "#,
            vec![
                sync_tool("echo", "Echo", json!("e")),
                sync_tool("github.list_issues", "List issues", json!("i")),
                sync_tool("github.create_issue", "Create an issue", json!("c")),
                sync_tool("other.thing", "Something else", json!("t")),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(
        result.value,
        Some(json!({
            "limited": 1,
            "empty": ["echo", "github.create_issue", "github.list_issues", "other.thing"],
            "namespace": ["github.create_issue", "github.list_issues"],
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn describe_tool_renders_declaration() {
    let sandbox = Sandbox::new().await.unwrap();
    let schema = json!({
        "type": "object",
        "properties": {
            "owner": { "type": "string", "description": "Repository owner" },
            "limit": { "type": "integer" }
        },
        "required": ["owner"]
    });
    let result = sandbox
        .execute(
            r#"
            return {
                known: await describeTool("github.list_issues"),
                alias: (await describeTool("github_list_issues")) !== undefined,
                unknown: await describeTool("nope"),
            };
            "#,
            vec![schema_tool(
                "github.list_issues",
                "List issues in a repository",
                schema,
                json!(null),
            )],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    let known = &result.value.as_ref().unwrap()["known"];
    assert_eq!(known["name"], "github.list_issues");
    let declaration = known["declaration"].as_str().unwrap();
    assert!(
        declaration.contains("declare function list_issues("),
        "{declaration}"
    );
    assert!(
        declaration.contains("/** Repository owner */ owner: string;"),
        "{declaration}"
    );
    assert!(declaration.contains("limit?: number;"), "{declaration}");
    assert_eq!(result.value.as_ref().unwrap()["alias"], json!(true));
    assert_eq!(result.value.as_ref().unwrap()["unknown"], json!(null));
}

#[tokio::test(flavor = "current_thread")]
async fn describe_namespace_lists_members() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "return await describeNamespace(\"github\");",
            vec![
                sync_tool("github.list_issues", "List issues", json!(null)),
                sync_tool("other.thing", "Something else", json!(null)),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    let namespace = result.value.unwrap();
    assert_eq!(namespace["name"], "github");
    let tools = namespace["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "github.list_issues");
    assert!(
        tools[0]["declaration"]
            .as_str()
            .unwrap()
            .contains("declare function")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn exit_ends_script_successfully_even_when_swallowed() {
    // Ported from pi's "ends the script successfully on exit(), keeping
    // output": a swallowed exit() still ends the script — nothing after it
    // runs, the value is gone, and the output produced before it is kept.
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            text("before");
            await tools.echo(1);
            try { exit(); } catch {}
            text("after");
            return "unreachable";
            "#,
            vec![sync_tool("echo", "Echo", json!("ok"))],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(result.value, None);
    assert_eq!(result.output, vec![OutputItem::Text("before".to_owned())]);
}

#[tokio::test(flavor = "current_thread")]
async fn options_line_sets_output_budget_and_timeout() {
    let sandbox = Sandbox::new().await.unwrap();
    // max_output_tokens truncates the rendered result, keeping start and end.
    let result = sandbox
        .execute(
            "// @options: {\"max_output_tokens\": 100}\nreturn \"x\".repeat(2000);",
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.max_output_tokens, 100);
    let rendered = rmcp_codemode_quickjs::render_result_text(&result);
    assert!(rendered.contains("output truncated"), "{rendered}");
    assert!(rendered.starts_with("Script completed"));
    assert!(rendered.contains('x')); // start kept
    assert!(rendered.len() < 600);

    // timeout_ms replaces the host deadline and preempts a busy loop.
    let started = std::time::Instant::now();
    let result = sandbox
        .execute(
            "// @options: {\"timeout_ms\": 100}\nwhile (true) {}",
            vec![],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        result.error.as_deref(),
        Some("script exceeded its deadline")
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_options_lines_fail_before_the_script_runs() {
    let sandbox = Sandbox::new().await.unwrap();
    for source in [
        "// @options: {oops}\nreturn 1;",
        "// @options: {\"nope\": 1}\nreturn 1;",
        "return 1;\n// @options: {\"timeout_ms\": 5}",
        "// @options: \"a string\"\nreturn 1;",
    ] {
        let result = sandbox.execute(source, vec![], None).await.unwrap();
        assert!(!result.ok, "source should fail: {source}");
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|m| m.contains("@options")),
            "source: {source}, error: {:?}",
            result.error
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn output_call_cap_fails_the_script() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "for (let i = 0; i < 100001; i++) console.log(\"x\");",
            vec![],
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    assert!(!result.ok, "the script should fail at the call cap");
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|m| m.contains("output limit exceeded")),
        "got: {:?}",
        result.error
    );
    assert_eq!(result.output.len(), rmcp_codemode_quickjs::MAX_OUTPUT_CALLS);
}

#[tokio::test(flavor = "current_thread")]
async fn render_result_text_matches_pi_format() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            text("first");
            text("second");
            console.log("a log line");
            return { done: true };
            "#,
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let rendered = rmcp_codemode_quickjs::render_result_text(&result);
    assert_eq!(
        rendered,
        "Script completed in Xms\n\
         ==> text 1/3 <==\nfirst\n\
         ==> text 2/3 <==\nsecond\n\
         ==> text 3/3 <==\n{\"done\":true}\n\
         <console_output>\na log line"
            .replace(" in Xms", &format!(" in {}ms", result.duration_ms))
    );

    // A failure keeps its partial output and ends with `Script error:`.
    let result = sandbox
        .execute(
            "text(\"partial\");\nthrow new Error(\"boom\");",
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let rendered = rmcp_codemode_quickjs::render_result_text(&result);
    assert!(rendered.starts_with("Script failed in"));
    assert!(rendered.contains("partial"));
    assert!(rendered.ends_with("Script error: boom"), "{rendered}");
}

// ---- store/load (pi's session-backed store semantics) ----

#[tokio::test(flavor = "current_thread")]
async fn store_commits_on_success_and_reads_across_runs() {
    use std::{collections::BTreeMap, sync::Mutex};

    use rmcp_codemode_quickjs::Store;

    let store: Store = Arc::new(Mutex::new(BTreeMap::new()));
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute_with_store(
            r#"
            store("cursor", 42);
            store("note", "keep me");
            text(String(load("cursor") * 2));
            return load("cursor");
            "#,
            vec![],
            Some(Duration::from_secs(5)),
            Some(&store),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    // Within the run, the pending write is readable.
    assert_eq!(result.output, vec![OutputItem::Text("84".to_owned())]);
    // Committed for the next run.
    assert_eq!(store.lock().unwrap().get("cursor"), Some(&json!(42)));

    let result = sandbox
        .execute_with_store(
            "return load(\"note\");",
            vec![],
            Some(Duration::from_secs(5)),
            Some(&store),
        )
        .await
        .unwrap();
    assert_eq!(result.value, Some(json!("keep me")));
}

#[tokio::test(flavor = "current_thread")]
async fn failed_script_discards_store_writes() {
    use std::{collections::BTreeMap, sync::Mutex};

    use rmcp_codemode_quickjs::Store;

    let store: Store = Arc::new(Mutex::new(BTreeMap::new()));
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute_with_store(
            "store(\"k\", 1);\nthrow new Error(\"boom\");",
            vec![],
            Some(Duration::from_secs(5)),
            Some(&store),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    assert!(
        store.lock().unwrap().is_empty(),
        "failed writes must not commit"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn storing_undefined_deletes_the_key() {
    use std::{collections::BTreeMap, sync::Mutex};

    use rmcp_codemode_quickjs::Store;

    let store: Store = Arc::new(Mutex::new(BTreeMap::new()));
    store.lock().unwrap().insert("gone".to_owned(), json!(1));
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute_with_store(
            "store(\"gone\", undefined); return load(\"gone\");",
            vec![],
            Some(Duration::from_secs(5)),
            Some(&store),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(result.value, None);
    assert!(!store.lock().unwrap().contains_key("gone"));
}

// ---- image output ----

#[tokio::test(flavor = "current_thread")]
async fn image_parses_data_uris_and_rejects_garbage() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            image("data:image/png;base64,aGVsbG8=");
            try { image("data:"); } catch (e) { return e.message; }
            "#,
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(json!(
            "image() expects a base64 data URI like data:image/png;base64,..."
        ))
    );
    assert_eq!(
        result.output,
        vec![OutputItem::Image {
            data: "aGVsbG8=".to_owned(),
            mime_type: "image/png".to_owned(),
        }]
    );
}

// ---- namespace descriptions from the host ----

#[tokio::test(flavor = "current_thread")]
async fn describe_namespace_includes_host_description() {
    use std::collections::BTreeMap;
    let mut sandbox = Sandbox::new().await.unwrap();
    let mut descriptions = BTreeMap::new();
    descriptions.insert("github".to_owned(), "The GitHub API".to_owned());
    sandbox.set_namespace_descriptions(descriptions);
    let result = sandbox
        .execute(
            "return await describeNamespace(\"github\");",
            vec![sync_tool("github.list_issues", "List issues", json!(null))],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let namespace = result.value.unwrap();
    assert_eq!(namespace["description"], "The GitHub API");
    assert_eq!(namespace["name"], "github");
}

// ---- pi's runtime semantics: unawaited calls are cancelled ----

#[tokio::test(flavor = "current_thread")]
async fn unawaited_tool_calls_are_cancelled_when_the_script_ends() {
    // pi: "calls still running when the script ends are cancelled, and
    // unawaited promises are discarded." Returning while a 500ms call is in
    // flight must not wait it out, and the cancelled call must not record.
    let sandbox = Sandbox::new().await.unwrap();
    let started = std::time::Instant::now();
    let result = sandbox
        .execute(
            "tools.slow(); return \"early\";",
            vec![slow_tool(
                "slow",
                "Resolves after a delay",
                500,
                json!("late"),
            )],
            None,
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(result.value, Some(json!("early")));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(300),
        "the unawaited call must be cancelled, not waited out"
    );
    assert!(
        result.calls.is_empty(),
        "cancelled calls are not recorded: {:?}",
        result.calls
    );

    // A fresh run on the same sandbox sees none of the stale job: no leaked
    // call records, no late side effects.
    let result = sandbox
        .execute(
            "return ALL_TOOLS.length;",
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(result.value, Some(json!(0)));
}

// ---- pi's overflow file: full output written, path included ----

#[tokio::test(flavor = "current_thread")]
async fn truncated_output_writes_the_full_text_to_a_file() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            "// @options: {\"max_output_tokens\": 100}\nreturn \"y\".repeat(2000);",
            vec![],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let dir = std::env::temp_dir();
    let rendered = rmcp_codemode_quickjs::render_result_text_with_overflow(&result, &dir);
    let marker = "Full output: ";
    let path_line = rendered
        .lines()
        .find(|l| l.starts_with(marker))
        .expect("overflow path");
    let path = std::path::PathBuf::from(&path_line[marker.len()..]);
    let full = std::fs::read_to_string(&path).expect("overflow file readable");
    // The file holds the *untruncated* rendering; the returned text is the
    // truncated one plus the path line.
    assert!(full.len() > 2000, "full output kept: {}", full.len());
    assert!(full.contains("yyyyyyyyyy"));
    assert!(rendered.contains("output truncated"));
    assert!(rendered.len() < full.len());
    std::fs::remove_file(&path).ok();
}

// ---- pi's discovery semantics: paging, namespace instructions, aliases ----

#[tokio::test(flavor = "current_thread")]
async fn search_tools_pages_with_offset_and_next() {
    let sandbox = Sandbox::new().await.unwrap();
    let result = sandbox
        .execute(
            r#"
            const first = await searchTools("", { limit: 2 });
            const second = first.next === null
                ? { items: [], remaining: 0 }
                : await searchTools("", { limit: 2, offset: first.next.offset ?? first.next });
            return {
                firstCount: first.items.length,
                remaining: first.remaining,
                firstNames: first.items.map((h) => h.name),
                secondNames: second.items.map((h) => h.name),
                noOverlap: !first.items.some((h) => second.items.some((g) => g.name === h.name)),
            };
            "#,
            vec![
                sync_tool("a.one", "First", json!(1)),
                sync_tool("a.two", "Second", json!(2)),
                sync_tool("b.three", "Third", json!(3)),
            ],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    assert_eq!(
        result.value,
        Some(json!({
            "firstCount": 2,
            "remaining": 1,
            "firstNames": ["a.one", "a.two"],
            "secondNames": ["b.three"],
            "noOverlap": true,
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn describe_namespace_returns_instructions_and_resolves_aliases() {
    use std::collections::BTreeMap;
    let mut sandbox = Sandbox::new().await.unwrap();
    let mut infos = BTreeMap::new();
    infos.insert(
        "mcp__docs".to_owned(),
        rmcp_codemode::NamespaceInfo {
            description: Some("Documentation search".to_owned()),
            instructions: Some("Use for repository documentation questions.".to_owned()),
            aliases: vec!["docs".to_owned()],
        },
    );
    sandbox.set_namespace_infos(infos);
    let sandbox_ref = &sandbox;
    let result = sandbox_ref
        .execute(
            r#"
            return {
                byAlias: await describeNamespace("docs"),
                byCanonical: await describeNamespace("mcp__docs"),
                byNormalized: await describeNamespace("mcp__docs".replaceAll("-", "_")),
            };
            "#,
            vec![sync_tool("mcp__docs.search", "Search docs", json!(null))],
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.ok, "error: {:?}", result.error);
    let value = result.value.unwrap();
    for key in ["byAlias", "byCanonical", "byNormalized"] {
        let namespace = &value[key];
        assert_eq!(namespace["name"], "mcp__docs", "{key}");
        assert_eq!(namespace["description"], "Documentation search", "{key}");
        assert_eq!(
            namespace["instructions"], "Use for repository documentation questions.",
            "{key}"
        );
        assert_eq!(namespace["tools"].as_array().unwrap().len(), 1, "{key}");
    }
}
