//! Exposure mixing at the server boundary: how `ToolExposure` states shape
//! `tools/list`, native call routing, and the `codemode` description.

use std::sync::Arc;

use rmcp::{
    model::{CallToolRequestParams, CallToolResult},
    service::{RoleClient, RoleServer, RunningService, ServerInitializeError, ServiceExt},
};
use rmcp_codemode::exposure::ToolExposure;
use rmcp_codemode_quickjs::server::{CodemodeMode, CodemodeServer, UpstreamTool};
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
