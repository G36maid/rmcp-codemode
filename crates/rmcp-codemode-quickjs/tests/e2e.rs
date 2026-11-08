//! End-to-end: upstream MCP servers → aggregator → `codemode` tool → script.
//!
//! Mirrors the architecture from rust-sdk#1437: a host sees exactly one tool;
//! upstream tools run inside the sandbox and their results pipe between tools
//! without round-tripping through the model.

use std::{sync::Arc, time::Duration};

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, ListToolsResult, PaginatedRequestParams,
        ServerConfig, Tool,
    },
    service::{RequestContext, RoleClient, RoleServer, ServiceExt},
};
use rmcp_codemode::{
    aggregate::{Aggregator, UpstreamConfig, UpstreamHandle, UpstreamTransport},
    exposure::ToolExposure,
};
use rmcp_codemode_quickjs::server::{CodemodeServer, NativeTool};
use serde_json::json;

/// A small upstream server with two tools whose outputs pipe together.
#[derive(Clone)]
struct WeatherServer;

impl ServerHandler for WeatherServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(Default::default()).with_instructions("weather")
    }
    async fn list_tools(
        &self,
        _r: Option<PaginatedRequestParams>,
        _c: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: vec![
                Tool::new(
                    "list_cities",
                    "List monitored cities",
                    json!({"type": "object"})
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                ),
                Tool::new(
                    "get_temperature",
                    "Temperature for one city",
                    json!({
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"]
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ),
            ],
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _c: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let payload = match request.name.as_ref() {
            "list_cities" => json!(["austin", "tokyo", "berlin"]),
            "get_temperature" => {
                let city = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("city"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let celsius = match city {
                    "berlin" => 4,
                    "tokyo" => 12,
                    _ => 28,
                };
                json!({"city": city, "celsius": celsius})
            }
            other => {
                return Ok(CallToolResponse::Complete(
                    rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(
                        format!("unknown tool {other}"),
                    )]),
                ));
            }
        };
        Ok(CallToolResponse::Complete(
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                payload.to_string(),
            )]),
        ))
    }
}

fn upstream(
    namespace: &str,
    server: impl ServerHandler + Clone,
) -> UpstreamConfig<impl UpstreamTransport> {
    UpstreamConfig {
        namespace: namespace.to_owned(),
        exposure: ToolExposure::CodeModeOnly,
        description: Some(format!("{namespace} tools")),
        naming: Default::default(),
        transport: move || {
            let server = server.clone();
            async move {
                let (client_transport, server_transport) = tokio::io::duplex(8192);
                let task = tokio::spawn(async move { server.serve(server_transport).await });
                let client = ().serve(client_transport).await?;
                let peer = client.peer().clone();
                // Keep both connection sides alive inside the keepalive.
                let keepalive = Box::pin(async move {
                    let _ = client.waiting().await;
                    let _ = task.await;
                });
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(UpstreamHandle::new(
                    peer, keepalive,
                ))
            }
        },
    }
}

/// A real host: an MCP client connected to the codemode server over an
/// in-process duplex transport.
///
/// Both halves return into the test frame so the connection stays open until
/// the test ends: the server task's JoinHandle owns the server
/// `RunningService`, and dropping it after the handshake would cancel the
/// server (see `UpstreamHandle` docs for this exact pitfall).
async fn host_client(
    server: CodemodeServer,
) -> (
    rmcp::service::RunningService<RoleClient, ()>,
    tokio::task::JoinHandle<
        Result<
            rmcp::service::RunningService<RoleServer, CodemodeServer>,
            rmcp::service::ServerInitializeError,
        >,
    >,
) {
    let (client_transport, server_transport) = tokio::io::duplex(16 * 1024);
    let task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await.expect("client init");
    (client, task)
}

