<style>
.rustdoc-hidden { display: none; }
</style>

<div class="rustdoc-hidden">

# rmcp-codemode

Code mode for [RMCP](https://github.com/modelcontextprotocol/rust-sdk) — the third-party reference implementation for [rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437).

</div>

Expose upstream MCP servers' tools to any host as a **single `codemode` script tool**. The model writes a short JavaScript program; upstream tools run inside a QuickJS sandbox and their intermediate results never round-trip through the model. Per-tool exposure metadata (`direct` / `deferred` / `codemode`) decides what else the host sees.

Semantics follow pi's shipped `codemode` tool ([docs](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/codemode.md)), with OpenCode's [`packages/codemode`](https://github.com/anomalyco/opencode/blob/dev/packages/codemode) as the second reference for the aggregation side and Cloudflare's [Code Mode](https://blog.cloudflare.com/code-mode/) as the architecture sketch. The crate stays outside the SDK's workspace on purpose: QuickJS cannot meet the SDK's dependency policy (see [`DEPENDENCY_POLICY.md`](DEPENDENCY_POLICY.md)), and exposure logic belongs to hosts — this crate is the input, not the ask.

## Why

- **Schemas are re-sent every turn.** Anthropic measured ~77K tokens before the first user message with 50+ MCP tools, dropping to ~8.7K with deferred loading ([Advanced tool use](https://www.anthropic.com/research/advanced-tool-use)). Servers with big surfaces (e.g. [ghidra-mcp](https://github.com/bethington/ghidra-mcp), 209 tools) implement their own lazy loading because hosts don't provide it.
- **Tool results always round-trip through the model**, even when they are just input for the next tool call. Anthropic measured 150K → 2K tokens for exactly this pattern ([Code execution with MCP](https://www.anthropic.com/research/code-execution-with-mcp)).
- **Server authors can't declare intent.** Every host invents its own policy; nothing in the SDK carries it. Codex's exposure enum has `Direct`, `Deferred`, and `CodeModeOnly` among its six variants ([`codex-rs/tools`](https://github.com/openai/codex/blob/main/codex-rs/tools/src/tool_executor.rs)) and Anthropic standardizes deferred discovery ([Tool Search Tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool)).

Code mode solves the first two problems in one mechanism: tool schemas stay out of the context (tools are functions, deferred by default), and intermediate results stay inside the script.

## Crates

| Crate | Purpose |
|---------|-------------|
| [`rmcp-codemode`](crates/rmcp-codemode) | Core, engine-agnostic: [`ToolExposure`](crates/rmcp-codemode/src/exposure.rs), the aggregated [`Registry`](crates/rmcp-codemode/src/registry.rs), the BM25 [`SearchIndex`](crates/rmcp-codemode/src/search.rs), the [`Aggregator`](crates/rmcp-codemode/src/aggregate.rs) (connect upstreams → registry → dispatch), plus the script contract — [`ScriptResult`](crates/rmcp-codemode/src/result.rs) and its model-facing rendering, TypeScript [`signature`](crates/rmcp-codemode/src/signature.rs) rendering, and tool-path [`identity`](crates/rmcp-codemode/src/identity.rs) rules, and pluggable [`NamingConvention`](crates/rmcp-codemode/src/naming.rs)s for host-specific tool spellings |
| [`rmcp-codemode-quickjs`](crates/rmcp-codemode-quickjs) | The QuickJS binding: the per-run VM ([`sandbox`](crates/rmcp-codemode-quickjs/src/sandbox.rs)), the globals [`bridge`](crates/rmcp-codemode-quickjs/src/bridge.rs), and the [`CodemodeServer`](crates/rmcp-codemode-quickjs/src/server.rs) — a drop-in `ServerHandler` exposing exactly one `codemode` tool |

## Transports

The transport layer is rmcp's, so everything rmcp connects to, this crate aggregates:

| Direction | Transports |
|:-:|---|
| **Upstream (client side)** | `TokioChildProcess` (spawn an MCP server binary), Streamable HTTP, or any in-process duplex — anything implementing rmcp's `IntoTransport` |
| **Downstream (server side)** | `stdio` for CLI hosts, Streamable HTTP for networked hosts, or any in-process duplex (tests use this) |

## Usage

Connect upstream servers, optionally mix in host-native tools, and serve everything as one `codemode` tool:

```rust,ignore
use std::sync::Arc;
use rmcp::{ServiceExt, transport::{TokioChildProcess, stdio}};
use rmcp_codemode::aggregate::{Aggregator, UpstreamConfig};
use rmcp_codemode::exposure::ToolExposure;
use rmcp_codemode_quickjs::server::{CodemodeServer, NativeTool};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Connect upstreams; each gets a namespace and an exposure.
    let aggregator = Aggregator::builder()
        .upstream(UpstreamConfig {
            namespace: "github".into(),
            exposure: ToolExposure::CodeModeOnly, // callable from scripts only
            description: Some("GitHub API".into()),
            transport: move || async {
                Ok(().serve(TokioChildProcess::new(
                    tokio::process::Command::new("npx")
                        .args(["-y", "@modelcontextprotocol/server-github"]),
                )?).await?)
            },
        })
        .upstream(UpstreamConfig {
            namespace: "fs".into(),
            exposure: ToolExposure::Direct, // also listed to the model
            description: None,
            transport: move || async {
                Ok(().serve(TokioChildProcess::new(
                    tokio::process::Command::new("npx")
                        .args(["-y", "@modelcontextprotocol/server-filesystem"]),
                )?).await?)
            },
        })
        .connect()
        .await?;

    // 2. Mix in host-native tools (non-MCP: read, edit, bash, ...) — they
    //    become tools.<path> alongside every MCP tool, with the same
    //    discovery and declarations. Default exposure is CodeModeOnly.
    let native = vec![
        NativeTool::new("bash", "Run a shell command", |args| async move {
            run_shell(args["command"].as_str().unwrap_or(""))
        })
        .with_input_schema(json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })),
    ];

    // 3. Serve the aggregated registry as one `codemode` tool.
    let server = CodemodeServer::from_aggregator(Arc::new(aggregator))
        .with_native_tools(native);
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
```

The host (and the model) sees exactly one tool. This is the whole integration for a host like Zed: connect upstream MCP servers, aggregate, serve. Sandboxing, discovery, exposure routing, and result formatting are the crate's job.

```json
{"name": "codemode", "inputSchema": {"properties": {"script": {"type": "string"}}}}
```

A model turn looks like this — two upstream calls pipe together, and only the final line reaches the model:

```js
// script
const cities = await tools.weather.list_cities({});
const readings = await Promise.all(
    cities.map((city) => tools.weather.get_temperature({ city }))
);
const coldest = readings.reduce((a, b) => (b.celsius < a.celsius ? b : a));
text(`coldest: ${coldest.city} at ${coldest.celsius}C`);
return { coldest: coldest.city };
```

## The script environment

The script is the body of an async function: top-level `await` and `return` work.

| Global | Purpose |
|---|---|
| `tools.<namespace>.<tool>(args)` | Every registered tool — upstream MCP and host-native alike — as namespaced promise-returning functions. Unknown paths fail with close-match suggestions (`websearch` → did you mean `web-search`?). |
| `text(value)` | Append a text item to the output. |
| `image(dataUri)` | Append an image (base64 data URI) to the output; it also becomes an MCP image content block. |
| `console.log/info/warn/error/debug` | Console lines, returned in one `<console_output>` block. |
| `ALL_TOOLS` | `{ name, description }` for every callable tool, including ones the description does not list. |
| `searchTools(query, { limit?, namespace? })` | BM25-ranked discovery over names and descriptions (default limit 8; empty query lists in path order). |
| `describeTool(name)` | The tool's description and TypeScript declaration, rendered from its JSON Schema. |
| `describeNamespace(name)` | A namespace's description and its tools' declarations. |
| `store(key, value)` / `load(key)` | Small JSON values kept across codemode calls; writes commit only when the script succeeds. Back it with your own session persistence via `with_store_persistence`. |
| `exit()` | End the script successfully — even when a try/catch swallows it. |
| `return value` | A top-level `return` adds the value like `text()`. |
| `// @options: {...}` | Optional first line: `{"max_output_tokens": ..., "timeout_ms": ...}`. |

Nothing else: no timers, `fetch`, `require`, `process`, or file system — scripts reach the outside world only through tool calls. The VM has a heap limit (256 MB default); allocations past it throw `InternalError: out of memory`.

### Runtime semantics (pi's model)

- Each script runs in a **fresh QuickJS VM**: a runaway or failed script cannot poison the next run, and unawaited tool calls are cancelled when the script ends.
- A script waiting on a promise nothing can settle fails immediately (there are no timers).
- Pure-JS busy loops are preempted at the deadline by the VM's interrupt handler.
- Scripts beyond the worker limit (2 by default; `with_max_workers`) queue in arrival order and fail without running if no worker frees up in time.
- Over-budget output keeps its start and end; the full text is written to a file whose path is included in the result.
- The result follows pi's format: `Script completed`/`Script failed` with wall time, `==> text N/M <==` markers, one `<console_output>` block, and `Script error:` on failure.

## Exposure model

| Exposure | In `tools/list` | Callable by host | Callable in scripts | Discovery |
|---|---|---|---|---|
| `Direct` | yes | yes | yes | — |
| `Deferred` | no | yes (guidance error) | yes | via `SearchIndex` |
| `CodeModeOnly` | no | no | yes | via `ALL_TOOLS` / `SearchIndex` |

How the states present at the server boundary (the mixing pi and OpenCode both ship):

- `Direct` tools appear in the host's `tools/list` (`CodemodeMode::On`) **and** are declared inline in the `codemode` description — TypeScript, grouped by namespace, within a 3000-token budget (pi's `codemode.inlineBudget`); declarations that do not fit are found via `searchTools`.
- `Deferred` and `CodeModeOnly` tools never appear in `tools/list`; calling them natively answers with an instructive error directing the model to a script (the self-correction shape from Zed discussion [#64004](https://github.com/zed-industries/zed/discussions/64004)).
- Inside the sandbox every tool is callable regardless of exposure.
- `CodemodeMode::Only` (pi's `codemode.mode: "only"`, OpenCode 2.0's default) hides direct tools behind the codemode tool entirely.

Per-tool overrides after connecting:

```rust,ignore
aggregator.registry_mut().override_exposure("github.get_me", ToolExposure::Direct);
```

## Naming conventions

Different hosts and model vendors spell tool paths differently. The sandbox tree is *built from* registry paths, so any spelling works end to end — a flat path is a single-segment leaf, a dotted path nests. Pick per upstream via `UpstreamConfig::naming`:

| Convention | Path for (`github`, `create_issue`) | Sandbox shape | Matches |
|---|---|---|---|
| `Namespaced` *(default)* | `github.create_issue` | nested; `Object.keys(tools.github)` enumerates | OpenCode |
| `McpPrefixed` | `mcp__github__create_issue` | flat, one segment | pi, Claude Code |
| `Flat { separator }` | `github__create_issue` (with `__`) | flat, no prefix | Cloudflare-style flattening |
| `Custom(mapping)` | whatever the closure returns | follows the path's shape | anything else |

The sandbox behaves identically in all cases — `ALL_TOOLS`, `searchTools`, close-match suggestions, and routing (the upstream always sees its own tool name) are spelling-agnostic. What differs: only `Namespaced` keeps namespace enumeration, namespace-scoped search, and per-namespace inline declarations, which is why it is the default.

## Development

The workspace mirrors the [rust-sdk](https://github.com/modelcontextprotocol/rust-sdk) layout and policies: resolver 3, edition 2024, workspace lints, nightly rustfmt, `just` recipes, and a [`DEPENDENCY_POLICY.md`](DEPENDENCY_POLICY.md).

```
just fmt      # nightly rustfmt
just check    # clippy, deny warnings
just test     # full suite (76 tests)
```

### Test provenance

The conformance suites port scenarios from two references:

- **pi `sandbox.test.ts`:** JSON round trips, parallel `Promise.all` fan-out, tool failures as catchable JS errors, partial output kept on failure, the no-timers/no-fetch/no-process guarantee, deadline and memory limits, `ALL_TOOLS`, normalized identifiers, close-match suggestions, and the host-shaped `tools` object.
- **OpenCode `tool-paths.test.ts`:** namespace enumeration (`Object.keys(tools.api)` lists child segments), non-callable namespaces, and blocked member names (`tools.issues.constructor`, `tools.prototype` resolve as real paths; `__proto__` poisoning stays inert).

Deliberately not ported: OpenCode's `test262`/`parity`/`stdlib` suites (~500KB) test the ECMAScript conformance of their custom interpreter — QuickJS-NG ships its own conformance, which is why this crate embeds it. pi's worker/`close()` lifecycle and session-snapshot machinery are host-level concerns this crate exposes as extension points instead.

## Status

PoC, tracking [rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437). Not on crates.io yet.

Known gaps:

- One store per server (pi scopes stores per session/branch — a host with sessions would key persisters itself); `_meta` plumbing.
- The aggregator reconnects upstreams only by rebuilding.
- Native-tool executor errors do not carry typed codes unless the tool encodes them in its return value.
- Syntax-error messages carry no line numbers (QuickJS reports none; pi's Node runtime does).

### Benchmarks (vs. pi / OpenCode)

Planned: a shared scenario set (N sequential tool calls, M parallel tool calls, script startup, search over K tools) run against pi's Node sandbox for a comparison table. Structural advantages on the tool-call-heavy workload that dominates codemode wall time:

- No WASM compile step at startup (pi compiles QuickJS-wasm per process); VM creation is native and sub-millisecond.
- No worker-thread hop per tool call; tool calls resolve as native tokio futures, so `Promise.all` maps to concurrent tasks with no thread boundary.
- Raw JS compute loses to V8/JSC — model-written scripts are orchestration, not benchmarking.

## License

This project is licensed under the terms specified in the repository's [LICENSE](LICENSE) file.
