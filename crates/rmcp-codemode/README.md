<style>
.rustdoc-hidden { display: none; }
</style>

<div class="rustdoc-hidden">

# rmcp-codemode

[![Crates.io](https://img.shields.io/crates/v/rmcp-codemode.svg)](https://crates.io/crates/rmcp-codemode)
[![Documentation](https://docs.rs/rmcp-codemode/badge.svg)](https://docs.rs/rmcp-codemode)

</div>

The engine-agnostic core of rmcp-codemode: client-side MCP aggregation plus the script-mode contract, with no JavaScript engine dependency.

| Piece | Purpose |
|---------|-------------|
| [`ToolExposure`](src/exposure.rs) | Per-tool exposure metadata: `Direct` (in `tools/list`), `Deferred` (discoverable via search), `CodeModeOnly` (script-only). Naming mirrors Codex's enum. |
| [`Registry`](src/registry.rs) | Every upstream tool under a namespaced path, with per-tool exposure and namespace descriptions. |
| [`Aggregator`](src/aggregate.rs) | Connect upstream MCP servers (any rmcp transport) → aggregated registry → routed dispatch. |
| [`SearchIndex`](src/search.rs) | BM25 (Okapi) ranking over tool names and descriptions; in-tree, no dependencies. |
| [`ScriptResult`](src/result.rs) + rendering | The script contract and pi's model-facing result format. |
| [`ts_declaration`](src/signature.rs) | JSON Schema → TypeScript declaration rendering. |
| [`identity`](src/identity.rs) | Tool-path → identifier rules and unknown-name suggestions. |

This crate says nothing about *how* scripts execute — pair it with a backend such as [`rmcp-codemode-quickjs`](../rmcp-codemode-quickjs).

For **why code mode**, **usage examples**, and the full story, see the [main README](../../README.md).

## License

This project is licensed under the terms specified in the repository's [LICENSE](../../LICENSE) file.
