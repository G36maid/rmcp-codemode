//! Exposure mixing at the server boundary: how `ToolExposure` states shape
//! `tools/list`, native call routing, and the `codemode` description.

use std::sync::Arc;

use rmcp::{
    model::{CallToolRequestParams, CallToolResult},
    service::{RoleClient, RoleServer, RunningService, ServerInitializeError, ServiceExt},
};
use rmcp_codemode::exposure::ToolExposure;
use rmcp_codemode_quickjs::server::{
    CodemodeMode, CodemodeServer, FileStorePersister, UpstreamTool,
};
use serde_json::{Value, json};

/// A tool that echoes its arguments back as `{"echo": <args>}`.
fn echo_tool(path: &str, exposure: ToolExposure) -> UpstreamTool {
    UpstreamTool {
        path: path.to_owned(),
        description: Some(format!("Echo tool at {path}")),
        input_schema: Some(json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        })),
        exposure,
        execute: Arc::new(move |args_json: String| {
            Box::pin(async move {
                serde_json::to_string(&json!({ "echo": args_json })).map_err(|e| e.to_string())
            })
        }),
    }
}

/// A tool that resolves after `delay_ms` (for worker-pool timing tests).
fn slow_tool(path: &str, exposure: ToolExposure, delay_ms: u64) -> UpstreamTool {
    UpstreamTool {
        path: path.to_owned(),
        description: Some("Resolves after a delay".to_owned()),
        input_schema: None,
        exposure,
        execute: Arc::new(move |_args_json: String| {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                Ok("\"late\"".to_owned())
            })
        }),
    }
}

