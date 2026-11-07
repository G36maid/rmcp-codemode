//! The `codemode` MCP server: what a host sees.
//!
//! One tool with a `script` parameter. Upstream tools are NOT listed to the
//! model — they are callable as `tools.<namespace>.<tool>(args)` inside the
//! script, discoverable via `ALL_TOOLS` (pi's semantics). Exposure states
//! shape what else the host sees; see [`CodemodeMode`].

use std::{collections::BTreeMap, sync::Arc};

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
        ContentBlock, ListToolsResult, PaginatedRequestParams, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer},
};
use rmcp_codemode::exposure::ToolExposure;
use serde_json::{Value, json};

use super::{Sandbox, SandboxTool, Store, ToolFn};

/// The single tool name exposed to hosts.
pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// How many tool calls one script may make (safety bound).
pub const DEFAULT_MAX_TOOL_CALLS: usize = 500;

/// The inline budget for tool declarations in the `codemode` description,
/// in estimated model tokens (pi's `codemode.inlineBudget`). Declarations
/// beyond the budget are omitted; the model finds those tools with
/// `searchTools` and `describeTool` inside the script.
pub const INLINE_BUDGET_TOKENS: u64 = 3_000;

/// How the server presents directly-exposed tools to the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::exhaustive_enums)] // two fixed modes
pub enum CodemodeMode {
    /// Direct tools stay in the host's `tools/list` and are additionally
    /// declared in the `codemode` description (pi's `codemode.mode:
    /// "on"`, the default).
    #[default]
    On,
    /// Direct tools are hidden from the host's `tools/list` and listed in
    /// the `codemode` description instead (pi's `codemode.mode: "only"`,
    /// OpenCode 2.0's default for MCP servers).
    Only,
}

