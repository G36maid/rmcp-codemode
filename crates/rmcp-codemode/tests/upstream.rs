//! Upstream ingestion tests: one connected rmcp peer becomes a namespace
//! of script-callable tools, with its handshake instructions attached.

use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer, ServiceExt},
};
use rmcp_codemode::{
    Space, ToolFnFut,
    space::Exposure,
    upstream::{UpstreamOptions, upstream_into_space},
};
use serde_json::json;

/// A tiny upstream with two tools and handshake instructions.
#[derive(Clone)]
struct TestUpstream;

impl ServerHandler for TestUpstream {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(Default::default())
            .with_instructions("Weather lookups; temperatures are Celsius.")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: vec![
                Tool::new(
                    "list_cities",
                    "List cities",
                    json!({ "type": "object", "properties": {} })
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
                Tool::new(
                    "get_temperature",
                    "Temperature of one city",
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
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let city = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("city"))
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if request.name.as_ref() == "boom" || city == "boom" {
            return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text("city not found".to_owned()),
            ])));
        }
        // Structured content exercises the unwrap order.
        let result = CallToolResult::success(vec![ContentBlock::text("ignored")]);
        let mut result = result;
        result.structured_content = Some(json!({ "celsius": 4, "city": city }));
        Ok(CallToolResponse::Complete(result))
    }
}

/// The peer plus the connection's keepalives: dropping the client service
/// or the server task's handle tears the transport down, so callers must
/// hold both for the connection's lifetime.
async fn connected_upstream() -> (
    rmcp::service::Peer<rmcp::service::RoleClient>,
    rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    tokio::task::JoinHandle<
        Result<
            rmcp::service::RunningService<RoleServer, TestUpstream>,
            rmcp::service::ServerInitializeError,
        >,
    >,
) {
    let (client_transport, server_transport) = tokio::io::duplex(16 * 1024);
    let task = tokio::spawn(async move { TestUpstream.serve(server_transport).await });
    let client = ().serve(client_transport).await.expect("client init");
    let peer = client.peer().clone();
    (peer, client, task)
}

#[tokio::test(flavor = "current_thread")]
async fn upstream_tools_enter_as_namespace_and_instructions_attach() {
    let (peer, _client, _task) = connected_upstream().await;
    let mut space = Space::new();
    let ingested = upstream_into_space(
        &mut space,
        &peer,
        UpstreamOptions::new("weather").with_description("Weather tools"),
    )
    .await
    .expect("ingest");

    assert_eq!(ingested.tools, 2);
    assert!(ingested.instructions, "handshake instructions attach");
    assert_eq!(
        space
            .namespace("weather")
            .and_then(|meta| meta.instructions.clone())
            .as_deref(),
        Some("Weather lookups; temperatures are Celsius."),
    );
    assert_eq!(
        space
            .namespace("weather")
            .and_then(|meta| meta.description.clone())
            .as_deref(),
        Some("Weather tools"),
    );
    assert_eq!(
        space
            .entry("weather.list_cities")
            .map(|t| t.description.clone())
            .unwrap()
            .as_deref(),
        Some("List cities"),
    );
    // Default exposure is CodeModeOnly: schemas stay out of the context.
    assert_eq!(
        space.entry("weather.list_cities").map(|t| t.exposure),
        Some(Exposure::CodeModeOnly),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn upstream_calls_unwrap_structured_content_and_error_text() {
    let (peer, _client, _task) = connected_upstream().await;
    let mut space = Space::new();
    upstream_into_space(&mut space, &peer, UpstreamOptions::new("weather"))
        .await
        .expect("ingest");

    // Structured content wins over the text block.
    let value =
        (space.entry("weather.get_temperature").unwrap().execute)(json!({ "city": "berlin" }))
            .await
            .expect("call");
    assert_eq!(value, json!({ "celsius": 4, "city": "berlin" }));

    // An isError upstream result becomes a script-visible Err with the
    // upstream's text.
    let error =
        (space.entry("weather.get_temperature").unwrap().execute)(json!({ "city": "boom" }))
            .await
            .expect_err("error surfaces");
    assert_eq!(error, "city not found");
}

#[tokio::test(flavor = "current_thread")]
async fn exposure_override_applies_to_every_upstream_tool() {
    let (peer, _client, _task) = connected_upstream().await;
    let mut space = Space::new();
    upstream_into_space(
        &mut space,
        &peer,
        UpstreamOptions::new("weather").with_exposure(Exposure::Direct),
    )
    .await
    .expect("ingest");
    assert_eq!(
        space.entry("weather.list_cities").map(|t| t.exposure),
        Some(Exposure::Direct),
    );
    assert_eq!(
        space.entry("weather.get_temperature").map(|t| t.exposure),
        Some(Exposure::Direct),
    );
}

// Silence the unused-import lint when the closure type is inferred.
#[allow(unused)]
fn _fut_shape(_: ToolFnFut) {}
