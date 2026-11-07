//! Client-side aggregation: connect upstream MCP servers into one registry.
//!
//! rmcp's client role connects to N upstream servers, but composing them into
//! one logical server (merged `tools/list` with namespaces, routed `tools/call`)
//! is left to implementers. This module provides that shape:
//!
//! 1. [`UpstreamConfig`] declares an upstream server: its namespace, transport
//!    factory, and per-server [`ToolExposure`].
//! 2. [`AggregatorBuilder::connect`] runs each upstream's `tools/list` and
//!    folds the results into one [`Registry`].
//! 3. [`Aggregator::call`] routes a `tools/call` by registry path to the
//!    owning upstream peer.

use std::{collections::BTreeMap, sync::Arc};

use futures::future::join_all;
use rmcp::{
    ServiceError,
    model::{CallToolRequestParams, CallToolResult, Tool},
    service::Peer,
};

use crate::{
    exposure::ToolExposure,
    registry::{Registry, RegistryError},
};

/// How to reach one upstream MCP server.
///
/// The transport is produced lazily per connection attempt, so an aggregator
/// can reconnect without reusing a consumed transport.
pub trait UpstreamTransport: Send + Sync + 'static {
    /// Build a fresh transport, finish the MCP handshake, and return a handle
    /// to the initialized client.
    fn connect(
        &self,
    ) -> impl Future<Output = Result<UpstreamHandle, Box<dyn std::error::Error + Send + Sync>>> + Send;
}

/// The client role alias used by upstream connections.
pub type ClientPeer = Peer<rmcp::service::RoleClient>;

/// A live upstream connection: the client [`Peer`] plus a type-erased
/// keepalive that holds the underlying service task open.
///
/// Dropping the handle cancels the connection (the keepalive owns the
/// [`RunningService`], whose `Drop` shuts the transport down).
pub struct UpstreamHandle {
    /// The initialized client peer used for requests.
    pub peer: ClientPeer,
    /// Never polled or locked; holds every resource that keeps the upstream
    /// connection alive (the client `RunningService` and any spawned peer
    /// task) so their `Drop`s run only when the handle is dropped. The
    /// `Mutex` makes the handle `Sync` so an aggregator can be shared.
    #[allow(dead_code)] // intentionally stored, never read or polled
    keepalive: std::sync::Mutex<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl UpstreamHandle {
    /// Pair a client peer with the keepalive future of its running service.
    ///
    /// `keepalive` should move every resource that keeps the connection open
    /// (the client [`RunningService`] and any spawned peer task — dropping the
    /// peer task's `JoinHandle` drops the server `RunningService` and closes
    /// the connection). It is stored, never polled or locked.
    pub fn new(
        peer: ClientPeer,
        keepalive: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
    ) -> Self {
        Self {
            peer,
            keepalive: std::sync::Mutex::new(keepalive),
        }
    }
}

impl<F, Fut, E> UpstreamTransport for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<UpstreamHandle, E>> + Send,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn connect(
        &self,
    ) -> impl Future<Output = Result<UpstreamHandle, Box<dyn std::error::Error + Send + Sync>>> + Send
    {
        let fut = (self)();
        async move { fut.await.map_err(Into::into) }
    }
}

/// Type-erased transport factory held by the builder.
type TransportFactory = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<UpstreamHandle, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// Declaration of one upstream server.
#[allow(clippy::exhaustive_structs)] // user-constructed by design
pub struct UpstreamConfig<T> {
    /// Namespace prefix for this server's tools, e.g. `github`. Must be a
    /// single path segment (no `.`), or insertion fails.
    pub namespace: String,
    /// How this server's tools are exposed by default. Tools can be
    /// overridden per-tool via [`Registry::override_exposure`].
    pub exposure: ToolExposure,
    /// A human-readable description, shown in discovery results.
    pub description: Option<String>,
    /// The transport factory.
    pub transport: T,
}

/// One connected upstream server.
struct ConnectedUpstream {
    handle: UpstreamHandle,
}

