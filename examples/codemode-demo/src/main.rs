//! A runnable codemode demo: an upstream MCP server with two tools whose
//! outputs pipe together, served as one `codemode` tool, then driven by a
//! real host client so you can see exactly what the model sees.

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolRequestParams as Params, CallToolResponse, CallToolResult,
        ContentBlock, ListToolsResult, PaginatedRequestParams, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer, ServiceExt},
};
use rmcp_codemode::{Space, ToolHandle};
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

/// Connect the weather server as an upstream and return its tool handles.
async fn weather_upstream() -> Result<Vec<ToolHandle>, Box<dyn std::error::Error>> {
    let weather = Arc::new(WeatherServer);
    let (client_transport, server_transport) = tokio::io::duplex(16 * 1024);
    let task = tokio::spawn(async move { weather.serve(server_transport).await });
    let client = ().serve(client_transport).await?;
    let peer = client.peer().clone();
    // Keep both connection sides alive for the process lifetime.
    tokio::spawn(async move {
        let _ = client.waiting().await;
        let _ = task.await;
    });

    let mut tools = Vec::new();
    for tool in peer.list_all_tools().await? {
        let peer = peer.clone();
        let name = tool.name.to_string();
        tools.push(ToolHandle::new(
            format!("weather.{name}")
                .split('.')
                .map(str::to_owned)
                .collect(),
            tool.description.clone().unwrap_or_default(),
            Arc::new(move |args| {
                let peer = peer.clone();
                let name = name.clone();
                Box::pin(async move {
                    let result = peer
                        .call_tool(
                            Params::new(name)
                                .with_arguments(args.as_object().cloned().unwrap_or_default()),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    let text = result
                        .content
                        .first()
                        .and_then(|c| c.as_text())
                        .map(|t| t.text.clone())
                        .unwrap_or_default();
                    serde_json::from_str(&text)
                        .map_err(|error| format!("upstream returned invalid JSON: {error}"))
                })
            }),
        ));
    }
    Ok(tools)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. The upstream's tools enter the space; the server's handshake
    //    instructions become the namespace guidance.
    let mut space = Space::new();
    space.set_namespace(
        "weather".into(),
        rmcp_codemode::NamespaceMeta::with_description("Weather lookups (Celsius)"),
    );
    for tool in weather_upstream().await? {
        space.insert(tool)?;
    }

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
