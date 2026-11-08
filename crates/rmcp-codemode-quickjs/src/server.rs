//! The `codemode` MCP server: what a host sees.
//!
//! One tool with a `script` parameter (pi/OpenCode/CF all converge here).
//! [`CodemodeServer`] wires a [`SuspendingQuickJsEngine`](crate::engine::SuspendingQuickJsEngine) (or any engine)
//! to rmcp: exposure mixing, host-native tools, approval routing, and the
//! store's persistence.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use futures::StreamExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
        ContentBlock, ListToolsResult, PaginatedRequestParams, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer},
};
use rmcp_codemode::{
    Budget, Catalog, Exposure, Mode, Outcome, RunContext, ScriptResult, Space, SuspendState,
    ToolFn, ToolHandle, Value,
};
use serde_json::json;

/// The single tool name exposed to hosts.
pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// The script store: small JSON values kept across codemode calls; writes
/// commit only when the script succeeds (pi's semantics).
pub type Store = Arc<Mutex<BTreeMap<String, Value>>>;

/// How the store persists across process restarts. A host with sessions
/// plugs in its own implementation (pi keeps the store in the session
/// file); [`FileStorePersister`] is the simple JSON-file default.
pub trait StorePersister: Send + Sync {
    /// Persist the full store after a script commits writes.
    fn save(&self, store: &BTreeMap<String, Value>);
    /// Load the store at server construction.
    fn load(&self) -> BTreeMap<String, Value>;
}

/// A [`StorePersister`] backed by one JSON file, written atomically
/// (tmp file + rename).
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
    fn save(&self, store: &BTreeMap<String, Value>) {
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

    fn load(&self) -> BTreeMap<String, Value> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

/// A host-native (non-MCP) tool for the codemode sandbox. A host like Zed
/// has built-ins — `read`, `edit`, `bash` — that enter the sandbox with the
/// same standing as MCP tools.
#[derive(Clone)]
#[non_exhaustive]
pub struct NativeTool {
    /// Registry path, e.g. `bash` or `fs.read`.
    pub path: String,
    /// Description shown in listings and discovery.
    pub description: String,
    /// How the tool is exposed. Default [`Exposure::CodeModeOnly`] (the host
    /// usually lists its built-ins itself).
    pub exposure: Exposure,
    /// The dispatcher.
    pub execute: ToolFn,
}

impl NativeTool {
    /// A codemode-only native tool.
    pub fn new<F>(path: impl Into<String>, description: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Value) -> futures::future::BoxFuture<'static, Result<Value, String>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            path: path.into(),
            description: description.into(),
            exposure: Exposure::CodeModeOnly,
            execute: Arc::new(execute),
        }
    }

    /// Opt the native tool onto the host's `tools/list` as well.
    pub fn with_exposure(mut self, exposure: Exposure) -> Self {
        self.exposure = exposure;
        self
    }

    fn into_handle(self) -> ToolHandle {
        let mut handle = ToolHandle::new(
            self.path.split('.').map(str::to_owned).collect(),
            self.description,
            self.execute,
        );
        handle.exposure = self.exposure;
        handle
    }
}

/// How directly-exposed tools are presented (pi's `codemode.mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerMode {
    /// Direct tools stay in the host's `tools/list` (default).
    #[default]
    On,
    /// Direct tools are hidden behind the codemode tool entirely (pi's
    /// `only`, OpenCode 2.0's MCP default).
    Only,
}

/// The codemode MCP server over any engine implementing
/// [`rmcp_codemode::Engine`] + [`rmcp_codemode::SuspendingEngine`].
pub struct CodemodeServer<E> {
    engine: Arc<E>,
    space: Arc<Space>,
    budget: Budget,
    mode: Mode,
    store: Store,
    persister: Option<Arc<dyn StorePersister>>,
    /// Suspended runs awaiting approval, by id (CF's pending map). A host
    /// calls [`CodemodeServer::resume`] with the id after approval.
    suspended: Arc<Mutex<BTreeMap<String, SuspendState>>>,
    next_suspension: Arc<std::sync::atomic::AtomicU64>,
    /// Admission control (pi's worker pool): at most `max_workers` scripts
    /// run concurrently; the rest queue in arrival order and fail without
    /// starting if no worker frees up within `worker_wait_ms`.
    workers: Arc<WorkerPool>,
}

/// pi's worker pool: at most [`Budget::max_workers`] scripts run at once.
/// The wait counts toward the script's deadline — a script that waits for
/// another script holds nothing, so a bounded wait prevents deadlock.
struct WorkerPool {
    slots: Arc<tokio::sync::Semaphore>,
    worker_wait_ms: u64,
}

