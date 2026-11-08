<style>
.rustdoc-hidden { display: none; }
</style>

<div class="rustdoc-hidden">

# rmcp-codemode

[![Crates.io](https://img.shields.io/crates/v/rmcp-codemode.svg)](https://crates.io/crates/rmcp-codemode)
[![Documentation](https://docs.rs/rmcp-codemode/badge.svg)](https://docs.rs/rmcp-codemode)

</div>

The engine-agnostic core of rmcp-codemode: the tool space, the script contract, the catalog, and the engine contracts — no JavaScript engine dependency.

| Piece | Purpose |
|---------|-------------|
| [`Space`](src/space.rs) | Every tool under path segments (a flat spelling is one segment — pi's model is a projection), with namespace metadata (description, instructions, aliases — pi's `ToolNamespace`), exposure states, and approval annotations (Cloudflare's). |
| [`Catalog`](src/catalog.rs) | The model-facing view: inline listings under pi's round-robin token budget, OpenCode's paged search (`{ items, remaining, next }`), and pi's `describe` shapes. |
| [`Budget`](src/budget.rs) | pi's limits plus OpenCode's `maxToolCalls` in one type; the script's `@options` line overrides exactly two fields. |
| [`Engine`](src/engine.rs) | The contract every script backend honors (fresh VM, never-settling promise fails immediately, unawaited calls cancelled, deadline preemption) plus `SuspendingEngine` — the host-controlled seam (`HostCall`/`deliver`) that approval, pause, and replay build on. |
| [`ScriptResult`](src/result.rs) | The script contract and pi's model-facing result format. |
| [`ts_declaration`](src/signature.rs) | JSON Schema → TypeScript declaration rendering. |

This crate says nothing about *how* scripts execute — pair it with a backend such as [`rmcp-codemode-quickjs`](../rmcp-codemode-quickjs).

For **why code mode**, **usage examples**, and the full story, see the [main README](../../README.md).

## License

This project is licensed under the terms specified in the repository's [LICENSE](../../LICENSE) file.
