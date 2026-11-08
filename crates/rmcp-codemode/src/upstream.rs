//! One-line upstream ingestion: build a [`Space`] from a connected rmcp
//! client peer.

use std::sync::Arc;

use rmcp::{
    model::{CallToolRequestParams, ServerPeerInfo},
    service::{Peer, RoleClient},
};

use crate::space::{NamespaceMeta, Space, ToolHandle};

/// Options for [`upstream_into_space`] / [`space_from_peer`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct UpstreamOptions {
    /// The namespace the upstream's tools enter under: `ns.tool`.
    pub namespace: String,
    /// The exposure applied to every upstream tool. Default:
    /// [`Exposure::CodeModeOnly`] — schemas stay out of the model's
    /// context; the script discovers them (Anthropic's deferred loading).
    pub exposure: crate::space::Exposure,
    /// Replace the namespace's description (upstream handshake
    /// instructions become the instructions either way).
    pub description: Option<String>,
}

impl UpstreamOptions {
    /// Options for one upstream under `namespace`. The exposure defaults
    /// to [`Exposure::CodeModeOnly`] — that is the point of code mode:
    /// upstream schemas stay out of the model's context unless a host
    /// overrides them.
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            exposure: crate::space::Exposure::CodeModeOnly,
            description: None,
        }
    }

    /// Set the exposure for the upstream's tools.
    pub fn with_exposure(mut self, exposure: crate::space::Exposure) -> Self {
        self.exposure = exposure;
        self
    }

    /// Set a namespace description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl Default for UpstreamOptions {
    fn default() -> Self {
        Self::new("upstream")
    }
}

/// The result of ingesting one upstream: how many tools were registered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ingested {
    /// Number of tools registered from the upstream.
    pub tools: usize,
    /// Whether the upstream's handshake `instructions` were attached to
    /// the namespace.
    pub instructions: bool,
}

/// Ingest one connected upstream: every tool it lists enters the space as
/// `namespace.tool`, and its handshake `instructions` become the
/// namespace's guidance (pi: the server describes itself).
pub async fn upstream_into_space(
    space: &mut Space,
    peer: &Peer<RoleClient>,
    options: UpstreamOptions,
) -> Result<Ingested, String> {
    let tools = peer
        .list_all_tools()
        .await
        .map_err(|error| format!("listing upstream tools: {error}"))?;
    for tool in &tools {
        let name = tool.name.to_string();
        let call_name = name.clone();
        let peer = peer.clone();
        let execute: crate::space::ToolFn = Arc::new(move |args| {
            let peer = peer.clone();
            let name = call_name.clone();
            Box::pin(async move {
                let result = peer
                    .call_tool(
                        CallToolRequestParams::new(name)
                            .with_arguments(args.as_object().cloned().unwrap_or_default()),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                tool_result_value(result)
            })
        });
        let mut handle = ToolHandle::new(
            vec![options.namespace.clone(), name],
            tool.description.clone().unwrap_or_default(),
            execute,
        );
        handle.exposure = options.exposure;
        space.insert(handle)?;
    }

    let instructions = peer
        .peer_info()
        .as_ref()
        .and_then(|info: &Arc<ServerPeerInfo>| info.instructions.clone());
    let mut meta = NamespaceMeta::with_description(options.description.clone().unwrap_or_default());
    let has_instructions = instructions.is_some();
    meta.instructions = instructions;
    space.set_namespace(options.namespace.clone(), meta);

    Ok(Ingested {
        tools: tools.len(),
        instructions: has_instructions,
    })
}

/// Convenience: build a fresh [`Space`] from one upstream.
pub async fn space_from_peer(
    peer: &Peer<RoleClient>,
    options: UpstreamOptions,
) -> Result<Space, String> {
    let mut space = Space::new();
    upstream_into_space(&mut space, peer, options).await?;
    Ok(space)
}

/// Extract the script-visible value from an upstream tool result
/// (Cloudflare's `unwrapMcpResult` order: structured content first, then
/// the first text block parsed as JSON, then the text itself).
fn tool_result_value(result: rmcp::model::CallToolResult) -> Result<serde_json::Value, String> {
    if result.is_error.unwrap_or(false) {
        let text = result
            .content
            .iter()
            .find_map(|block| block.as_text().map(|text| text.text.clone()))
            .unwrap_or_else(|| "upstream tool failed".to_owned());
        return Err(text);
    }
    if let Some(structured) = result.structured_content {
        return Ok(structured);
    }
    let text = result
        .content
        .iter()
        .find_map(|block| block.as_text().map(|text| text.text.clone()))
        .unwrap_or_default();
    serde_json::from_str(&text).or(Ok(serde_json::Value::String(text)))
}
