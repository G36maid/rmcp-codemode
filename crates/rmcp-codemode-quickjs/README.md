<style>
.rustdoc-hidden { display: none; }
</style>

<div class="rustdoc-hidden">

# rmcp-codemode-quickjs

[![Crates.io](https://img.shields.io/crates/v/rmcp-codemode-quickjs.svg)](https://crates.io/crates/rmcp-codemode-quickjs)
[![Documentation](https://docs.rs/rmcp-codemode-quickjs/badge.svg)](https://docs.rs/rmcp-codemode-quickjs)

</div>

The QuickJS engine for [rmcp-codemode](https://github.com/G36maid/rmcp-codemode): run model-written JavaScript in a sandboxed VM where the only capability is calling registered tools, and serve the whole thing as one `codemode` MCP tool.

- **[`QuickJsEngine`](src/engine.rs)** — one fresh QuickJS VM per script run (pi's model): heap limit, deadline preemption via the interrupt handler, immediate failure on a promise nothing can settle, unawaited calls cancelled when the script ends.
- **[`SuspendingQuickJsEngine`](src/engine.rs)** — the same engine plus host-controlled runs: every tool call yields as a `HostCall`, the host delivers an `Outcome` (allow/deny), and a `SuspendState` resumes by replaying recorded calls (pi's `start()`/`deliver()`, Cloudflare's pause/approve flow).
- **[`CodemodeServer`](src/server.rs)** — a drop-in rmcp `ServerHandler` exposing exactly one `codemode` tool: exposure mixing, native tools, the approval round trip, and store persistence.

```rust,ignore
use rmcp::ServiceExt;
use rmcp_codemode::Space;
use rmcp_codemode_quickjs::{CodemodeServer, QuickJsEngine};

let server = CodemodeServer::new(QuickJsEngine::default(), Space::new());
let running = server.serve(rmcp::transport::stdio()).await?;
running.waiting().await?;
```

For the script environment (globals, discovery, `store`/`load`), the exposure model, and the approval flow, see the [main README](../../README.md).

## License

This project is licensed under the terms specified in the repository's [LICENSE](../../LICENSE) file.