/// How long a script may wait for a free worker before failing without
/// running (pi's constant). The effective cap is the smaller of this and
/// the script's own timeout.
pub const WORKER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The default worker count (pi: at most two codemode workers across all
/// sessions in one process), overridable via the `CODEMODE_MAX_WORKERS`
/// environment variable.
pub fn default_max_workers() -> usize {
    std::env::var("CODEMODE_MAX_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(2)
}

/// Durable storage for the script-visible [`Store`], so values survive a
/// process restart (pi persists the store into the session file; a Rust
/// host plugs its own session persistence in here).
pub trait StorePersister: Send + Sync {
    /// Persist the full store; called after each script that committed
    /// writes.
    fn save(&self, store: &std::collections::BTreeMap<String, Value>);
    /// Load the store at server construction.
    fn load(&self) -> std::collections::BTreeMap<String, Value>;
}

/// A [`StorePersister`] that keeps the store in one JSON file, written
/// atomically (tmp file + rename).
pub struct FileStorePersister {
    path: std::path::PathBuf,
}

impl FileStorePersister {
    /// A persister backed by the JSON file at `path`.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl StorePersister for FileStorePersister {
    fn save(&self, store: &std::collections::BTreeMap<String, Value>) {
        let tmp = self.path.with_extension("json.tmp");
        if serde_json::to_string_pretty(store)
            .map_err(|_| ())
            .and_then(|text| std::fs::write(&tmp, text).map_err(|_| ()))
            .and_then(|()| std::fs::rename(&tmp, &self.path).map_err(|_| ()))
            .is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn load(&self) -> std::collections::BTreeMap<String, Value> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

/// An upstream tool dispatcher wired into the sandbox.
#[derive(Clone)] // the executor is shared behind an Arc
#[allow(clippy::exhaustive_structs)] // user-constructed by design
pub struct UpstreamTool {
    /// Registry path, e.g. `github.list_issues`.
    pub path: String,
    /// Description shown in `ALL_TOOLS` / `searchTools`.
    pub description: Option<String>,
    /// The tool's JSON Schema for its input, rendered into the TypeScript
    /// declarations `describeTool` / `describeNamespace` return.
    pub input_schema: Option<Value>,
    /// How the tool is exposed. Direct tools appear in the host's
    /// `tools/list` (mode [`CodemodeMode::On`]) and in the `codemode`
    /// description's inline declarations; deferred and codemode-only
    /// tools are discoverable only inside scripts.
    pub exposure: ToolExposure,
    /// Async dispatcher: JSON args string → JSON result string.
    pub execute: ToolFn,
}

/// Wire every registered tool of an [`Aggregator`](rmcp_codemode::aggregate::Aggregator)
/// into sandbox tools dispatched through [`Aggregator::call_from_script`](rmcp_codemode::aggregate::Aggregator::call_from_script).
pub fn tools_from_aggregator(
    aggregator: std::sync::Arc<rmcp_codemode::aggregate::Aggregator>,
) -> Vec<UpstreamTool> {
    aggregator
        .registry()
        .entries()
        .map(|entry| {
            let path = entry.path.clone();
            let description = entry.tool.description.as_ref().map(|d| d.to_string());
            let input_schema = Some(Value::Object((*entry.tool.input_schema).clone()));
            let exposure = entry.exposure;
            let aggregator = std::sync::Arc::clone(&aggregator);
            UpstreamTool {
                path: path.clone(),
                description,
                input_schema,
                exposure,
                execute: Arc::new(move |args_json: String| {
                    let aggregator = std::sync::Arc::clone(&aggregator);
                    let path = path.clone();
                    Box::pin(async move {
                        let arguments: Option<serde_json::Map<String, Value>> =
                            serde_json::from_str(&args_json)
                                .ok()
                                .and_then(|v: Value| v.as_object().cloned());
                        let result = aggregator
                            .call_from_script(&path, arguments)
                            .await
                            .map_err(|e| e.to_string())?;
                        result
                            .content
                            .first()
                            .and_then(|c| c.as_text())
                            .map(|t| t.text.clone())
                            .ok_or_else(|| "upstream returned no text content".to_owned())
                    })
                }),
            }
        })
        .collect()
}

/// A host-native (non-MCP) tool for the codemode sandbox.
///
/// A host like Zed has built-in tools — `read`, `edit`, `bash`, … — that
/// are not MCP servers. They enter the sandbox the same way MCP tools
/// do: as a path, a description, and a JSON dispatcher, so a script can
/// pipe them with upstream MCP tools (pi: every session tool is a method
/// of `tools`, native or not). The executor takes parsed JSON arguments
/// and returns a JSON value, so no string plumbing is needed.
#[allow(clippy::exhaustive_structs)] // user-constructed by design
pub struct NativeTool {
    /// Registry path, e.g. `bash` or `fs.read`. Top-level paths put the
    /// tool at `tools.bash`; dotted paths nest it like MCP namespaces.
    pub path: String,
    /// Description shown in `ALL_TOOLS` / `searchTools` and the inline
    /// declarations.
    pub description: String,
    /// The tool's JSON Schema for its input (optional; without one the
    /// TypeScript declaration types the argument as an open record).
    pub input_schema: Option<Value>,
    /// How the tool is exposed. Default [`ToolExposure::CodeModeOnly`]:
    /// a host's built-ins are usually already native tools of their own
    /// (`Direct` would duplicate them in `tools/list`); `CodeModeOnly`
    /// makes them script-reachable without changing the host's list.
    pub exposure: ToolExposure,
    /// The parsed-JSON executor.
    pub execute: NativeFn,
}

/// A native tool's executor: parsed JSON arguments in, a JSON value or
/// error message out.
pub type NativeFn = Arc<
    dyn Fn(Value) -> std::pin::Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

impl NativeTool {
    /// A codemode-only native tool from a path, a description, and an
    /// async executor taking parsed JSON arguments.
    pub fn new<F, Fut>(path: impl Into<String>, description: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        Self {
            path: path.into(),
            description: description.into(),
            input_schema: None,
            exposure: ToolExposure::CodeModeOnly,
            execute: Arc::new(move |args: Value| Box::pin(execute(args))),
        }
    }

    /// Attach the tool's JSON Schema for its input.
    pub fn with_input_schema(mut self, schema: Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// Set the tool's exposure (default [`ToolExposure::CodeModeOnly`]).
    pub fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = exposure;
        self
    }
}

impl From<NativeTool> for UpstreamTool {
    fn from(native: NativeTool) -> Self {
        let execute = native.execute;
        UpstreamTool {
            path: native.path,
            description: Some(native.description),
            input_schema: native.input_schema,
            exposure: native.exposure,
            execute: Arc::new(move |args_json: String| {
                let execute = Arc::clone(&execute);
                Box::pin(async move {
                    // `"null"` arrives when the script called the tool
                    // without arguments.
                    let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
                    let value = execute(args).await?;
                    serde_json::to_string(&value).map_err(|e| e.to_string())
                })
            }),
        }
    }
}

/// A ready-to-serve codemode MCP server over a set of upstream tools.
///
/// Construct with [`CodemodeServer::new`], then serve it over any rmcp
/// transport; hosts see exactly one `codemode` tool. Each script gets a
/// fresh QuickJS VM (pi's model): a runaway or failed script cannot poison
/// the next run, and the handler stays `Send` without a shared runtime.
pub struct CodemodeServer {
    tools: Arc<Vec<UpstreamTool>>,
    timeout: Option<std::time::Duration>,
    memory_limit: usize,
    mode: CodemodeMode,
    namespace_descriptions: Arc<BTreeMap<String, String>>,
    store: Store,
    /// Bounded concurrency for script VMs, acquired FIFO (pi's worker
    /// pool: at most N workers alive, scripts queue in arrival order).
    worker_semaphore: Arc<tokio::sync::Semaphore>,
    worker_wait: std::time::Duration,
    persister: Option<Arc<dyn StorePersister>>,
    overflow_dir: std::path::PathBuf,
}

impl CodemodeServer {
    /// Create the server over a set of upstream tools.
    pub fn new(tools: Vec<UpstreamTool>) -> Self {
        let persister: Option<Arc<dyn StorePersister>> = None;
        let store = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::default()));
        Self {
            tools: Arc::new(tools),
            timeout: None,
            memory_limit: super::types::DEFAULT_MEMORY_LIMIT,
            mode: CodemodeMode::default(),
            namespace_descriptions: Arc::default(),
            store,
            worker_semaphore: Arc::new(tokio::sync::Semaphore::new(default_max_workers())),
            worker_wait: WORKER_WAIT,
            persister,
            overflow_dir: std::env::temp_dir(),
        }
    }

    /// Create the server from a connected aggregator: every registered
    /// tool becomes callable from scripts, routed by registry path, with
    /// the aggregator's namespace descriptions served to
    /// `describeNamespace`.
    pub fn from_aggregator(
        aggregator: std::sync::Arc<rmcp_codemode::aggregate::Aggregator>,
    ) -> Self {
        let namespace_descriptions: BTreeMap<String, String> = aggregator
            .namespaces()
            .filter_map(|namespace| {
                aggregator
                    .registry()
                    .namespace_description(namespace)
                    .map(|d| (namespace.to_owned(), d.to_owned()))
            })
            .collect();
        let mut server = Self::new(tools_from_aggregator(aggregator));
        server.namespace_descriptions = Arc::new(namespace_descriptions);
        server
    }

    /// Mix host-native (non-MCP) tools into the sandbox: they become
    /// `tools.<path>` alongside every MCP tool, with discovery and
    /// declarations identical to upstream tools. Chain after
    /// [`CodemodeServer::from_aggregator`] or [`CodemodeServer::new`].
    pub fn with_native_tools(mut self, native: Vec<NativeTool>) -> Self {
        let mut tools = (*self.tools).clone();
        tools.extend(native.into_iter().map(UpstreamTool::from));
        tools.sort_by(|a, b| a.path.cmp(&b.path));
        self.tools = Arc::new(tools);
        self
    }

    /// Set how directly-exposed tools are presented (default
    /// [`CodemodeMode::On`]).
    pub fn with_mode(mut self, mode: CodemodeMode) -> Self {
        self.mode = mode;
        self
    }

    /// Persist the script store through `persister`: values are loaded at
    /// construction and saved after each script that committed writes
    /// (pi keeps the store in the session file; hosts plug in their own
    /// session persistence).
    pub fn with_store_persistence(mut self, persister: Arc<dyn StorePersister>) -> Self {
        *self.store.lock().unwrap() = persister.load();
        self.persister = Some(persister);
        self
    }

    /// Set the worker-pool size: at most `max` script VMs run at once;
    /// further scripts queue in arrival order (pi's worker pool).
    pub fn with_max_workers(mut self, max: usize) -> Self {
        self.worker_semaphore = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
        self
    }

    /// Set how long a script may wait for a free worker before failing
    /// without running (pi's 60 seconds). The effective cap is the
    /// smaller of this and the script's own timeout.
    pub fn with_worker_wait(mut self, wait: std::time::Duration) -> Self {
        self.worker_wait = wait;
        self
    }

    /// Set the directory where truncated scripts' full output is written
    /// (default: the system temp directory).
    pub fn with_overflow_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.overflow_dir = dir.into();
        self
    }

    /// Set a default script deadline.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set the per-run VM heap limit (bytes).
    pub fn with_memory_limit(mut self, memory_limit: usize) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    /// The tool description shown to the model: the script environment's
    /// globals, plus TypeScript declarations for directly-exposed tools
    /// within the inline budget (pi: deferred tools are not listed, the
    /// model finds them with `searchTools`/`describeTool`).
    pub fn tool_definition(&self) -> Tool {
        let mut description = String::from(
            "Run a JavaScript script that calls tools. Upstream tools are available \
             as `tools.<namespace>.<tool>(args)` returning promises; use \
             `Promise.allSettled` to fan out and filter results before returning. \
             Only the script's return value, `text()` output, and `console` lines \
             reach the model — intermediate tool results stay inside the script.\n\n\
             The script may start with an options line: \
             `// @options: {\"max_output_tokens\": 2000, \"timeout_ms\": 60000}`.\n\n\
             Globals: `tools.*`, `text(value)`, `image(dataUri)`, \
             `console.log(...)`, `exit()`, `store(key, value)` / `load(key)`, \\
             `ALL_TOOLS` (every tool as `{ name, description }`), \\
             `searchTools(query, { limit?, namespace? })` (BM25 ranking, \
             default limit 8), `describeTool(name)` (description plus \
             TypeScript declaration), `describeNamespace(name)`. The script \
             is the body of an async function: top-level `await` and `return` \
             work. No timers, network, or file system: a script waiting on a \
             promise nothing can settle fails immediately.",
        );
        let inline = self.inline_declarations();
        if !inline.is_empty() {
            description.push_str("\n\n## Tools\n\n");
            description.push_str(&inline);
        }
        Tool::new(
            CODEMODE_TOOL_NAME,
            description,
            json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "Raw JavaScript source (the body of an async function)."
                    }
                },
                "required": ["script"]
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        )
    }

    /// TypeScript declarations of directly-exposed tools, grouped by
    /// namespace, within the inline budget (pi's `codemode.inlineBudget`:
    /// declarations that do not fit are omitted — the model finds them
    /// with `searchTools` and `describeTool` inside the script).
    fn inline_declarations(&self) -> String {
        let budget = (INLINE_BUDGET_TOKENS as usize) * rmcp_codemode::result::CHARS_PER_TOKEN;
        let mut used = 0usize;
        let mut omitted = 0usize;
        let mut by_namespace: BTreeMap<&str, Vec<&UpstreamTool>> = BTreeMap::new();
        for tool in self.tools.iter().filter(|t| t.exposure.is_listed()) {
            by_namespace
                .entry(tool.path.split('.').next().unwrap_or(&tool.path))
                .or_default()
                .push(tool);
        }
        let mut sections = String::new();
        for (namespace, tools) in &by_namespace {
            let mut members = String::new();
            if let Some(description) = self.namespace_descriptions.get(*namespace) {
                members.push_str(&format!("<!-- {description} -->\n"));
            }
            for tool in tools {
                let declaration = super::ts_declaration(
                    &tool.path,
                    tool.description.as_deref(),
                    tool.input_schema.as_ref(),
                );
                let needed = declaration.len() + 1;
                if used + needed > budget {
                    omitted += 1;
                    continue;
                }
                used += needed;
                members.push_str(&declaration);
                members.push('\n');
            }
            if members.is_empty() {
                continue;
            }
            sections.push_str(&format!("### {namespace}\n\n{members}\n"));
        }
        if omitted > 0 {
            sections.push_str(&format!(
                "{omitted} more tools exceed the inline declaration budget; call them via \
                 `tools.<path>(...)` and find them with `searchTools` or `describeTool`."
            ));
        }
        sections
    }

    /// Dispatch one script in a fresh VM, converting the sandbox result
    /// into an MCP `CallToolResult`.
    ///
    /// The VM is created inside a dedicated task that owns it, so this
    /// future stays `Send` (rmcp's `ServerHandler` requirement).
    pub async fn run_script(&self, script: &str) -> Result<CallToolResult, super::SandboxError> {
        // Worker pool (pi): at most N script VMs alive, scripts wait in
        // arrival order; one that gets no worker within the wait cap (or
        // before its own timeout) fails without running.
        let wait_cap = self
            .timeout
            .map_or(self.worker_wait, |t| t.min(self.worker_wait));
        let permit = match tokio::time::timeout(
            wait_cap,
            Arc::clone(&self.worker_semaphore).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            _ => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Script failed: no codemode worker available within {}s",
                    wait_cap.as_secs()
                ))]));
            }
        };

        let script = script.to_owned();
        let tools = Arc::clone(&self.tools);
        let timeout = self.timeout;
        let memory_limit = self.memory_limit;
        let store = Arc::clone(&self.store);
        let namespace_descriptions = Arc::clone(&self.namespace_descriptions);
        let outcome = tokio::spawn(async move {
            let sandbox_tools: Vec<SandboxTool> = tools
                .iter()
                .map(|t| {
                    let tool = SandboxTool::new(
                        t.path.clone(),
                        t.description.clone().unwrap_or_default(),
                        Arc::clone(&t.execute),
                    );
                    match &t.input_schema {
                        Some(schema) => tool.with_input_schema(schema.clone()),
                        None => tool,
                    }
                })
                .collect();
            let mut sandbox = Sandbox::with_memory_limit(memory_limit).await?;
            sandbox.set_namespace_descriptions((*namespace_descriptions).clone());
            sandbox
                .execute_with_store(&script, sandbox_tools, timeout, Some(&store))
                .await
        });
        let result = match outcome.await {
            Ok(result) => result?,
            Err(e) => {
                drop(permit);
                return Err(super::SandboxError::Engine(format!(
                    "script task failed: {e}"
                )));
            }
        };
        // The worker slot frees as soon as the VM is done, not when the
        // result is rendered.
        drop(permit);

        // Persist committed store writes (pi keeps the store in the
        // session file).
        if let (true, Some(persister)) = (result.ok, &self.persister) {
            persister.save(&self.store.lock().unwrap());
        }

        // pi's codemode result format: a header with the wall time, the
        // script's text/console output, and `Script error:` on failure.
        // Over-budget output keeps its start and end, and the full text
        // is written to a file whose path is included in the result.
        // Images additionally become MCP image content blocks, in output
        // order after the text.
        let text = super::render_result_text_with_overflow(&result, &self.overflow_dir);
        let mut content = vec![ContentBlock::text(text)];
        for item in &result.output {
            if let super::OutputItem::Image { data, mime_type } = item {
                content.push(ContentBlock::image(data.clone(), mime_type.clone()));
            }
        }
        if result.ok {
            Ok(CallToolResult::success(content))
        } else {
            Ok(CallToolResult::error(content))
        }
    }
}

