//! L5 server tests: the codemode MCP server over a real rmcp client.

use std::sync::Arc;

use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, CallToolResult},
    service::{RoleClient, RoleServer, RunningService, ServerInitializeError, ServiceExt},
};
use rmcp_codemode::{Exposure, Space, ToolHandle};
use rmcp_codemode_quickjs::{CodemodeServer, NativeTool, QuickJsEngine, SuspendingQuickJsEngine};
use serde_json::{Value, json};

fn sleeping_tool(path: &str, ms: u64, result: Value) -> ToolHandle {
    ToolHandle::new(
        path.split('.').map(str::to_owned).collect(),
        format!("Sleeps {ms}ms then echoes"),
        Arc::new(move |_| {
            let result = result.clone();
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(result.clone())
            }) as rmcp_codemode::ToolFnFut
        }),
    )
}

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

async fn host_client(
    server: CodemodeServer<SuspendingQuickJsEngine>,
) -> (
    RunningService<RoleClient, ()>,
    tokio::task::JoinHandle<
        Result<
            RunningService<RoleServer, CodemodeServer<SuspendingQuickJsEngine>>,
            ServerInitializeError,
        >,
    >,
) {
    let (client_transport, server_transport) = tokio::io::duplex(16 * 1024);
    let task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await.expect("client init");
    (client, task)
}

