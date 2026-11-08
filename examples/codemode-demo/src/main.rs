//! A runnable codemode demo: an upstream MCP server with two tools whose
//! outputs pipe together, served as one `codemode` tool, then driven by a
//! real host client so you can see exactly what the model sees.

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer, ServiceExt},
};
use rmcp_codemode::{
    Space,
    upstream::{UpstreamOptions, space_from_peer},
};
use rmcp_codemode_quickjs::{
    QuickJsEngine,
    server::{CodemodeServer, NativeTool},
};
use serde_json::json;

/// A tiny upstream MCP server with two weather tools.
#[derive(Clone)]
struct WeatherServer;

impl ServerHandler for WeatherServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(Default::default())
            .with_instructions("Weather lookups; temperatures are Celsius.")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: vec![
                Tool::new(
                    "list_cities",
                    "List the cities with weather data",
                    json!({ "type": "object", "properties": {} })
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
                Tool::new(
                    "get_temperature",
                    "Current temperature of one city",
                    json!({
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"]
                    })
                    .as_object()
                    .cloned()
                    .unwrap(),
                ),
            ],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let result = match request.name.as_ref() {
            "list_cities" => json!(["berlin", "paris", "tokyo"]),
            "get_temperature" => {
                let city = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("city"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let celsius = match city {
                    "berlin" => 4,
                    "paris" => 11,
                    "tokyo" => 18,
                    _ => 15,
                };
                json!({ "city": city, "celsius": celsius })
            }
            other => json!({ "error": format!("unknown tool {other}") }),
        };
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(result.to_string()),
        ])))
    }
}

/// Connect the weather server as an upstream: one call ingests its tools
/// into the space and attaches its handshake instructions.
async fn connect_weather() -> Result<Space, Box<dyn std::error::Error>> {
    let (client_transport, server_transport) = tokio::io::duplex(16 * 1024);
    let task = tokio::spawn(async move { WeatherServer.serve(server_transport).await });
    let client = ().serve(client_transport).await?;
    let peer = client.peer().clone();

    // One call: tools become weather.*, instructions attach from the
    // handshake, exposure defaults to CodeModeOnly.
    let space = space_from_peer(
        &peer,
        UpstreamOptions::new("weather").with_description("Weather lookups (Celsius)"),
    )
    .await?;

    // Tool calls hold peer clones, but the connection's two sides must
    // outlive the process for this in-process demo, so leak the keepalives.
    std::mem::forget(client);
    std::mem::forget(task);
    Ok(space)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. The upstream's tools enter the space in one call; the server's
    //    handshake instructions become the namespace guidance.
    let space = connect_weather().await?;

    // 2. A host-native tool joins the same way.
    let native = NativeTool::new("shout", "Uppercase a message", |args| {
        Box::pin(async move {
            Ok(json!({ "shouted": args["message"].as_str().unwrap_or("").to_uppercase() }))
        })
    });

    // 3. Serve everything as one `codemode` tool and drive it with a real
    //    host client, printing what the model would see.
    let server =
        CodemodeServer::new(QuickJsEngine::default(), space).with_native_tools(vec![native]);
    let (client_transport, server_transport) = tokio::io::duplex(64 * 1024);
    let _server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let tools = client.list_all_tools().await?;
    println!("exposing {} tool(s):", tools.len());
    for tool in &tools {
        println!(
            "  {} ({} chars of description)",
            tool.name,
            tool.description.as_deref().map_or(0, str::len)
        );
    }

    let script = r#"
        const cities = await tools.weather.list_cities({});
        const readings = await Promise.all(
            cities.map((city) => tools.weather.get_temperature({ city }))
        );
        const coldest = readings.reduce((a, b) => (b.celsius < a.celsius ? b : a));
        const shout = await tools.shout({ message: `coldest: ${coldest.city}` });
        text(`${coldest.celsius}C in ${coldest.city}`);
        return { coldest: coldest.city, cities: cities.length, shout };
    "#;
    let result = client
        .call_tool(
            CallToolRequestParams::new("codemode")
                .with_arguments(json!({ "script": script }).as_object().cloned().unwrap()),
        )
        .await?;

    println!("--- the model sees ---");
    for block in &result.content {
        if let Some(text) = block.as_text() {
            println!("{}", text.text);
        }
    }
    println!("isError: {}", result.is_error.unwrap_or(false));
    Ok(())
}
