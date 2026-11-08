<style>
.rustdoc-hidden { display: none; }
</style>

<div class="rustdoc-hidden">

# rmcp-codemode

Code mode for [RMCP](https://github.com/modelcontextprotocol/rust-sdk) — the third-party reference implementation for [rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437).

</div>

Expose upstream MCP servers' tools to any host as a **single `codemode` script tool**. The model writes a short JavaScript program; upstream tools run inside a QuickJS sandbox and their intermediate results never round-trip through the model. Per-tool exposure metadata (`direct` / `deferred` / `codemode`) decides what else the host sees. Tools that need a human approve first: the run **pauses**, the host approves, and the run **resumes by replaying** the calls already made.

The design is the union of three shipped codemode implementations, expressed as one model: **pi**'s script contract and host-controlled runs, **OpenCode**'s hierarchical tool space and paged discovery, **Cloudflare**'s connector namespaces, approval annotations, and replay states. Engine-agnostic core, one QuickJS backend.

## Why

- **Schemas are re-sent every turn.** Anthropic measured ~77K tokens before the first user message with 50+ MCP tools, dropping to ~8.7K with deferred loading ([Advanced tool use](https://www.anthropic.com/research/advanced-tool-use)). Servers with big surfaces (e.g. [ghidra-mcp](https://github.com/bethington/ghidra-mcp), 209 tools) implement their own lazy loading because hosts don't provide it.
- **Tool results always round-trip through the model**, even when they are just input for the next tool call. Anthropic measured 150K → 2K tokens for exactly this pattern ([Code execution with MCP](https://www.anthropic.com/research/code-execution-with-mcp)).
- **Server authors can't declare intent.** Every host invents its own policy; nothing in the SDK carries it. Codex reserves the same three states (`ToolExposure::Direct | Deferred | CodeModeOnly`) and Anthropic standardizes deferred discovery ([Tool Search Tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool)).

Code mode solves the first two problems in one mechanism: tool schemas stay out of the context (tools are functions, deferred by default), and intermediate results stay inside the script.

## Crates

| Crate | Purpose |
|---------|-------------|
| [`rmcp-codemode`](crates/rmcp-codemode) | Engine-agnostic core: the tool [`Space`](crates/rmcp-codemode/src/space.rs) (segments + namespace metadata + exposure), the [`Catalog`](crates/rmcp-codemode/src/catalog.rs) (inline budget, paged search, describe), the [`Budget`](crates/rmcp-codemode/src/budget.rs), the script contract and its rendering ([`result`](crates/rmcp-codemode/src/result.rs), [`signature`](crates/rmcp-codemode/src/signature.rs), [`identity`](crates/rmcp-codemode/src/identity.rs)), and the [`Engine`](crates/rmcp-codemode/src/engine.rs) contracts — including `SuspendingEngine`, the host-controlled seam that approval and replay build on |
| [`rmcp-codemode-quickjs`](crates/rmcp-codemode-quickjs) | The QuickJS engine: fresh VM per run, the script bridge, suspending runs with replay, and the [`CodemodeServer`](crates/rmcp-codemode-quickjs/src/server.rs) — a drop-in rmcp `ServerHandler` exposing exactly one `codemode` tool |

## Transports

The transport layer is rmcp's, so everything rmcp connects to, this crate aggregates:

| Direction | Transports |
|:-:|---|
| **Upstream (client side)** | `TokioChildProcess` (spawn an MCP server binary), Streamable HTTP, or any in-process duplex |
| **Downstream (server side)** | `stdio` for CLI hosts, Streamable HTTP for networked hosts, or any in-process duplex (tests use this) |

## Usage

Build a tool space from MCP upstreams and host-native tools, and serve it as one `codemode` tool:

```rust,ignore
use std::sync::Arc;
use rmcp::{ServiceExt, transport::{TokioChildProcess, stdio}};
use rmcp_codemode::{Exposure, NamespaceMeta, Space, ToolHandle};
use rmcp_codemode_quickjs::{CodemodeServer, NativeTool, QuickJsEngine, FileStorePersister};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Connect an upstream MCP server (any rmcp transport) and register
    //    its tools under a namespace. The server's handshake `instructions`
    //    become the namespace's guidance automatically.
    let github = ().serve(TokioChildProcess::new(
        tokio::process::Command::new("npx")
            .args(["-y", "@modelcontextprotocol/server-github"]),
    )?)
    .await?;
    let mut space = Space::new();
    space.set_namespace(
        "github".into(),
        NamespaceMeta::with_description("GitHub repositories and issues"),
    );
    for tool in github.peer().list_all_tools().await? {
        space.insert(ToolHandle::new(
            format!("github.{}", tool.name).split('.').map(str::to_owned).collect(),
            tool.description.clone().unwrap_or_default(),
            Arc::new(move |args| {
                // dispatch to github.peer().call_tool(...)
                #![allow(unused)]
                Box::pin(async { Ok(json!({})) })
            }),
        ))?;
    }

    // 2. Host-native tools enter the same way. Default exposure is
    //    CodeModeOnly; Direct also lists them to the host.
    let native = NativeTool::new("bash", "Run a shell command", |args| {
        Box::pin(async move {
            Ok(json!({ "output": run_shell(args["command"].as_str().unwrap_or("")) }))
        })
    });

    // 3. Serve everything as one `codemode` tool. The store persists across
    //    calls (and restarts) through the persister.
    let server = CodemodeServer::new(QuickJsEngine::default(), space)
        .with_native_tools(vec![native])
        .with_store_persistence(Arc::new(FileStorePersister::new("codemode-store.json")));
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
```

The host (and the model) sees exactly one tool:

```json
{"name": "codemode", "inputSchema": {"properties": {"script": {"type": "string"}}}}
```

A model turn looks like this — two upstream calls pipe together, and only the final line reaches the model:

```js
// script
const cities = await tools.github.list_issues({ state: "open" });
const readings = await Promise.all(cities.map((issue) => tools.github.get_issue({ id: issue.id })));
const oldest = readings.reduce((a, b) => (a.created_at < b.created_at ? a : b));
text(`oldest: ${oldest.title}`);
return { count: readings.length, oldest: oldest.title };
```

## The script environment

The script is the body of an async function: top-level `await` and `return` work.

| Global | Purpose |
|---|---|
| `tools.<namespace>.<tool>(args)` | Every registered tool — MCP and host-native alike — as namespaced promise-returning functions. Unknown paths fail with pi's guard message (close-match suggestions: `websearch` → did you mean `web_search`?). |
| `text(value)` | Append a text item to the output. |
| `image(dataUri)` | Append an image (base64 data URI); it also becomes an MCP image content block. |
| `console.log/info/warn/error/debug` | Console lines, returned in one `<console_output>` block. |
| `ALL_TOOLS` | `{ name, description }` for every callable tool. |
| `searchTools(query, { limit?, namespace?, offset? })` | BM25-ranked discovery, paged: `{ items, remaining, next }` (default limit 8). |
| `describeTool(name)` | Description plus TypeScript declaration, rendered from the JSON Schemas. |
| `describeNamespace(name)` | `{ name, description?, instructions?, tools }` — resolves through aliases (`describeNamespace("docs")` finds `mcp__docs`). |
| `store(key, value)` / `load(key)` | Small JSON values kept across codemode calls; writes commit only when the script succeeds. Back it with a persister for restart survival. |
| `exit()` | End the script successfully — even when a try/catch swallows it. |
| `return value` | A top-level `return` adds the value like `text()`. |
| `// @options: {...}` | Optional first line: `{"max_output_tokens": ..., "timeout_ms": ...}`. |

Nothing else: no timers, `fetch`, `require`, `process`, or file system — scripts reach the outside world only through tool calls. The VM has a heap limit (256 MB default).

### Runtime semantics (pi's model)

- Each script runs in a **fresh QuickJS VM**: a runaway or failed script cannot poison the next run, and unawaited tool calls are cancelled when the script ends.
- A script waiting on a promise nothing can settle fails **immediately** (there are no timers).
- Pure-JS busy loops are preempted at the deadline by the VM's interrupt handler.
- Scripts beyond the worker limit (2 by default; `Budget::max_workers`) queue in arrival order and fail without running if no worker frees up in time.
- Over-budget output keeps its start and end; the full text is written to a file whose path is included in the result.
- The result follows pi's format: `Script completed`/`Script failed` with wall time, `==> text N/M <==` markers, one `<console_output>` block, and `Script error:` on failure.

## Exposure model

| Exposure | In `tools/list` | Callable by host | Callable in scripts |
|---|---|---|---|
| `Direct` | yes | yes | yes |
| `Deferred` | no | yes (guidance error) | yes |
| `CodeModeOnly` | no | no | yes |

- `Direct` tools appear in the host's `tools/list` (`ServerMode::On`) **and** are declared inline in the `codemode` description — TypeScript, grouped by namespace, within pi's 3000-token budget using round-robin selection (every namespace is represented before any namespace is complete).
- `Deferred` and `CodeModeOnly` tools never appear in `tools/list`; calling them natively answers with an instructive error directing the model to a script (the self-correction shape from Zed discussion [#64004](https://github.com/zed-industries/zed/discussions/64004)).
- Inside the sandbox every tool is callable regardless of exposure.
- `ServerMode::Only` (pi's `codemode.mode: "only"`, OpenCode 2.0's MCP default) hides direct tools behind the codemode tool entirely.

## Approval, pause, and replay

Mark a tool `requiresApproval` (Cloudflare's annotation) and runs that reach it **pause**:

```rust,ignore
let mut handle = engine.start(&script, &context).await?;
while let Some(call) = handle.next_call.next().await {
    if requires_approval(&call.name) {
        let state = SuspendState::new(script, answered_so_far);
        session.save(state);            // JSON-serializable — persist anywhere
        return paused(call.name);       // the model sees { status: "paused", ... }
    }
    let outcome = (call.execute)(call.args).await?;
    handle.deliveries.unbounded_send((call.id, Outcome::Deliver(outcome))).unwrap();
}
```

The host answers every yielded call — allow, deny, or pause. Resuming replays the recorded outcomes (the recorded calls never re-execute) and continues the script; the state is `serde`-serializable, so persistence is the host's one-liner. `CodemodeServer::run_with_approval` wraps the whole flow: pause at the annotated tool, return `{ status: "paused", executionId, pending }`, and `resume(&execution_id)` continues after approval.

### Deterministic replay

Set [`Determinism`] on the run context and the run becomes replayable (pi's model): the VM's clock starts at `started_at_ms` and advances **only** with delivered results — the host passes a clock value with each answer — and never goes backwards. `Math.random` draws from a PRNG over the seed; `WeakRef`/`FinalizationRegistry` don't exist in a deterministic VM. The recording captures each applied clock value, so a resumed run observes the same times as the original: same script + same seed + same clock values + same results → same output. [`SuspendState`] carries the determinism config, so a resumed run is self-contained.

## Development

The workspace mirrors the [rust-sdk](https://github.com/modelcontextprotocol/rust-sdk) layout and policies: resolver 3, edition 2024, workspace lints, nightly rustfmt, `just` recipes, and a [`DEPENDENCY_POLICY.md`](DEPENDENCY_POLICY.md).

```
just fmt      # nightly rustfmt
just check    # clippy, deny warnings
just test     # full suite
```

### Test provenance

The tests are **contract tests**: each lifecycle clause in [`rmcp_codemode::engine`](crates/rmcp-codemode/src/engine.rs) (fresh VM, never-settling promise fails immediately, unawaited calls cancelled, deadline preemption, partial output surviving failure, `exit()` through a swallowed catch) has a test against the contract, plus catalog semantics (pi's round-robin budget, OpenCode's paging, alias resolution, the guard message), and an end-to-end approval pause/resume round trip over a real rmcp client.

Deliberately not ported: OpenCode's `test262`/`parity`/`stdlib` suites (~500KB) test the ECMAScript conformance of their custom interpreter — QuickJS-NG ships its own conformance, which is why this crate embeds it.

## Status

PoC, tracking [rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437). Not on crates.io yet.

Known gaps:

- Namespace-scoped store domains (pi scopes stores per session/branch).
- The snippet save/run API (`codemode.run("name")` — the store persists; the script-reuse surface is next).
- Timezone is the host process's (pi runs the deterministic VM in UTC); scripts that format dates with local offsets replay process-dependently.
- `_meta` plumbing; upstream reconnection (currently rebuild-only).

## License

This project is licensed under the terms specified in the repository's [LICENSE](LICENSE) file.