impl WorkerPool {
    fn new(max_workers: usize, worker_wait_ms: u64) -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(max_workers.max(1))),
            worker_wait_ms,
        }
    }

    /// Reserve a slot, waiting at most `min(deadline, worker_wait_ms)` (pi:
    /// the wait counts toward the script's deadline). `None` means no
    /// worker became free in time.
    async fn reserve(&self, deadline_ms: Option<u64>) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let wait = match deadline_ms {
            Some(deadline) => deadline.min(self.worker_wait_ms),
            None => self.worker_wait_ms,
        };
        match tokio::time::timeout(
            std::time::Duration::from_millis(wait.max(1)),
            tokio::sync::Semaphore::acquire_owned(Arc::clone(&self.slots)),
        )
        .await
        {
            Ok(Ok(permit)) => Some(permit),
            _ => None,
        }
    }
}

impl<E> CodemodeServer<E>
where
    E: rmcp_codemode::Engine + rmcp_codemode::SuspendingEngine + Send + Sync + 'static,
{
    /// A server over an engine and a tool space.
    pub fn new(engine: E, space: Space) -> Self {
        Self {
            engine: Arc::new(engine),
            workers: Arc::new(WorkerPool::new(
                Budget::default().max_workers,
                Budget::default().worker_wait_ms,
            )),
            space: Arc::new(space),
            budget: Budget::default(),
            mode: Mode::default(),
            store: Arc::default(),
            persister: None,
            suspended: Arc::default(),
            next_suspension: Arc::default(),
        }
    }

    /// Mix host-native tools into the space.
    pub fn with_native_tools(mut self, native: Vec<NativeTool>) -> Self {
        let space = Arc::get_mut(&mut self.space).expect("with_native_tools before serving");
        for tool in native {
            let _ = space.insert(tool.into_handle());
        }
        self
    }

    /// Set the resource budgets. The worker pool is rebuilt from
    /// `max_workers`/`worker_wait_ms` (pi: shared by every session in the
    /// process, at most two codemode workers).
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.workers = Arc::new(WorkerPool::new(budget.max_workers, budget.worker_wait_ms));
        self.budget = budget;
        self
    }

    /// Set how directly-exposed tools are presented.
    pub fn with_mode(mut self, mode: ServerMode) -> Self {
        self.mode = match mode {
            ServerMode::On => Mode::On,
            ServerMode::Only => Mode::Only,
        };
        self
    }

    /// Persist the script store through `persister`.
    pub fn with_store_persistence(mut self, persister: Arc<dyn StorePersister>) -> Self {
        *self.store.lock().unwrap() = persister.load();
        self.persister = Some(persister);
        self
    }

    /// The codemode tool's model-facing description (the catalog's inline
    /// listing, budgeted).
    pub fn tool_description(&self) -> String {
        self.catalog().description(self.mode)
    }

    fn catalog(&self) -> Catalog<'_> {
        Catalog::new(&self.space, &self.budget)
    }

    fn run_context(&self) -> RunContext {
        RunContext::new((*self.space).clone())
            .with_budget(self.budget.clone())
            .with_store(self.store.lock().unwrap().clone())
    }

    /// Resume a suspended run by id (CF's `runtime.approve` flow): the
    /// recorded calls replay, calls past the recording yield again.
    pub async fn resume(&self, id: &str) -> Result<CallToolResult, McpError> {
        let state = self.suspended.lock().unwrap().remove(id).ok_or_else(|| {
            McpError::invalid_params(format!("unknown suspended run '{id}'"), None)
        })?;
        // Approval means "allow what follows": the resumed run executes
        // without pausing (the host already decided).
        let result = self
            .engine
            .execute(&state.code, &self.run_context())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.persist_store(&result);
        Ok(self.result_to_call_result(&result))
    }

    fn persist_store(&self, result: &rmcp_codemode::ScriptResult) {
        if !result.ok || result.store_writes.set.is_empty() {
            return;
        }
        {
            let mut store = self.store.lock().unwrap();
            for (key, value) in &result.store_writes.set {
                store.insert(key.clone(), value.clone());
            }
            for key in &result.store_writes.delete {
                store.remove(key);
            }
        }
        if let Some(persister) = &self.persister {
            persister.save(&self.store.lock().unwrap());
        }
    }

    /// pi's result format, as MCP content.
    fn result_to_call_result(&self, result: &rmcp_codemode::ScriptResult) -> CallToolResult {
        let text = rmcp_codemode::render_result_text_with_overflow(result, &std::env::temp_dir());
        let mut content = vec![ContentBlock::text(text)];
        for item in &result.output {
            if let rmcp_codemode::OutputItem::Image { data, mime_type } = item {
                content.push(ContentBlock::image(data.clone(), mime_type.clone()));
            }
        }
        if result.ok {
            CallToolResult::success(content)
        } else {
            CallToolResult::error(content)
        }
    }

    /// Run one script one-shot (public for hosts that want the result
    /// directly, e.g. the store test).
    pub async fn run_one_shot(&self, script: &str) -> Result<CallToolResult, McpError> {
        // pi: the wait for a worker counts toward the script's deadline,
        // and a script that never gets one fails without starting.
        let Some(_permit) = self.workers.reserve(self.budget.timeout_ms).await else {
            return Ok(self.result_to_call_result(&ScriptResult::failed(
                format!(
                    "No codemode worker became free within {} seconds. All {} workers, shared by every session in this process, are held by other scripts. Retry after they finish.",
                    self.budget.worker_wait_ms as f64 / 1000.0,
                    self.budget.max_workers
                )
                .to_owned()
                + "\n\nThe script did not start.",
                self.budget.max_output_tokens,
                0,
            )));
        };
        let result = self
            .engine
            .execute(script, &self.run_context())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.persist_store(&result);
        Ok(self.result_to_call_result(&result))
    }

    /// Run a script under host control, pausing at tools annotated
    /// `requires_approval` (CF's annotations). The run's state is stored
    /// under a returned id; [`CodemodeServer::resume`] continues it.
    pub async fn run_with_approval(&self, script: &str) -> Result<CallToolResult, McpError> {
        let Some(_permit) = self.workers.reserve(self.budget.timeout_ms).await else {
            return Ok(self.result_to_call_result(&ScriptResult::failed(
                format!(
                    "No codemode worker became free within {} seconds. All {} workers, shared by every session in this process, are held by other scripts. Retry after they finish.",
                    self.budget.worker_wait_ms as f64 / 1000.0,
                    self.budget.max_workers
                )
                .to_owned()
                + "\n\nThe script did not start.",
                self.budget.max_output_tokens,
                0,
            )));
        };
        let mut handle = self
            .engine
            .start(script, &self.run_context())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut answered: Vec<rmcp_codemode::AnsweredCall> = Vec::new();
        loop {
            let Some(call) = handle.next_call.next().await else {
                // The run ended; deliver its result.
                let result = handle.result.await.unwrap_or_default();
                self.persist_store(&result);
                return Ok(self.result_to_call_result(&result));
            };
            let tool = self.space.entry(&call.name);
            let requires_approval = tool.is_some_and(|t| t.annotations.requires_approval);
            if requires_approval {
                // Pause: record answered calls, store the state, return the
                // pending id (CF's `{ status: "paused", executionId }`).
                let id = format!(
                    "suspended-{}",
                    self.next_suspension
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                );
                let state = SuspendState::new(script, answered);
                self.suspended.lock().unwrap().insert(id.clone(), state);
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    json!({
                        "status": "paused",
                        "executionId": id,
                        "pending": call.name,
                    })
                    .to_string(),
                )]));
            }
            let outcome = match (call.execute)(call.args.clone()).await {
                Ok(value) => Outcome::Deliver(value),
                Err(message) => Outcome::Fail {
                    message,
                    properties: Value::Null,
                },
            };
            let recorded = match &outcome {
                Outcome::Deliver(value) => rmcp_codemode::RecordedOutcome::Ok(value.clone()),
                Outcome::Fail { message, .. } => {
                    rmcp_codemode::RecordedOutcome::Err(message.clone())
                }
                // A non_exhaustive enum: unknown variants skip recording.
                _ => continue,
            };
            answered.push(rmcp_codemode::AnsweredCall::new(
                call.id,
                call.name.clone(),
                recorded,
            ));
            handle
                .deliveries
                .unbounded_send((call.id, outcome))
                .map_err(|_| McpError::internal_error("the run stopped answering", None))?;
        }
    }
}