/// A real host: an MCP client connected to the codemode server over an
/// in-process duplex transport. Both halves return into the test frame so the
/// connection stays open until the test ends (dropping the server task's
/// JoinHandle after the handshake would cancel the server).
async fn host_client(
    server: CodemodeServer,
) -> (
    RunningService<RoleClient, ()>,
    tokio::task::JoinHandle<
        Result<RunningService<RoleServer, CodemodeServer>, ServerInitializeError>,
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

/// The result value of a single-text-item codemode call.
fn result_value(result: CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("text content")
        .text
        .clone();
    let value_line = text
        .lines()
        .rfind(|line| !line.starts_with("==>") && !line.starts_with("Script "))
        .unwrap_or("");
    serde_json::from_str(value_line).expect("JSON value line")
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
async fn on_mode_lists_direct_tools_alongside_codemode() {
    let server = CodemodeServer::new(vec![
        echo_tool("weather.get", ToolExposure::Direct),
        echo_tool("github.list_issues", ToolExposure::Deferred),
        echo_tool("github.create_issue", ToolExposure::CodeModeOnly),
    ]);
    let (client, _task) = host_client(server).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    // Direct tools join the codemode tool; deferred and codemode-only do not.
    assert_eq!(names, ["codemode", "weather.get"]);
}

#[tokio::test(flavor = "current_thread")]
async fn only_mode_hides_direct_tools_behind_codemode() {
    let server = CodemodeServer::new(vec![echo_tool("weather.get", ToolExposure::Direct)])
        .with_mode(CodemodeMode::Only);
    let (client, _task) = host_client(server).await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_ref(), "codemode");
}

#[tokio::test(flavor = "current_thread")]
async fn direct_tools_are_callable_natively() {
    let server = CodemodeServer::new(vec![echo_tool("weather.get", ToolExposure::Direct)]);
    let (client, _task) = host_client(server).await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("weather.get")
                .with_arguments(json!({ "value": "hello" }).as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    let text = result_text(&result);
    assert!(text.contains(r#"{\"value\":\"hello\"}"#), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_tools_answer_native_calls_with_guidance() {
    // The self-correction shape from Zed discussion #64004: the error tells
    // the model exactly how to reach the tool.
    let server = CodemodeServer::new(vec![
        echo_tool("github.list_issues", ToolExposure::Deferred),
        echo_tool("secret.render", ToolExposure::CodeModeOnly),
    ]);
    let (client, _task) = host_client(server).await;
    for name in ["github.list_issues", "secret.render"] {
        let result = client
            .call_tool(CallToolRequestParams::new(name))
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(
            text.contains("codemode script instead") && text.contains("searchTools"),
            "{name}: {text}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn codemode_description_declares_direct_tools_inline() {
    let server = CodemodeServer::new(vec![
        echo_tool("weather.get", ToolExposure::Direct),
        echo_tool("weather.set", ToolExposure::Direct),
        echo_tool("github.list_issues", ToolExposure::Deferred),
    ]);
    let (client, _task) = host_client(server).await;
    let tools = client.list_all_tools().await.unwrap();
    let codemode = tools
        .iter()
        .find(|t| t.name.as_ref() == "codemode")
        .unwrap();
    let description = codemode.description.as_deref().unwrap();
    assert!(description.contains("## Tools"), "{description}");
    assert!(description.contains("### weather"), "{description}");
    assert!(
        description.contains("declare function get("),
        "{description}"
    );
    assert!(
        description.contains("declare function set("),
        "{description}"
    );
    // Deferred tools are not listed (pi's semantics) but remain callable.
    assert!(!description.contains("list_issues"), "{description}");
}

#[tokio::test(flavor = "current_thread")]
async fn inline_declarations_respect_the_budget() {
    let mut tools = Vec::new();
    for i in 0..200 {
        tools.push(echo_tool(&format!("big.tool_{i}"), ToolExposure::Direct));
    }
    let server = CodemodeServer::new(tools);
    let (client, _task) = host_client(server).await;
    let tools = client.list_all_tools().await.unwrap();
    let codemode = tools
        .iter()
        .find(|t| t.name.as_ref() == "codemode")
        .unwrap();
    let description = codemode.description.as_deref().unwrap();
    // pi's inlineBudget: some declarations fit, the rest are named in a note.
    assert!(
        description.contains("exceed the inline declaration budget"),
        "{description}"
    );
    assert!(!description.contains("tool_199"), "{description}");
}

#[tokio::test(flavor = "current_thread")]
async fn sandbox_sees_every_tool_regardless_of_exposure() {
    // ALL_TOOLS includes deferred and codemode-only tools (pi: "including
    // tools the description does not list"), and scripts can call them.
    let server = CodemodeServer::new(vec![
        echo_tool("weather.get", ToolExposure::Direct),
        echo_tool("github.list_issues", ToolExposure::Deferred),
        echo_tool("github.create_issue", ToolExposure::CodeModeOnly),
    ]);
    let (client, _task) = host_client(server).await;
    let value = result_value(
        client
            .call_tool(script_args(
                r#"
                const names = ALL_TOOLS.map((t) => t.name);
                const viaSearch = (await searchTools("issues")).map((h) => h.name);
                const called = await tools.github.create_issue({ value: "x" });
                return { names, viaSearch, called };
                "#,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(value["names"].as_array().unwrap().len(), 3);
    assert!(
        value["viaSearch"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "github.list_issues")
    );
    assert!(value["called"]["echo"].as_str().unwrap().contains("value"));
}

#[tokio::test(flavor = "current_thread")]
async fn store_persists_across_codemode_calls_on_one_server() {
    let server = CodemodeServer::new(vec![echo_tool("echo", ToolExposure::CodeModeOnly)]);
    let (client, _task) = host_client(server).await;
    let first = client
        .call_tool(script_args("store(\"k\", { runs: 1 }); return \"stored\";"))
        .await
        .unwrap();
    assert!(
        result_text(&first).contains("Script completed"),
        "{}",
        result_text(&first)
    );
    let second = client
        .call_tool(script_args("return load(\"k\");"))
        .await
        .unwrap();
    assert!(
        result_text(&second).contains(r#""runs":1"#),
        "{}",
        result_text(&second)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn image_output_becomes_an_mcp_image_block() {
    let server = CodemodeServer::new(vec![]);
    let (client, _task) = host_client(server).await;
    let response = client
        .call_tool(script_args(
            "image(\"data:image/png;base64,aGVsbG8=\"); return \"done\";",
        ))
        .await
        .unwrap();
    let image = response
        .content
        .iter()
        .find_map(|c| c.as_image())
        .expect("image content block");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(image.data, "aGVsbG8=");
}

// ---- worker pool (pi: at most N workers, FIFO queue, bounded wait) ----

#[tokio::test(flavor = "current_thread")]
async fn workers_run_scripts_in_parallel_up_to_the_limit() {
    let server = CodemodeServer::new(vec![echo_tool("echo", ToolExposure::CodeModeOnly)])
        .with_max_workers(2);
    // Two scripts at once are fine with 2 workers.
    let (a, b) = tokio::join!(
        server.run_script("return \"a\";"),
        server.run_script("return \"b\";"),
    );
    assert!(a.unwrap().content.iter().any(|c| c.as_text().is_some()));
    assert!(b.unwrap().content.iter().any(|c| c.as_text().is_some()));
}

#[tokio::test(flavor = "current_thread")]
async fn scripts_beyond_the_worker_limit_wait_in_arrival_order() {
    use std::time::Duration;
    let server = CodemodeServer::new(vec![slow_tool("slow", ToolExposure::CodeModeOnly, 200)])
        .with_max_workers(1)
        .with_timeout(Duration::from_millis(500));
    // First script occupies the only worker for ~200ms; the second queues
    // behind it and still runs (within its 200ms wait cap).
    let (a, b) = tokio::join!(
        server.run_script("return await tools.slow({});"),
        server.run_script("return \"queued and ran\";"),
    );
    assert!(a.unwrap().content.iter().any(|c| c.as_text().is_some()));
    let b = b.unwrap();
    assert!(
        b.content.iter().any(|c| c
            .as_text()
            .is_some_and(|t| t.text.contains("queued and ran"))),
        "second script should run after the first frees the worker"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_script_that_gets_no_worker_in_time_fails_without_running() {
    use std::time::Duration;
    let server = std::sync::Arc::new(
        CodemodeServer::new(vec![slow_tool("slow", ToolExposure::CodeModeOnly, 300)])
            .with_max_workers(1)
            .with_worker_wait(Duration::from_millis(100)),
    );
    // Occupy the worker for 300ms; the queued script's 100ms wait expires.
    let held = tokio::spawn({
        let server = std::sync::Arc::clone(&server);
        async move { server.run_script("return await tools.slow({});").await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let result = server.run_script("return \"never\";").await.unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(text.contains("no codemode worker available"), "{text}");
    let _ = held.await.unwrap().unwrap();
}

// ---- durable store (pi keeps the store in the session file) ----

#[tokio::test(flavor = "current_thread")]
async fn store_survives_a_server_restart_through_a_persister() {
    let dir = std::env::temp_dir().join(format!(
        "codemode-store-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.json");

    let persister = std::sync::Arc::new(FileStorePersister::new(&path));
    let server = CodemodeServer::new(vec![echo_tool("echo", ToolExposure::CodeModeOnly)])
        .with_store_persistence(persister);
    let first = server
        .run_script("store(\"session\", { id: 7 }); return \"stored\";")
        .await
        .unwrap();
    assert!(first.is_error.is_none_or(|e| !e), "store run failed");
    drop(server);

    // A fresh server over the same persister sees the committed values.
    let persister = std::sync::Arc::new(FileStorePersister::new(&path));
    let server = CodemodeServer::new(vec![]).with_store_persistence(persister);
    let second = server
        .run_script("return load(\"session\");")
        .await
        .unwrap();
    let text = second
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(text.contains("\"id\":7"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- overflow file wiring through the server ----

#[tokio::test(flavor = "current_thread")]
async fn server_overflow_goes_to_the_configured_directory() {
    let dir = std::env::temp_dir().join(format!(
        "codemode-overflow-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let server = CodemodeServer::new(vec![]).with_overflow_dir(&dir);
    let result = server
        .run_script("// @options: {\"max_output_tokens\": 100}\nreturn \"z\".repeat(3000);")
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    let marker = "Full output: ";
    let path_line = text
        .lines()
        .find(|l| l.starts_with(marker))
        .expect("overflow path");
    let path = std::path::PathBuf::from(&path_line[marker.len()..]);
    assert!(path.starts_with(&dir), "{path:?}");
    assert!(path.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- host-native (non-MCP) tools ----

#[tokio::test(flavor = "current_thread")]
async fn native_tools_join_the_sandbox_and_discovery() {
    use rmcp_codemode_quickjs::server::NativeTool;
    let server = CodemodeServer::new(vec![]).with_native_tools(vec![
        NativeTool::new("bash", "Run a shell command", |args| async move {
            Ok(json!({ "output": args["command"].as_str().unwrap_or("").to_uppercase() }))
        })
        .with_input_schema(json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })),
        // No-argument native tool: `tools.ping()` with null args.
        NativeTool::new(
            "ping",
            "Ping the host",
            |_args| async move { Ok(json!("pong")) },
        ),
    ]);
    let (client, _task) = host_client(server).await;
    let value = result_value(
        client
            .call_tool(script_args(
                r#"
                const names = ALL_TOOLS.map((t) => t.name);
                const shell = await tools.bash({ command: "hi" });
                const ping = await tools.ping();
                const decl = (await describeTool("bash")).declaration;
                return { names, shell, ping, decl };
                "#,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(value["names"], json!(["bash", "ping"]));
    assert_eq!(value["shell"]["output"], "HI");
    assert_eq!(value["ping"], "pong");
    // The schema renders into the TypeScript declaration.
    assert!(
        value["decl"].as_str().unwrap().contains("command: string;"),
        "{}",
        value["decl"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_tools_default_to_codemode_only_but_can_be_direct() {
    use rmcp_codemode_quickjs::server::NativeTool;
    // Defaults: codemode-only, so tools/list shows just `codemode` — a
    // host's built-ins are usually already native tools of their own.
    let default_server = CodemodeServer::new(vec![]).with_native_tools(vec![NativeTool::new(
        "bash",
        "Run a shell command",
        |_args| async move { Ok(json!({})) },
    )]);
    let (client, _task) = host_client(default_server).await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_ref(), "codemode");

    // Opting into Direct puts it on the host's list and in the inline
    // declarations.
    let server = CodemodeServer::new(vec![]).with_native_tools(vec![
        NativeTool::new("bash", "Run a shell command", |_args| async move {
            Ok(json!({}))
        })
        .with_exposure(ToolExposure::Direct),
    ]);
    let (client, _task) = host_client(server).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["codemode", "bash"]);
    let description = tools[0].description.as_deref().unwrap();
    assert!(
        description.contains("declare function bash("),
        "{description}"
    );
}

// ---- naming conventions: vendors spell tool paths differently ----

#[tokio::test(flavor = "current_thread")]
async fn claude_style_flat_paths_work_end_to_end() {
    use rmcp::service::ServiceExt;
    use rmcp_codemode::{
        aggregate::{Aggregator, UpstreamConfig, UpstreamHandle},
        naming::NamingConvention,
    };

    // A Claude Code-style host: flat `mcp__<server>__<tool>` paths.
    struct FlatServer;
    impl rmcp::handler::server::ServerHandler for FlatServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            rmcp::model::ServerConfig::new(Default::default())
        }
        async fn list_tools(
            &self,
            _: Option<rmcp::model::PaginatedRequestParams>,
            _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
            Ok(rmcp::model::ListToolsResult {
                tools: vec![rmcp::model::Tool::new(
                    "create_issue",
                    "Create an issue",
                    json!({ "type": "object", "properties": {} })
                        .as_object()
                        .cloned()
                        .unwrap(),
                )],
                ..Default::default()
            })
        }
        async fn call_tool(
            &self,
            request: rmcp::model::CallToolRequestParams,
            _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
            Ok(rmcp::model::CallToolResponse::Complete(
                rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    serde_json::json!({ "created": request.name }).to_string(),
                )]),
            ))
        }
    }

    let server = Arc::new(FlatServer);
    let aggregator = Aggregator::builder()
        .upstream(UpstreamConfig {
            namespace: "github".into(),
            exposure: ToolExposure::CodeModeOnly,
            description: None,
            naming: NamingConvention::McpPrefixed,
            transport: move || {
                let server = Arc::clone(&server);
                async move {
                    let (client_transport, server_transport) = tokio::io::duplex(8192);
                    let task = tokio::spawn(async move { server.serve(server_transport).await });
                    let client = ().serve(client_transport).await?;
                    let peer = client.peer().clone();
                    let keepalive = Box::pin(async move {
                        let _ = client.waiting().await;
                        let _ = task.await;
                    });
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(UpstreamHandle::new(
                        peer, keepalive,
                    ))
                }
            },
        })
        .connect()
        .await
        .unwrap();

    // The registry path is the flat Claude-style spelling.
    let paths: Vec<&str> = aggregator
        .registry()
        .entries()
        .map(|e| e.path.as_str())
        .collect();
    assert_eq!(paths, ["mcp__github__create_issue"]);

    let codemode = CodemodeServer::from_aggregator(Arc::new(aggregator));
    let (client, _task) = host_client(codemode).await;
    let response = client
        .call_tool(script_args(
            r#"
            return {
                names: ALL_TOOLS.map((t) => t.name),
                topSegments: Object.keys(tools),
                called: await tools.mcp__github__create_issue({ title: "x" }),
            };
            "#,
        ))
        .await
        .unwrap();
    let value = result_value(response);
    // Flat paths present flat: one top-level segment, the full spelling.
    assert_eq!(value["names"], json!(["mcp__github__create_issue"]));
    assert_eq!(value["topSegments"], json!(["mcp__github__create_issue"]));
    // Routing strips the spelling: the upstream sees its own tool name.
    assert_eq!(value["called"]["created"], "create_issue");
}
