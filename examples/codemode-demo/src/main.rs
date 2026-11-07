//! Demo: aggregate two in-process MCP servers and drive them through the
//! `codemode` tool with a script that pipes their outputs together.
//!
//! Run with: cargo run -p codemode-demo

use std::{sync::Arc, time::Duration};

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, ListToolsResult, PaginatedRequestParams,
        ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer, ServiceExt},
};
use rmcp_codemode::{
    aggregate::{Aggregator, UpstreamConfig, UpstreamHandle, UpstreamTransport},
    exposure::ToolExposure,
};
use rmcp_codemode_quickjs::server::CodemodeServer;
use serde_json::json;

/// One fake upstream: a weather server with two tools.
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
            other => json!({"error": format!("unknown tool {other}")}),
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
        transport: move || {
            let server = server.clone();
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
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Aggregate the upstream(s). CodeModeOnly: hidden from any tools/list.
    let aggregator = Arc::new(
        Aggregator::builder()
            .upstream(upstream("weather", WeatherServer))
            .connect()
            .await?,
    );

    println!(
        "aggregated {} tools from {:?}",
        aggregator.registry().len(),
        aggregator.namespaces().collect::<Vec<_>>(),
    );
    println!(
        "host-visible tools/list: {} (codemode-only tools are hidden)",
        aggregator.registry().listed_tools().count(),
    );

    // Serve as one codemode tool.
    let server = CodemodeServer::from_aggregator(Arc::clone(&aggregator))
        .with_timeout(Duration::from_secs(10));
    let definition = server.tool_definition();
    println!(
        "exposing 1 tool: {} ({})",
        definition.name,
        definition
            .description
            .as_deref()
            .unwrap_or("")
            .split('.')
            .next()
            .unwrap_or(""),
    );

    // What the model would write: fan out over cities, reduce, return one line.
    let script = r#"
        const cities = await tools.weather.list_cities({});
        const readings = await Promise.all(
            cities.map((city) => tools.weather.get_temperature({ city }))
        );
        const coldest = readings.reduce((a, b) => (b.celsius < a.celsius ? b : a));
        text(`coldest: ${coldest.city} at ${coldest.celsius}C`);
        return { coldest: coldest.city, cities: cities.length };
    "#;

    let result = server.run_script(script).await?;
    for block in &result.content {
        if let Some(text) = block.as_text() {
            println!("model sees: {}", text.text);
        }
    }
    println!("isError: {}", result.is_error.unwrap_or(false));
    Ok(())
}
