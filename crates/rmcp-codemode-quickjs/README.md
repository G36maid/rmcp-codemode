<style>
.rustdoc-hidden { display: none; }
</style>

<div class="rustdoc-hidden">

# rmcp-codemode-quickjs

[![Crates.io](https://img.shields.io/crates/v/rmcp-codemode-quickjs.svg)](https://crates.io/crates/rmcp-codemode-quickjs)
[![Documentation](https://docs.rs/rmcp-codemode-quickjs/badge.svg)](https://docs.rs/rmcp-codemode-quickjs)

</div>

The QuickJS backend for [rmcp-codemode](https://github.com/G36maid/rmcp-codemode): run model-written JavaScript in a sandboxed VM where the only capability is calling registered tools, and serve the whole thing as one `codemode` MCP tool.

- **[`Sandbox`](src/sandbox.rs)** — one fresh QuickJS VM per script run (pi's model): heap limit, deadline preemption via the interrupt handler, immediate failure on a promise nothing can settle, unawaited calls cancelled when the script ends.
- **[`CodemodeServer`](src/server.rs)** — a drop-in rmcp `ServerHandler` exposing exactly one `codemode` tool: exposure mixing, the worker pool, store persistence, TypeScript inline declarations.
- **[`NativeTool`](src/server.rs)** — host-native (non-MCP) tools enter the sandbox with the same standing as MCP tools.

```rust,ignore
use rmcp::ServiceExt;
use rmcp_codemode_quickjs::server::CodemodeServer;

let server = CodemodeServer::new(tools).with_timeout(std::time::Duration::from_secs(60));
let running = server.serve(rmcp::transport::stdio()).await?;
running.waiting().await?;
```

For the script environment (globals, discovery, `store`/`load`), the exposure model, and the full usage example, see the [main README](../../README.md).

## License

This project is licensed under the terms specified in the repository's [LICENSE](../../LICENSE) file.