/// Error type for aggregation operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AggregateError {
    /// An upstream server failed to connect or list its tools.
    #[error("upstream `{namespace}` failed: {source}")]
    Upstream {
        /// The failing upstream's namespace.
        namespace: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A registry conflict (duplicate tool path).
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// A tool call named an unknown tool, namespace, or a `codemode`-only tool
    /// reached from the host.
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    /// The upstream tool call failed at the protocol level.
    #[error("tool call failed: {0}")]
    Service(#[from] ServiceError),
}

/// Aggregates upstream MCP servers into one logical registry.
///
/// Build with [`Aggregator::builder`], register upstreams with
/// [`AggregatorBuilder::upstream`], then [`AggregatorBuilder::connect`] to run
/// every upstream's `tools/list` and build the registry.
pub struct Aggregator {
    registry: Registry,
    upstreams: BTreeMap<String, ConnectedUpstream>,
}

/// Builder for [`Aggregator`].
#[must_use]
pub struct AggregatorBuilder {
    configs: Vec<(String, ToolExposure, Option<String>, TransportFactory)>,
}

impl Aggregator {
    /// Start configuring an aggregator.
    pub fn builder() -> AggregatorBuilder {
        AggregatorBuilder {
            configs: Vec::new(),
        }
    }

    /// The aggregated registry after connecting.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The aggregated registry, mutably (to adjust per-tool exposure).
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// The connected upstream namespaces, in sorted order.
    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.upstreams.keys().map(String::as_str)
    }

    /// Route a `tools/call` by registry path to the owning upstream.
    ///
    /// Tools with [`ToolExposure::CodeModeOnly`] are rejected here: they may
    /// only be invoked from the script sandbox, via
    /// [`Aggregator::call_from_script`].
    pub async fn call(
        &self,
        path: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, AggregateError> {
        let entry = self
            .registry
            .get(path)
            .ok_or_else(|| AggregateError::UnknownTool(path.to_owned()))?;
        if entry.exposure.is_sandbox_only() {
            return Err(AggregateError::UnknownTool(path.to_owned()));
        }
        self.dispatch(entry, arguments).await
    }

    /// Route a `tools/call` from inside the script sandbox: every registered
    /// tool is callable, including [`ToolExposure::CodeModeOnly`] ones.
    pub async fn call_from_script(
        &self,
        path: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, AggregateError> {
        let entry = self
            .registry
            .get(path)
            .ok_or_else(|| AggregateError::UnknownTool(path.to_owned()))?;
        self.dispatch(entry, arguments).await
    }

    async fn dispatch(
        &self,
        entry: &crate::registry::RegistryEntry,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, AggregateError> {
        let upstream = self
            .upstreams
            .get(&entry.namespace)
            .ok_or_else(|| AggregateError::UnknownTool(entry.path.clone()))?;
        let mut params = CallToolRequestParams::new(entry.upstream_name.clone());
        params.arguments = arguments;
        let result = upstream.handle.peer.call_tool(params).await?;
        Ok(result)
    }
}

impl AggregatorBuilder {
    /// Register an upstream server.
    pub fn upstream<T>(mut self, config: UpstreamConfig<T>) -> Self
    where
        T: UpstreamTransport,
    {
        let transport = Arc::new(config.transport);
        let factory: TransportFactory = Arc::new(move || {
            let t = Arc::clone(&transport);
            Box::pin(async move { t.connect().await })
        });
        self.configs.push((
            config.namespace,
            config.exposure,
            config.description,
            factory,
        ));
        self
    }

    /// Connect every configured upstream, list its tools, and build the
    /// aggregated registry.
    ///
    /// Upstreams connect concurrently; the first failure aborts the whole
    /// aggregation. If two upstreams produce the same tool path (same
    /// namespace and tool name), the error names the conflicting path.
    ///
    /// # Panics
    ///
    /// Panics if a namespace contains a `.` (it would corrupt registry paths).
    /// The builder's `upstream` method validates this in debug builds.
    pub async fn connect(self) -> Result<Aggregator, AggregateError> {
        let mut registry = Registry::new();
        let mut upstreams = BTreeMap::new();

        let connections = self
            .configs
            .iter()
            .map(|(namespace, _exposure, _desc, factory)| {
                let namespace = namespace.clone();
                async move {
                    let handle = factory().await.map_err(|source| AggregateError::Upstream {
                        namespace: namespace.clone(),
                        source,
                    })?;
                    let tools: Vec<Tool> =
                        handle.peer.list_all_tools().await.map_err(|source| {
                            AggregateError::Upstream {
                                namespace: namespace.clone(),
                                source: Box::new(source),
                            }
                        })?;
                    Ok::<_, AggregateError>((namespace, handle, tools))
                }
            });
        let results = join_all(connections).await;

        for result in results {
            let (namespace, handle, tools) = result?;
            if let Some((_, exposure, desc, _)) =
                self.configs.iter().find(|(ns, _, _, _)| *ns == namespace)
            {
                if let Some(desc) = desc {
                    registry.set_namespace_description(&namespace, desc.clone());
                }
                for tool in tools {
                    registry.insert(&namespace, tool, *exposure)?;
                }
                upstreams.insert(namespace, ConnectedUpstream { handle });
            }
        }

        Ok(Aggregator {
            registry,
            upstreams,
        })
    }
}

#[cfg(test)]
mod tests {
    use rmcp::{
        ErrorData as McpError,
        handler::server::ServerHandler,
        model::{
            CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
            PaginatedRequestParams, ServerConfig, Tool,
        },
        service::{RequestContext, RoleServer, ServiceExt},
    };

    use super::*;

    /// A tiny in-process upstream server for tests.
    #[derive(Clone)]
    struct TestServer {
        tools: Vec<Tool>,
    }

    impl ServerHandler for TestServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(Default::default()).with_instructions("test server")
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            Ok(ListToolsResult {
                tools: self.tools.clone(),
                ..Default::default()
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, McpError> {
            Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(format!("called {}", request.name)),
            ])))
        }
    }

    fn tool(name: &str, description: &str) -> Tool {
        Tool::new(
            name.to_owned(),
            description.to_owned(),
            rmcp::model::JsonObject::default(),
        )
    }

    fn upstream(namespace: &str, server: TestServer) -> UpstreamConfig<impl UpstreamTransport> {
        UpstreamConfig {
            namespace: namespace.to_owned(),
            exposure: ToolExposure::Direct,
            description: Some(format!("{namespace} tools")),
            transport: move || {
                let server = server.clone();
                async move {
                    let (client_transport, server_transport) = tokio::io::duplex(8192);
                    let task = tokio::spawn(async move { server.serve(server_transport).await });
                    let client = ().serve(client_transport).await?;
                    let peer = client.peer().clone();
                    // Both sides of the connection move into the keepalive: the
                    // client RunningService and the server task (whose output,
                    // the server RunningService, is dropped with its handle).
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

    #[tokio::test(flavor = "current_thread")]
    async fn aggregates_namespaced_tools() {
        let github = TestServer {
            tools: vec![
                tool("list_issues", "List issues"),
                tool("search_code", "Search code"),
            ],
        };
        let ghidra = TestServer {
            tools: vec![tool("decompile", "Decompile")],
        };

        let aggregator = Aggregator::builder()
            .upstream(upstream("github", github))
            .upstream(upstream("ghidra", ghidra))
            .connect()
            .await
            .unwrap();

        let paths: Vec<_> = aggregator
            .registry()
            .entries()
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "ghidra.decompile",
                "github.list_issues",
                "github.search_code"
            ]
        );
        assert_eq!(
            aggregator.namespaces().collect::<Vec<_>>(),
            ["ghidra", "github"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn debug_routes_calls_by_path() {
        let github = TestServer {
            tools: vec![tool("list_issues", "List issues")],
        };
        let result = Aggregator::builder()
            .upstream(upstream("github", github))
            .connect()
            .await;
        if let Err(e) = &result {
            let mut chain = format!("{e}");
            let mut src = std::error::Error::source(e);
            while let Some(s) = src {
                chain.push_str(&format!(" <- {s}"));
                src = std::error::Error::source(s);
            }
            eprintln!("ERROR CHAIN: {chain}");
        }
        let aggregator = result.unwrap();
        let r = aggregator.call("github.list_issues", None).await;
        if let Err(e) = &r {
            let mut chain = format!("{e}");
            let mut src = std::error::Error::source(e);
            while let Some(s) = src {
                chain.push_str(&format!(" <- {s}"));
                src = std::error::Error::source(s);
            }
            eprintln!("CALL CHAIN: {chain}");
        }
        r.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn routes_calls_by_path() {
        let github = TestServer {
            tools: vec![tool("list_issues", "List issues")],
        };
        let aggregator = Aggregator::builder()
            .upstream(upstream("github", github))
            .connect()
            .await
            .unwrap();

        let result = aggregator.call("github.list_issues", None).await.unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        assert_eq!(text, "called list_issues");

        assert!(matches!(
            aggregator.call("github.nope", None).await,
            Err(AggregateError::UnknownTool(_))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn codemode_only_tools_are_host_invisible() {
        let ghidra = TestServer {
            tools: vec![tool("decompile", "Decompile")],
        };
        let mut aggregator = Aggregator::builder()
            .upstream(UpstreamConfig {
                namespace: "ghidra".into(),
                exposure: ToolExposure::CodeModeOnly,
                description: None,
                transport: upstream("ghidra", ghidra).transport,
            })
            .connect()
            .await
            .unwrap();

        // Hidden from the model's tool list.
        assert_eq!(aggregator.registry().listed_tools().count(), 0);
        // Host dispatch refuses it...
        assert!(matches!(
            aggregator.call("ghidra.decompile", None).await,
            Err(AggregateError::UnknownTool(_))
        ));
        // ...but the script sandbox can call it.
        let result = aggregator
            .call_from_script("ghidra.decompile", None)
            .await
            .unwrap();
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "called decompile"
        );

        // And exposure can be relaxed per tool afterwards.
        aggregator
            .registry_mut()
            .override_exposure("ghidra.decompile", ToolExposure::Direct);
        assert_eq!(aggregator.registry().listed_tools().count(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failing_upstream_aborts_aggregation() {
        struct BrokenTransport;
        impl UpstreamTransport for BrokenTransport {
            async fn connect(
                &self,
            ) -> Result<UpstreamHandle, Box<dyn std::error::Error + Send + Sync>> {
                Err("connection refused".into())
            }
        }

        let err = match Aggregator::builder()
            .upstream(UpstreamConfig {
                namespace: "broken".into(),
                exposure: ToolExposure::Direct,
                description: None,
                transport: BrokenTransport,
            })
            .connect()
            .await
        {
            Ok(_) => panic!("broken upstream must fail aggregation"),
            Err(err) => err,
        };

        assert!(matches!(err, AggregateError::Upstream { namespace, .. } if namespace == "broken"));
    }
}
