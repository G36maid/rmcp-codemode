# rmcp-codemode

Code mode for [RMCP](https://github.com/modelcontextprotocol/rust-sdk): expose upstream MCP servers' tools to any host as a **single `codemode` script tool**. The model writes a short JavaScript program; upstream tools run inside a QuickJS sandbox and their intermediate results never round-trip through the model.

This crate is the third-party reference implementation proposed in [modelcontextprotocol/rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437) — a client-side aggregator plus the two SDK-level primitives the issue asks for: a client-side aggregation interface and per-server exposure metadata (`direct` / `deferred` / `codemode`). It stays outside the SDK's workspace on purpose: QuickJS cannot meet the SDK's dependency policy, and exposure logic belongs to hosts. The crate is the input, not the ask.

Semantics follow pi's shipped `codemode` tool ([docs](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/codemode.md)), with OpenCode's [`packages/codemode`](https://github.com/anomalyco/opencode/blob/dev/packages/codemode) as the second reference for the aggregation side and Cloudflare's [Code Mode](https://blog.cloudflare.com/code-mode/) as the architecture sketch.

## Why

- **Schemas are re-sent every turn.** Anthropic measured ~77K tokens before the first user message with 50+ MCP tools, dropping to ~8.7K with deferred loading ([Advanced tool use](https://www.anthropic.com/research/advanced-tool-use)). Servers with big surfaces (e.g. [ghidra-mcp](https://github.com/bethington/ghidra-mcp), 209 tools) implement their own lazy loading because hosts don't provide it.
- **Tool results always round-trip through the model**, even when they are just input for the next tool call. Anthropic measured 150K → 2K tokens for exactly this pattern ([Code execution with MCP](https://www.anthropic.com/research/code-execution-with-mcp)).
- **Server authors can't declare intent.** Every host invents its own policy; nothing in the SDK carries it. Codex reserves the same three states (`ToolExposure::Direct | Deferred | CodeModeOnly`, [`codex-rs/tools`](https://github.com/openai/codex/blob/main/codex-rs/tools/src/tool_executor.rs)) and Anthropic standardizes deferred discovery ([Tool Search Tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool)).

Code mode solves both problems in one mechanism: tool schemas stay out of the context (tools are functions, deferred by default), and intermediate results stay inside the script.

## Crates

| Crate | Purpose | Feature |
|---|---|---|
| `rmcp-codemode` | Core: [`ToolExposure`](crates/rmcp-codemode/src/exposure.rs), aggregated [`Registry`](crates/rmcp-codemode/src/registry.rs), BM25 [`SearchIndex`](crates/rmcp-codemode/src/search.rs), the [`Aggregator`](crates/rmcp-codemode/src/aggregate.rs) (connect upstreams → registry → dispatch) | — |
| `rmcp-codemode-quickjs` | The QuickJS sandbox backend and the [`CodemodeServer`](crates/rmcp-codemode-quickjs/src/lib.rs): a drop-in `ServerHandler` exposing exactly one `codemode` tool | `codemode` backend |

## Usage

Connect upstream servers, aggregate their tools, and serve them as one `codemode` tool over any rmcp transport:

```rust,ignore
use std::sync::Arc;
use rmcp::ServiceExt;
use rmcp::transport::TokioChildProcess;
use rmcp_codemode::aggregate::{Aggregator, UpstreamConfig};
use rmcp_codemode::exposure::ToolExposure;
use rmcp_codemode_quickjs::server::CodemodeServer;
use rmcp::transport::stdio;

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
                    tokio::process::Command::new("npx").args(["-y", "@modelcontextprotocol/server-github"]),
                )?).await?)
            },
        })
        .upstream(UpstreamConfig {
            namespace: "fs".into(),
            exposure: ToolExposure::Direct, // also listed to the model
            description: None,
            transport: move || async {
                Ok(().serve(TokioChildProcess::new(
                    tokio::process::Command::new("npx").args(["-y", "@modelcontextprotocol/server-filesystem"]),
                )?).await?)
            },
        })
        .connect()
        .await?;

    // 2. Serve the aggregated registry as one `codemode` tool.
    let server = CodemodeServer::from_aggregator(Arc::new(aggregator));
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
const cities = await tools.weather.list_cities({});
const readings = await Promise.all(
    cities.map((city) => tools.weather.get_temperature({ city }))
);
const coldest = readings.reduce((a, b) => (b.celsius < a.celsius ? b : a));
text(`coldest: ${coldest.city} at ${coldest.celsius}C`);
return { coldest: coldest.city };
```

### The script environment

- The script is the body of an async function: top-level `await` and `return` work.
- `tools.<namespace>.<tool>(args)` — every registered upstream tool, as namespaced promise-returning functions. Arguments and results make a JSON round trip. Unknown paths fail with an `Unknown tool` message plus registry suggestions.
- `text(value)` — append a line to the result the model sees.
- `console.log/info/warn/error/debug` — append `[console]` lines.
- `ALL_TOOLS` — `{ name, description }` for every callable tool.
- Nothing else: no timers, `fetch`, `require`, `process`, or file system. Scripts reach the outside world only through tool calls. The VM has a heap limit (256 MB default); allocations past it throw `InternalError: out of memory`.
- Only the script's return value, `text()` output, and `console` lines reach the model. Tool calls before a failure are real and not undone.

Each script runs in a fresh QuickJS VM (pi's model): a runaway or failed script cannot poison the next run.

## Exposure model

| Exposure | In `tools/list` | Callable by host | Callable in scripts | Discovery |
|---|---|---|---|---|
| `Direct` | yes | yes | yes | — |
| `Deferred` | no | yes | yes | via `SearchIndex` |
| `CodeModeOnly` | no | no (host dispatch refuses) | yes | via `ALL_TOOLS` / `SearchIndex` |

Per-tool overrides after connecting:

```rust,ignore
aggregator.registry_mut().override_exposure("github.get_me", ToolExposure::Direct);
```

## Development

The workspace mirrors the [rust-sdk](https://github.com/modelcontextprotocol/rust-sdk) layout and policies (see [`DEPENDENCY_POLICY.md`](DEPENDENCY_POLICY.md)); QuickJS is the dependency the SDK's policy rules out, which is why this crate exists outside it.

```
just fmt      # nightly rustfmt
just check    # clippy, deny warnings
just test     # full suite
```

### Test provenance

The sandbox conformance suite (`crates/rmcp-codemode-quickjs/tests/sandbox.rs`) ports scenarios from two sources:

**pi `packages/codemode` `sandbox.test.ts`:** JSON round trips, parallel `Promise.all` fan-out, tool failures as catchable JS errors, partial output kept on failure, the no-timers/no-fetch/no-process guarantee, deadline and memory limits, `ALL_TOOLS`, normalized identifiers (`tools.my_tool` reaches `my-tool`), unknown-tool close-match suggestions (`websearch` → did you mean `web-search`), and the host-shaped `tools` object (`"echo" in tools`, `JSON.stringify(tools) === "{}"`).

**OpenCode `packages/codemode` `tool-paths.test.ts`:** namespace enumeration (`Object.keys(tools.api)` lists child segments), non-callable namespaces, and blocked member names (`tools.issues.constructor`, `tools.prototype` resolve as real paths; `__proto__` poisoning and `Function.constructor` escapes stay inert).

**Deliberately not ported:** OpenCode's `*test262*`/`parity`/`stdlib`/`promise`/`generator` suites (~500KB) test the ECMAScript conformance of their custom interpreter — QuickJS-NG ships its own conformance, which is why this crate embeds it. OpenCode's `limits.test.ts` caps (`MAX_STRING_LENGTH`, pending-promise budgets) are properties of their interpreter, not portable. pi's worker/`close()`/abort-signal lifecycle, `store`/`load` snapshots, and image output test host machinery this crate does not implement yet. pi's syntax-error line-number reporting is not portable as-is: QuickJS error messages carry no source location (a separate compile pass would be needed).

## Benchmarks (vs. pi / OpenCode)

Planned: a shared scenario set (N sequential tool calls, M parallel tool calls, script startup, search over K tools) run against pi's Node sandbox for a comparison table. Structural advantages on the tool-call-heavy workload that dominates codemode wall time:

- No WASM compile step at startup (pi compiles QuickJS-wasm per process); VM creation is native and sub-millisecond.
- No worker-thread hop per tool call; tool calls resolve as native tokio futures, so `Promise.all` maps to concurrent tasks with no thread boundary.
- Raw JS compute loses to V8/JSC — model-written scripts are orchestration, not benchmarking.

## Status

PoC, tracking [rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437). Not on crates.io yet.

The sandbox tracks pi's `codemode` semantics one-to-one where the host machinery allows: `searchTools(query, { limit?, namespace? })` (BM25 over names and descriptions, default limit 8, empty query lists in path order), `describeTool(name)` and `describeNamespace(name)` with TypeScript declarations rendered from the tools' JSON Schemas, `exit()`, the `// @options: {"max_output_tokens", "timeout_ms"}` first line, pi's output caps (16 MiB of text, 100k text/console calls) and result format (`Script completed`/`Script failed` with wall time, `==> text N/M <==` markers, `<console_output>` block, `Script error:`), and immediate failure for a script waiting on a promise nothing can settle. Pure-JS busy loops are preempted by the VM's interrupt handler at the deadline.

Known gaps:
- No persistence (`store`/`load`), image output, or `_meta` exposure plumbing (pi's store writes are session-backed; ours would need the same host machinery).
- Namespace descriptions/instructions (`describeNamespace`'s optional fields) are not tracked yet.
- No per-tool exposure filtering at the boundary yet: every registered tool enters the sandbox and none are listed in `tools/list` (the `ToolExposure` states exist in the core registry but the codemode server does not mix direct + codemode tools yet).
- The aggregator reconnects upstreams only by rebuilding.

## License

Apache-2.0