impl ServerHandler for CodemodeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(Default::default()).with_instructions(
            "Code mode: upstream MCP tools are callable from scripts, not listed.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = vec![self.tool_definition()];
        // Direct tools stay in the host's tools/list in `On` mode; `Only`
        // hides them behind the codemode tool entirely (pi's
        // `codemode.mode`, OpenCode 2.0's default).
        if self.mode == CodemodeMode::On {
            for tool in self.tools.iter().filter(|t| t.exposure.is_listed()) {
                tools.push(Tool::new(
                    tool.path.clone(),
                    tool.description.clone().unwrap_or_default(),
                    tool.input_schema
                        .clone()
                        .and_then(|schema| schema.as_object().cloned())
                        .unwrap_or_default(),
                ));
            }
        }
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != CODEMODE_TOOL_NAME {
            return self.call_upstream(&request.name, request.arguments).await;
        }
        let script = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("script"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                McpError::invalid_params("missing required `script` string argument", None)
            })?
            .to_owned();

        self.run_script(&script)
            .await
            .map(CallToolResponse::Complete)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

impl CodemodeServer {
    /// Route a native host call to a directly-exposed tool. Deferred and
    /// codemode-only tools answer with an instructive error result (the
    /// self-correction shape from Zed discussion #64004): the model is
    /// told to call the tool from a script instead.
    async fn call_upstream(
        &self,
        name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<CallToolResponse, McpError> {
        let tool = self.tools.iter().find(|t| t.path == name);
        let Some(tool) = tool else {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        };
        if !tool.exposure.is_listed() {
            let exposure = tool.exposure;
            let result = CallToolResult::error(vec![ContentBlock::text(format!(
                "Tool '{name}' is {exposure} and not directly callable. Write a \
                 codemode script instead: call `tools.{name}(...)` inside a `codemode` \
                 tool call; discover it with `searchTools` inside the script."
            ))]);
            return Ok(CallToolResponse::Complete(result));
        }
        let args_json = arguments.map(Value::Object).unwrap_or(Value::Null);
        let args_json = serde_json::to_string(&args_json)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match (tool.execute)(args_json).await {
            Ok(value) => Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(value),
            ]))),
            Err(message) => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!("{name}: {message}")),
            ]))),
        }
    }
}