#[tokio::test(flavor = "current_thread")]
async fn full_codemode_pipeline() {
    // 1. Aggregate the upstream server (its tools are codemode-only).
    let aggregator = Arc::new(
        Aggregator::builder()
            .upstream(upstream("weather", WeatherServer))
            .connect()
            .await
            .unwrap(),
    );

    // Codemode-only tools are invisible to a host's tools/list...
    assert_eq!(aggregator.registry().listed_tools().count(), 0);
    // ...but present in the registry for the sandbox.
    assert_eq!(aggregator.registry().len(), 2);

    // 2. Serve the codemode tool over the aggregator.
    let server = CodemodeServer::from_aggregator(Arc::clone(&aggregator))
        .with_timeout(Duration::from_secs(10));

    // 3. The host's tools/list sees exactly one tool.
    let (client, _server_task) = host_client(server).await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_ref(), "codemode");

    // 4. The model writes a script that pipes the two tools together.
    let script = r#"
        const cities = await tools.weather.list_cities({});
        const readings = await Promise.all(
            cities.map((city) => tools.weather.get_temperature({ city }))
        );
        const coldest = readings.reduce((a, b) => (b.celsius < a.celsius ? b : a));
        text(`coldest: ${coldest.city} at ${coldest.celsius}C`);
        return { coldest: coldest.city };
    "#;
    let result = client
        .call_tool(
            CallToolRequestParams::new("codemode")
                .with_arguments(json!({"script": script}).as_object().cloned().unwrap()),
        )
        .await
        .unwrap();

    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("coldest: berlin at 4C"), "got: {text}");
    assert!(text.contains("\"coldest\":\"berlin\""), "got: {text}");
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_script_tool_fails_with_registry_hint() {
    let aggregator = Arc::new(
        Aggregator::builder()
            .upstream(upstream("weather", WeatherServer))
            .connect()
            .await
            .unwrap(),
    );
    let server = CodemodeServer::from_aggregator(aggregator);
    let (client, _server_task) = host_client(server).await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("codemode").with_arguments(
                json!({"script": "await tools.weather.list_city({});"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.as_str())
        .collect();
    assert!(result.is_error.unwrap_or(false));
    assert!(
        text.contains("Unknown tool 'weather.list_city'"),
        "got: {text}"
    );
    assert!(text.contains("weather.list_cities"), "got: {text}");
}

/// The host-native-tool pattern: a host like Zed has built-in tools (read,
/// edit, bash, ...) that are not MCP. They enter the codemode sandbox the
/// same way MCP tools do — as a path, a description, and a JSON dispatcher —
/// so a script can pipe them with upstream MCP tools (pi: every session tool
/// is a method of `tools`, native or not).
#[tokio::test(flavor = "current_thread")]
async fn native_host_tools_pipe_with_upstream_mcp_tools() {
    // The host's own `bash`-like tool (not MCP, plain Rust) — defined with
    // the parsed-JSON NativeTool API.
    let native_bash = NativeTool::new("bash", "Run a shell command", |args| async move {
        let command = args["command"].as_str().ok_or("missing command")?;
        // A stand-in for the real executor; shape like pi's bash:
        // { output, exit_code }.
        Ok(json!({
            "output": if command == "echo hello" { "hello" } else { "unknown" },
            "exit_code": 0,
        }))
    })
    .with_input_schema(json!({
        "type": "object",
        "properties": { "command": { "type": "string" } },
        "required": ["command"]
    }));

    // Aggregate an MCP upstream as usual, then mix in the native tools.
    let aggregator = Arc::new(
        Aggregator::builder()
            .upstream(upstream("weather", WeatherServer))
            .connect()
            .await
            .unwrap(),
    );
    let server = CodemodeServer::from_aggregator(aggregator)
        .with_native_tools(vec![native_bash])
        .with_timeout(Duration::from_secs(10));

    let (client, _task) = host_client(server).await;
    let script = r#"
        // Native tool inside the script, piped with the MCP upstream tool:
        const who = await tools.bash({ command: "echo hello" });
        const cities = await tools.weather.list_cities({});
        const readings = await Promise.all(
            cities.map((city) => tools.weather.get_temperature({ city }))
        );
        return { shell: who.output, exitCode: who.exit_code, cities: cities.length, coldest: readings.reduce((a, b) => (b.celsius < a.celsius ? b : a)).city };
    "#;
    let result = client
        .call_tool(
            CallToolRequestParams::new("codemode")
                .with_arguments(json!({ "script": script }).as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false), "script failed");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains(r#""shell":"hello""#), "{text}");
    assert!(text.contains("\"exitCode\":0"), "{text}");
    assert!(text.contains("coldest"), "{text}");

    // Native tools participate in discovery too.
    let result = client
        .call_tool(
            CallToolRequestParams::new("codemode").with_arguments(
                json!({ "script": "return (await searchTools(\"shell\")).items.map((h) => h.name);" })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("bash"), "{text}");
}