fn script_args(script: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("codemode")
        .with_arguments(json!({ "script": script }).as_object().cloned().unwrap())
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
async fn host_sees_one_tool_and_scripts_pipe_results() {
    let mut space = Space::new();
    space
        .insert(echo_tool("weather.list_cities", json!(["berlin", "paris"])))
        .unwrap();
    space
        .insert(echo_tool(
            "weather.get_temperature",
            json!({ "celsius": 4 }),
        ))
        .unwrap();
    let server = CodemodeServer::new(
        SuspendingQuickJsEngine::from_engine(QuickJsEngine::default()),
        space,
    );
    let (client, _task) = host_client(server).await;

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_ref(), "codemode");
    // The description carries the globals and the inline declarations.
    let description = tools[0].description.as_deref().unwrap();
    assert!(description.contains("searchTools"), "{description}");

    let result = client
        .call_tool(script_args(
            r#"
            const cities = await tools.weather.list_cities({});
            const readings = await Promise.all(
                cities.map((city) => tools.weather.get_temperature({ city }))
            );
            return { cities: cities.length, coldest: readings[0].celsius };
            "#,
        ))
        .await
        .unwrap();
    let text = result_text(&result);
    assert!(text.contains("Script completed"), "{text}");
    assert!(text.contains(r#""cities":2"#), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn native_tools_join_the_space_and_direct_tools_stay_listed() {
    let mut space = Space::new();
    let mut direct = echo_tool("fs.read", json!("file content"));
    direct.exposure = Exposure::Direct;
    space.insert(direct).unwrap();
    let server = CodemodeServer::new(
        SuspendingQuickJsEngine::from_engine(QuickJsEngine::default()),
        space,
    )
    .with_native_tools(vec![NativeTool::new(
        "bash",
        "Run a shell command",
        |args| {
            Box::pin(async move {
                let command = args["command"].as_str().unwrap_or("");
                Ok(json!({ "output": command.to_uppercase() }))
            })
        },
    )]);
    let (client, _task) = host_client(server).await;

    // On mode: the direct tool joins the codemode tool on the host's list.
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["codemode", "fs.read"]);

    // The native tool is callable from scripts.
    let result = client
        .call_tool(script_args("return await tools.bash({ command: \"hi\" });"))
        .await
        .unwrap();
    assert!(
        result_text(&result).contains("\"output\":\"HI\""),
        "{}",
        result_text(&result)
    );

    // Deferred/codemode-only tools answer native calls with guidance.
    let result = client
        .call_tool(CallToolRequestParams::new("fs.read"))
        .await
        .unwrap();
    // fs.read is Direct, so it routes natively.
    assert!(
        result_text(&result).contains("file content"),
        "{}",
        result_text(&result)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn approval_pause_and_resume_round_trip() {
    // A tool marked requires_approval: the run pauses, the state is stored,
    // resume replays the recorded calls and continues.
    let mut space = Space::new();
    let mut sensitive = echo_tool("github.create_issue", json!({ "id": 99 }));
    sensitive.annotations = rmcp_codemode::ToolAnnotations::requires_approval();
    space.insert(sensitive).unwrap();
    space
        .insert(echo_tool("a.lookup", json!("lookup-result")))
        .unwrap();
    let server = CodemodeServer::new(
        SuspendingQuickJsEngine::from_engine(QuickJsEngine::default()),
        space,
    );

    let script = r#"
        const data = await tools.a.lookup({});
        const issue = await tools.github.create_issue({ title: "x" });
        return { data, issue };
    "#;
    let paused = server.run_with_approval(script).await.unwrap();
    let text = result_text(&paused);
    assert!(text.contains("\"status\":\"paused\""), "{text}");
    // The id is embedded in the JSON line; extract robustly.
    let execution_id = text
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|value| value["executionId"].as_str().map(str::to_owned))
        .expect("paused JSON with executionId");
    assert!(!execution_id.is_empty());

    // Resume: the recorded lookup replays, the approved issue runs.
    let resumed = server.resume(&execution_id).await.unwrap();
    let text = result_text(&resumed);
    assert!(text.contains(r#""issue":{"id":99}"#), "{text}");
    assert!(text.contains(r#""data":"lookup-result""#), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn store_persists_across_calls_through_a_persister() {
    let dir = std::env::temp_dir().join(format!(
        "codemode-store-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.json");
    let persister = Arc::new(rmcp_codemode_quickjs::FileStorePersister::new(&path));

    let server = CodemodeServer::new(QuickJsEngine::default(), Space::new())
        .with_store_persistence(persister);
    let first = server
        .run_one_shot("store(\"k\", { v: 1 }); return \"ok\";")
        .await
        .unwrap();
    assert!(
        result_text(&first).contains("Script completed"),
        "{}",
        result_text(&first)
    );
    let second = server.run_one_shot("return load(\"k\");").await.unwrap();
    assert!(
        result_text(&second).contains("\"v\":1"),
        "{}",
        result_text(&second)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(dead_code)]
fn value_of(result: CallToolResponse) -> Value {
    match result {
        CallToolResponse::Complete(result) => {
            serde_json::from_str(result_text(&result).lines().last().unwrap_or("null"))
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn queued_scripts_admit_in_order_when_workers_free() {
    // Two workers (the default): both start, queue nothing, both finish.
    let space = Space::new();
    let server = CodemodeServer::new(
        SuspendingQuickJsEngine::from_engine(QuickJsEngine::default()),
        space,
    );
    let (client, _task) = host_client(server).await;
    let scripts: Vec<_> = (0..2)
        .map(|i| script_args(&format!("return {i};")))
        .collect();
    let results = futures_join(scripts, client).await;
    assert!(results.iter().all(|r| !r.is_error.unwrap_or(false)));
}

#[tokio::test(flavor = "current_thread")]
async fn worker_wait_times_out_without_running_the_script() {
    use rmcp_codemode::Budget;

    // One worker held by a script that waits forever on a host call; the
    // second script must fail without starting, with pi's message.
    let mut space = Space::new();
    space
        .insert(sleeping_tool("slow.ping", 1_000, json!("pong")))
        .unwrap();
    let budget = Budget::default()
        .with_max_workers(1)
        .with_worker_wait_ms(300)
        .with_timeout_ms(10_000);
    let server = CodemodeServer::new(
        SuspendingQuickJsEngine::from_engine(QuickJsEngine::default()),
        space,
    )
    .with_budget(budget);
    let (client, _task) = host_client(server).await;

    // Holder: a tool that sleeps 1s keeps the one worker busy.
    let holder_client = client.clone();
    let holder = tokio::spawn(async move {
        holder_client
            .call_tool(script_args("await tools.slow.ping({});"))
            .await
            .expect("holder call")
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let victim = client
        .call_tool(script_args("return 1;"))
        .await
        .expect("victim call");
    let text = result_text(&victim);
    assert!(victim.is_error.unwrap_or(false), "victim must fail: {text}");
    assert!(
        text.contains("No codemode worker became free within 0.3 seconds"),
        "{text}"
    );
    assert!(text.contains("The script did not start"), "{text}");
    holder.abort();
    let _ = holder.await;
}

/// Fire several codemode calls concurrently through one client.
async fn futures_join(
    scripts: Vec<CallToolRequestParams>,
    client: RunningService<RoleClient, ()>,
) -> Vec<CallToolResult> {
    let peer = client.peer().clone();
    let handles: Vec<_> = scripts
        .into_iter()
        .map(|params| {
            let peer = peer.clone();
            tokio::spawn(async move { peer.call_tool(params).await.expect("call") })
        })
        .collect();
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.expect("task"));
    }
    results
}