impl<E> ServerHandler for CodemodeServer<E>
where
    E: rmcp_codemode::Engine + rmcp_codemode::SuspendingEngine + Send + Sync + 'static,
{
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(Default::default())
            .with_instructions("Code mode: upstream tools are callable from scripts, not listed.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = vec![Tool::new(
            CODEMODE_TOOL_NAME,
            self.tool_description(),
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
        )];
        // In `on` mode, direct tools join the host's list (pi's mode: on).
        if self.mode == Mode::On {
            for tool in self.space.entries().filter(|t| t.exposure.is_listed()) {
                tools.push(Tool::new(
                    tool.canonical(),
                    tool.description.clone().unwrap_or_default(),
                    tool.schema
                        .input
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
            // Route native direct tools (a host calls them natively).
            let Some(tool) = self.space.entry(&request.name) else {
                return Err(McpError::method_not_found::<CallToolRequestMethod>());
            };
            if !tool.exposure.is_listed() {
                let result = CallToolResult::error(vec![ContentBlock::text(format!(
                    "Tool '{}' is not directly callable. Write a codemode script instead: \
                     call `tools.{}(...)` inside a `codemode` tool call.",
                    request.name, request.name
                ))]);
                return Ok(CallToolResponse::Complete(result));
            }
            let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
            let outcome = (tool.execute)(args).await;
            let result = match outcome {
                Ok(value) => CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned()),
                )]),
                Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
            };
            return Ok(CallToolResponse::Complete(result));
        }
        let script = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("script"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                McpError::invalid_params("missing required `script` string argument", None)
            })?
            .to_owned();

        // Runs annotated with approval-required tools pause; everything
        // else executes one-shot. Approval pauses are opt-in via
        // `run_with_approval`; a plain `call_tool` runs one-shot.
        self.run_one_shot(&script)
            .await
            .map(CallToolResponse::Complete)
    }
}
